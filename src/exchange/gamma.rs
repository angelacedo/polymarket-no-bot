use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;

use super::retry::retry_get as shared_retry_get;
use crate::config::ExchangeConfig;
use crate::types::MarketMeta;

pub struct GammaClient {
    client: Client,
    base_url: String,
}

impl GammaClient {
    pub fn new(config: &ExchangeConfig) -> Self {
        Self {
            client: Client::new(),
            base_url: config.gamma_base_url.clone(),
        }
    }

    pub async fn fetch_active_markets(&self, limit: u32) -> Result<Vec<MarketMeta>> {
        let url = format!(
            "{}/markets?active=true&closed=false&limit={limit}",
            self.base_url
        );
        let resp = retry_get(&self.client, &url)
            .await
            .context("gamma markets request")?;

        if !resp.status().is_success() {
            anyhow::bail!("gamma api returned {}", resp.status());
        }

        let markets: Vec<GammaMarket> = resp.json().await.context("gamma markets json")?;
        let raw_count = markets.len();
        let parsed: Vec<MarketMeta> = markets
            .into_iter()
            .filter_map(|m| self.to_market_meta(m))
            .collect();
        tracing::debug!(
            raw = raw_count,
            parsed = parsed.len(),
            "gamma markets parsed"
        );
        Ok(parsed)
    }

    fn to_market_meta(&self, m: GammaMarket) -> Option<MarketMeta> {
        if !m.enable_order_book.unwrap_or(false) {
            return None;
        }
        let tokens = m.clob_token_ids.as_ref()?;
        if tokens.len() < 2 {
            return None;
        }
        let end = m
            .end_date
            .as_deref()
            .and_then(parse_gamma_date)
            .unwrap_or_else(|| Utc::now() + chrono::Duration::days(30));

        let question = m.question.as_deref().unwrap_or("");
        let (category, underlying) = classify_market(question, m.tags.as_deref().unwrap_or(&[]));

        Some(MarketMeta {
            condition_id: m.condition_id.unwrap_or_else(|| m.id.clone()),
            question: m.question.unwrap_or_default(),
            yes_token_id: tokens[0].clone(),
            no_token_id: tokens[1].clone(),
            category,
            underlying,
            end_date: end,
            enable_order_book: true,
            liquidity_usd: m.liquidity_num.unwrap_or(0.0),
        })
    }
}

async fn retry_get(client: &Client, url: &str) -> Result<reqwest::Response> {
    shared_retry_get(client, url).await
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    id: String,
    question: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    condition_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_clob_token_ids")]
    clob_token_ids: Option<Vec<String>>,
    #[serde(default)]
    enable_order_book: Option<bool>,
    #[serde(default)]
    end_date: Option<String>,
    #[serde(default)]
    liquidity_num: Option<f64>,
}

/// Gamma returns `clobTokenIds` as a JSON-encoded string, not a native array.
fn deserialize_clob_token_ids<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;

    let value: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    match value {
        None => Ok(None),
        Some(serde_json::Value::Array(items)) => Ok(Some(
            items
                .into_iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
        )),
        Some(serde_json::Value::String(raw)) => {
            serde_json::from_str(&raw).map(Some).map_err(Error::custom)
        }
        Some(_) => Ok(None),
    }
}

fn parse_gamma_date(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(23, 59, 59))
                .map(|dt| dt.and_utc())
        })
}

/// POLYMARKET_INTEGRATION: refine category mapping using Gamma tags
pub fn classify_market(question: &str, tags: &[String]) -> (String, String) {
    let q = question.to_lowercase();
    let tag_str = tags.join(" ").to_lowercase();

    let underlying = if q.contains("btc") || tag_str.contains("bitcoin") {
        "BTC".to_string()
    } else if q.contains("eth") || tag_str.contains("ethereum") {
        "ETH".to_string()
    } else if q.contains("sol") {
        "SOL".to_string()
    } else {
        "OTHER".to_string()
    };

    let category = if tag_str.contains("crypto") || q.contains("bitcoin") || q.contains("btc") || q.contains("ethereum") {
        "crypto".to_string()
    } else if tag_str.contains("politic") || q.contains("election") || q.contains("president") {
        "politics".to_string()
    } else if tag_str.contains("macro") || q.contains("fed") || q.contains("cpi") || q.contains("gdp") {
        "macro".to_string()
    } else if tag_str.contains("sport") {
        "sports".to_string()
    } else {
        "other".to_string()
    };

    (category, underlying)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_crypto() {
        let (cat, und) = classify_market("Will BTC exceed 100k?", &["crypto".into()]);
        assert_eq!(cat, "crypto");
        assert_eq!(und, "BTC");
    }

    #[test]
    fn parses_gamma_market_json() {
        let raw = r#"{
            "id": "540817",
            "question": "Will BTC exceed 100k?",
            "conditionId": "0xabc",
            "clobTokenIds": "[\"111\", \"222\"]",
            "enableOrderBook": true,
            "endDate": "2026-07-31T12:00:00Z",
            "liquidityNum": 17572.66
        }"#;
        let market: GammaMarket = serde_json::from_str(raw).unwrap();
        assert_eq!(market.condition_id.as_deref(), Some("0xabc"));
        assert_eq!(
            market.clob_token_ids,
            Some(vec!["111".into(), "222".into()])
        );
        assert_eq!(market.enable_order_book, Some(true));

        let client = GammaClient::new(&ExchangeConfig {
            gamma_base_url: "https://gamma-api.polymarket.com".into(),
            data_api_base_url: String::new(),
            clob_host: String::new(),
            chain_id: 137,
            market_discovery_limit: 200,
        });
        let meta = client.to_market_meta(market).expect("valid market");
        assert_eq!(meta.yes_token_id, "111");
        assert_eq!(meta.no_token_id, "222");
        assert_eq!(meta.category, "crypto");
    }
}
