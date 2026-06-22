use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use hmac::{Hmac, Mac};
use alloy::signers::local::PrivateKeySigner;
use polymarket_client_sdk_v2::auth::{Credentials, Signer};
use polymarket_client_sdk_v2::clob::types::request::BalanceAllowanceRequest;
use polymarket_client_sdk_v2::clob::types::{OrderType, Side as ClobSide};
use polymarket_client_sdk_v2::clob::{Client, Config};
use polymarket_client_sdk_v2::types::{Address, Decimal, U256};
use polymarket_client_sdk_v2::POLYGON;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client as HttpClient, RequestBuilder, Response};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sha2::Sha256;
use tokio::sync::OnceCell;
use tracing::{info, warn};

use crate::config::ExchangeConfig;
use crate::exchange::retry::{retry_delete_with, retry_get_with, retry_post_with};
use crate::storage::Storage;
use crate::types::{
    Balances, ExecutionMode, OrderRequest, OrderResult, OrderStatus, Position, Side,
};

type AuthClobClient = Client<polymarket_client_sdk_v2::auth::state::Authenticated<polymarket_client_sdk_v2::auth::Normal>>;

/// Live execution backend using Polymarket CLOB SDK + authenticated HTTP.
pub struct LiveBackend {
    storage: Storage,
    exchange: ExchangeConfig,
    http: HttpClient,
    wallet: Address,
    credentials: Option<Credentials>,
    private_key: Option<String>,
    sdk_client: OnceCell<Arc<AuthClobClient>>,
    authenticated: bool,
}

impl LiveBackend {
    pub fn new(storage: Storage, exchange: ExchangeConfig) -> Result<Self> {
        let private_key = std::env::var("POLYMARKET_PRIVATE_KEY").ok();
        let api_key = std::env::var("POLYMARKET_API_KEY").ok();
        let api_secret = std::env::var("POLYMARKET_API_SECRET").ok();
        let api_passphrase = std::env::var("POLYMARKET_API_PASSPHRASE").ok();

        let signer = private_key
            .as_ref()
            .map(|pk| PrivateKeySigner::from_str(pk).map(|s| s.with_chain_id(Some(exchange.chain_id))))
            .transpose()
            .context("invalid POLYMARKET_PRIVATE_KEY")?;

        let wallet = signer.as_ref().map(|s| s.address()).unwrap_or_default();

        let credentials = match (api_key, api_secret, api_passphrase) {
            (Some(key), Some(secret), Some(passphrase)) => Some(Credentials::new(
                key.parse().context("invalid POLYMARKET_API_KEY")?,
                secret,
                passphrase,
            )),
            _ => None,
        };

        let authenticated = private_key.is_some() && credentials.is_some();

        Ok(Self {
            storage,
            exchange,
            http: HttpClient::new(),
            wallet,
            credentials,
            private_key,
            sdk_client: OnceCell::new(),
            authenticated,
        })
    }

    fn ensure_auth(&self) -> Result<()> {
        if !self.authenticated {
            bail!(
                "LIVE mode requires POLYMARKET_PRIVATE_KEY, POLYMARKET_API_KEY, POLYMARKET_API_SECRET, and POLYMARKET_API_PASSPHRASE"
            );
        }
        Ok(())
    }

    fn signer(&self) -> Result<PrivateKeySigner> {
        let pk = self.private_key.as_ref().context("missing private key")?;
        Ok(PrivateKeySigner::from_str(pk)?.with_chain_id(Some(self.exchange.chain_id)))
    }

    async fn sdk_client(&self) -> Result<Arc<AuthClobClient>> {
        self.ensure_auth()?;
        self.sdk_client
            .get_or_try_init(|| async {
                let signer = self.signer()?;
                let credentials = self.credentials.as_ref().context("missing credentials")?;

                let client = Client::new(&self.exchange.clob_host, Config::default())?
                    .authentication_builder(&signer)
                    .credentials(credentials.clone())
                    .authenticate()
                    .await?;

                Ok(Arc::new(client))
            })
            .await
            .cloned()
    }

    fn auth_headers(&self, method: &str, path: &str, body: &str) -> Result<HeaderMap> {
        let credentials = self.credentials.as_ref().context("missing credentials")?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock before UNIX epoch")?
            .as_secs()
            .to_string();

        let message = format!("{timestamp}{method}{path}{body}");
        let signature = l2_hmac(credentials.secret(), &message)?;

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("poly_address"),
            HeaderValue::from_str(&format!("{:#x}", self.wallet))?,
        );
        headers.insert(
            HeaderName::from_static("poly_api_key"),
            HeaderValue::from_str(&credentials.key().to_string())?,
        );
        headers.insert(
            HeaderName::from_static("poly_passphrase"),
            HeaderValue::from_str(credentials.passphrase().expose_secret())?,
        );
        headers.insert(
            HeaderName::from_static("poly_signature"),
            HeaderValue::from_str(&signature)?,
        );
        headers.insert(
            HeaderName::from_static("poly_timestamp"),
            HeaderValue::from_str(&timestamp)?,
        );
        headers.insert(
            HeaderName::from_static("poly_nonce"),
            HeaderValue::from_static(""),
        );
        Ok(headers)
    }

    fn with_auth(&self, builder: RequestBuilder, method: &str, path: &str, body: &str) -> Result<RequestBuilder> {
        let headers = self.auth_headers(method, path, body)?;
        Ok(builder.headers(headers))
    }

    fn clob_url(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.exchange.clob_host.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }
}

fn l2_hmac(secret: &SecretString, message: &str) -> Result<String> {
    let decoded = URL_SAFE
        .decode(secret.expose_secret())
        .context("decode POLYMARKET_API_SECRET")?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&decoded).context("invalid hmac key length")?;
    mac.update(message.as_bytes());
    Ok(URL_SAFE.encode(mac.finalize().into_bytes()))
}

#[async_trait]
impl super::ExecutionBackend for LiveBackend {
    async fn place_order(&self, req: OrderRequest) -> Result<OrderResult> {
        self.ensure_auth()?;
        let start = std::time::Instant::now();

        let client = self.sdk_client().await?;
        let signer = self.signer()?;

        let token_id = U256::from_str(&req.token_id).context("invalid token_id")?;
        let price = Decimal::from_str(&format!("{:.8}", req.limit_price))
            .context("invalid limit price")?;
        let size = Decimal::from_str(&format!("{:.8}", req.size_shares))
            .context("invalid size")?;

        let side = match req.side {
            Side::No | Side::Yes => ClobSide::Buy,
        };

        // LIVE_INTEGRATION_NOTE: FOK used for immediate fill-or-kill semantics per bot spec.
        let order = client
            .limit_order()
            .token_id(token_id)
            .side(side)
            .price(price)
            .size(size)
            .order_type(OrderType::FOK)
            .build()
            .await
            .context("build limit order")?;

        let signed = client.sign(&signer, order).await.context("sign order")?;
        let resp = client.post_order(signed).await.context("post order")?;

        let status = if resp.success {
            if resp.taking_amount > Decimal::ZERO || resp.making_amount > Decimal::ZERO {
                OrderStatus::Filled
            } else {
                OrderStatus::Pending
            }
        } else {
            OrderStatus::Rejected
        };

        let filled_shares = resp.taking_amount.to_string().parse::<f64>().unwrap_or(0.0);
        let avg_fill_price = if filled_shares > 0.0 {
            req.limit_price
        } else {
            0.0
        };

        let result = OrderResult {
            order_id: resp.order_id.clone(),
            client_order_id: req.client_order_id.clone(),
            filled_shares,
            avg_fill_price,
            status,
            latency_ms: start.elapsed().as_millis() as u64,
        };

        if let Some(err) = resp.error_msg {
            warn!(error = %err, order_id = %resp.order_id, "live order response carried error message");
        }

        self.storage.insert_order(&req, &result, 0)?;
        info!(
            order_id = %resp.order_id,
            status = ?status,
            latency_ms = result.latency_ms,
            "live order submitted"
        );

        Ok(result)
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        self.ensure_auth()?;
        let path = format!("/order/{order_id}");
        let url = self.clob_url(&path);

        let resp = retry_delete_with(&self.http, &url, |builder| {
            self.with_auth(builder, "DELETE", &path, "")
                .expect("auth headers")
        })
        .await
        .context("cancel order request")?;

        if !resp.status().is_success() {
            bail!("cancel order failed: {}", resp.status());
        }

        info!(order_id, "live order cancelled");
        Ok(())
    }

    async fn open_positions(&self) -> Result<Vec<Position>> {
        self.ensure_auth()?;
        let path = format!("/positions?owner={:#x}", self.wallet);
        let url = self.clob_url(&path);

        let resp = retry_get_with(&self.http, &url, |builder| {
            self.with_auth(builder, "GET", &path, "")
                .expect("auth headers")
        })
        .await
        .context("fetch positions")?;

        if !resp.status().is_success() {
            bail!("positions request failed: {}", resp.status());
        }

        // LIVE_INTEGRATION_NOTE: verify CLOB /positions response schema against production API.
        let rows: Vec<ClobPositionRow> = resp.json().await.context("parse positions json")?;

        Ok(rows
            .into_iter()
            .map(|row| Position {
                condition_id: row.condition_id.unwrap_or_default(),
                token_id: row.asset_id.unwrap_or_default(),
                side: Side::No,
                size_shares: row.size.unwrap_or(0.0),
                avg_entry_price: row.avg_price.unwrap_or(0.0),
                category: String::new(),
                underlying: String::new(),
                source: String::new(),
                copy_wallet: None,
                mode: ExecutionMode::Live,
            })
            .collect())
    }

    async fn balances(&self) -> Result<Balances> {
        self.ensure_auth()?;
        let client = self.sdk_client().await?;
        let bal = client
            .balance_allowance(BalanceAllowanceRequest::default())
            .await
            .context("balance_allowance")?;

        let usdc_available = bal.balance.to_string().parse::<f64>().unwrap_or(0.0);

        Ok(Balances {
            usdc_available,
            usdc_locked: 0.0,
        })
    }

    fn mode(&self) -> ExecutionMode {
        ExecutionMode::Live
    }
}

#[derive(Debug, Deserialize)]
struct ClobPositionRow {
    #[serde(default, rename = "asset_id")]
    asset_id: Option<String>,
    #[serde(default, rename = "conditionId")]
    condition_id: Option<String>,
    #[serde(default)]
    size: Option<f64>,
    #[serde(default, rename = "avgPrice")]
    avg_price: Option<f64>,
}

pub fn load_signer_address() -> Result<Address> {
    let pk = std::env::var("POLYMARKET_PRIVATE_KEY").context("POLYMARKET_PRIVATE_KEY")?;
    let signer = PrivateKeySigner::from_str(&pk)?.with_chain_id(Some(POLYGON));
    Ok(signer.address())
}

// Keep retry_post_with available for future manual order POST path.
#[allow(dead_code)]
async fn authenticated_post(
    backend: &LiveBackend,
    path: &str,
    body: &str,
) -> Result<Response> {
    let url = backend.clob_url(path);
    retry_post_with(&backend.http, &url, |builder| {
        backend
            .with_auth(
                builder.header("Content-Type", "application/json"),
                "POST",
                path,
                body,
            )
            .expect("auth headers")
    })
    .await
}
