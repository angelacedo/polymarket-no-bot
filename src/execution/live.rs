use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use polymarket_client_sdk_v2::auth::{LocalSigner, Signer};
use polymarket_client_sdk_v2::types::Address;
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::ExchangeConfig;
use crate::storage::Storage;
use crate::types::{
    Balances, ExecutionMode, OrderRequest, OrderResult, OrderStatus, Position, Side,
};

/// Live execution backend using Polymarket CLOB SDK.
pub struct LiveBackend {
    storage: Storage,
    exchange: ExchangeConfig,
    // POLYMARKET_INTEGRATION: authenticated CLOB client stored after startup auth
    authenticated: bool,
}

impl LiveBackend {
    pub fn new(storage: Storage, exchange: ExchangeConfig) -> Result<Self> {
        Ok(Self {
            storage,
            exchange,
            authenticated: std::env::var("POLYMARKET_PRIVATE_KEY").is_ok(),
        })
    }

    fn ensure_auth(&self) -> Result<()> {
        if !self.authenticated {
            bail!("LIVE mode requires POLYMARKET_PRIVATE_KEY and API credentials in environment");
        }
        Ok(())
    }

    // POLYMARKET_INTEGRATION: derive API credentials and build authenticated CLOB client
    #[allow(dead_code)]
    async fn build_clob_client(&self) -> Result<()> {
        let pk = std::env::var("POLYMARKET_PRIVATE_KEY").context("POLYMARKET_PRIVATE_KEY")?;
        let _signer = LocalSigner::from_str(&pk)?.with_chain_id(Some(self.exchange.chain_id));
        info!(host = %self.exchange.clob_host, "CLOB client ready for live trading");
        Ok(())
    }
}

#[async_trait]
impl super::ExecutionBackend for LiveBackend {
    async fn place_order(&self, req: OrderRequest) -> Result<OrderResult> {
        self.ensure_auth()?;
        let start = std::time::Instant::now();

        // POLYMARKET_INTEGRATION: map OrderRequest to SDK order builder, sign, submit
        // Example flow:
        // 1. client.limit_order().token_id(req.token_id).price(req.limit_price)...
        // 2. sign with LocalSigner
        // 3. client.post_order(signed_order)
        warn!(
            token = %req.token_id,
            price = req.limit_price,
            size = req.size_shares,
            "LIVE order placement stub — wire polymarket_client_sdk_v2 order builder here"
        );

        let order_id = Uuid::new_v4().to_string();
        let result = OrderResult {
            order_id: order_id.clone(),
            client_order_id: req.client_order_id.clone(),
            filled_shares: 0.0,
            avg_fill_price: 0.0,
            status: OrderStatus::Pending,
            latency_ms: start.elapsed().as_millis() as u64,
        };

        self.storage.insert_order(&req, &result, 0)?;
        Ok(result)
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        self.ensure_auth()?;
        // POLYMARKET_INTEGRATION: client.cancel_order(order_id)
        warn!(order_id, "LIVE cancel stub");
        Ok(())
    }

    async fn open_positions(&self) -> Result<Vec<Position>> {
        self.ensure_auth()?;
        // POLYMARKET_INTEGRATION: data API or CLOB get_positions for own wallet
        Ok(vec![])
    }

    async fn balances(&self) -> Result<Balances> {
        self.ensure_auth()?;
        // POLYMARKET_INTEGRATION: CLOB balance endpoint
        Ok(Balances {
            usdc_available: 0.0,
            usdc_locked: 0.0,
        })
    }

    fn mode(&self) -> ExecutionMode {
        ExecutionMode::Live
    }
}

pub fn load_signer_address() -> Result<Address> {
    let pk = std::env::var("POLYMARKET_PRIVATE_KEY").context("POLYMARKET_PRIVATE_KEY")?;
    let signer = LocalSigner::from_str(&pk)?;
    Ok(signer.address())
}
