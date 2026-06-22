use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;

use crate::config::ExchangeConfig;
use crate::types::{Side, WalletTradeEvent};

pub struct DataApiClient {
    client: Client,
    base_url: String,
}

impl DataApiClient {
    pub fn new(config: &ExchangeConfig) -> Self {
        Self {
            client: Client::new(),
            base_url: config.data_api_base_url.clone(),
        }
    }

    pub async fn fetch_trades(&self, wallet: &str, limit: u32) -> Result<Vec<WalletTradeEvent>> {
        let url = format!("{}/trades?user={wallet}&limit={limit}", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("data api trades")?;

        if !resp.status().is_success() {
            anyhow::bail!("data api returned {}", resp.status());
        }

        let rows: Vec<DataTrade> = resp.json().await.context("data api trades json")?;
        Ok(rows.into_iter().filter_map(|t| t.into_event(wallet)).collect())
    }
}

#[derive(Debug, Deserialize)]
struct DataTrade {
    #[serde(default)]
    asset: Option<String>,
    #[serde(default)]
    condition_id: Option<String>,
    #[serde(default)]
    side: Option<String>,
    #[serde(default)]
    price: Option<f64>,
    #[serde(default)]
    size: Option<f64>,
    #[serde(default)]
    transaction_hash: Option<String>,
    #[serde(default)]
    timestamp: Option<i64>,
}

impl DataTrade {
    // POLYMARKET_INTEGRATION: verify field names against live Data API schema
    fn into_event(self, wallet: &str) -> Option<WalletTradeEvent> {
        let asset_id = self.asset?;
        let side = match self.side.as_deref()? {
            "BUY" | "buy" => Side::No,
            "SELL" | "sell" => Side::Yes,
            other if other.eq_ignore_ascii_case("no") => Side::No,
            _ => Side::No,
        };
        let price = self.price?;
        let size = self.size.unwrap_or(0.0);
        let ts = self
            .timestamp
            .and_then(|t| DateTime::from_timestamp(t, 0))
            .unwrap_or_else(Utc::now);

        Some(WalletTradeEvent {
            wallet: wallet.to_string(),
            asset_id,
            condition_id: self.condition_id.unwrap_or_default(),
            side,
            price,
            size_usd: size * price,
            tx_hash: self.transaction_hash.unwrap_or_else(|| format!("{ts:?}")),
            timestamp: ts,
        })
    }
}
