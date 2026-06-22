use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Deserialize;

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
        let resp: Vec<GammaMarket> = self
            .client
            .get(&url)
            .send()
            .await
            .context("gamma markets request")?
            .json()
            .await
            .context("gamma markets json")?;

        Ok(resp
            .into_iter()
            .filter_map(|m| self.to_market_meta(m))
            .collect())
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
            .end_date_iso
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
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

#[derive(Debug, Deserialize)]
struct GammaMarket {
    id: String,
    question: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    condition_id: Option<String>,
    #[serde(default)]
    clob_token_ids: Option<Vec<String>>,
    #[serde(default)]
    enable_order_book: Option<bool>,
    #[serde(default)]
    end_date_iso: Option<String>,
    #[serde(default)]
    liquidity_num: Option<f64>,
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

    let category = if tag_str.contains("crypto") || q.contains("bitcoin") || q.contains("ethereum") {
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
}
