use std::collections::HashMap;

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

    /// Check whether any of the given condition IDs have resolved.
    /// Returns a map of condition_id → `true` if NO won, `false` if NO lost.
    /// Only resolved markets with a decisive outcome are included.
    ///
    /// Gamma does not expose a `winnerOutcome` field; resolution is encoded in
    /// `outcomePrices`, which mirrors the order of `outcomes`. A resolved market
    /// has prices like `["0","1"]` (NO won) or `["1","0"]` (YES won).
    pub async fn fetch_market_resolutions(
        &self,
        condition_ids: &[String],
    ) -> Result<HashMap<String, bool>> {
        if condition_ids.is_empty() {
            return Ok(HashMap::new());
        }
        // Batch by chunks of 20 to stay within reasonable URL lengths.
        let mut results = HashMap::new();
        for chunk in condition_ids.chunks(20) {
            let ids_param = chunk.join(",");
            let url = format!(
                "{}/markets?conditionIds={}&closed=true",
                self.base_url, ids_param
            );
            let resp = match retry_get(&self.client, &url).await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    tracing::warn!(status = %r.status(), "resolution check returned non-200");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "resolution check request failed");
                    continue;
                }
            };
            let markets: Vec<GammaMarket> = match resp.json().await {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %e, "resolution check json parse failed");
                    continue;
                }
            };
            for m in markets {
                if !m.closed.unwrap_or(false) {
                    continue;
                }
                let cid = m.condition_id.clone().unwrap_or_else(|| m.id.clone());
                if let Some(no_won) = resolved_no_outcome(&m) {
                    results.insert(cid, no_won);
                }
            }
        }
        Ok(results)
    }

    pub(crate) fn to_market_meta(&self, m: GammaMarket) -> Option<MarketMeta> {
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

        // Prefer the parent event slug for the public URL — the per-market slug
        // carries a numeric suffix and 404s on polymarket.com. Fall back to the
        // market slug only if no event slug is available.
        let slug = m
            .events
            .as_ref()
            .and_then(|evs| evs.first())
            .and_then(|ev| ev.slug.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| m.slug.clone())
            .unwrap_or_default();

        // Validate YES/NO token order from the `outcomes` field.
        // Gamma guarantees outcomes[i] matches clobTokenIds[i].
        // If outcomes[0] starts with "No", the token order is inverted.
        let (yes_token_id, no_token_id) = if m
            .outcomes
            .as_deref()
            .and_then(|o| o.first())
            .map(|s| s.to_ascii_lowercase().starts_with("no"))
            .unwrap_or(false)
        {
            // Swapped: tokens[0] = NO, tokens[1] = YES
            tracing::debug!(
                question,
                "swapping YES/NO token order based on outcomes field"
            );
            (tokens[1].clone(), tokens[0].clone())
        } else {
            // Normal: tokens[0] = YES, tokens[1] = NO
            (tokens[0].clone(), tokens[1].clone())
        };

        Some(MarketMeta {
            condition_id: m.condition_id.unwrap_or_else(|| m.id.clone()),
            question: m.question.unwrap_or_default(),
            slug,
            yes_token_id,
            no_token_id,
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

/// Determine the NO-side resolution from a closed market's `outcomes` and
/// `outcomePrices`. Returns:
/// - `Some(true)`  → NO won (NO price ≈ 1.0)
/// - `Some(false)` → NO lost (NO price ≈ 0.0)
/// - `None`        → indecisive / voided (e.g. prices `["0","0"]`) or missing data
fn resolved_no_outcome(m: &GammaMarket) -> Option<bool> {
    let outcomes = m.outcomes.as_ref()?;
    let prices = m.outcome_prices.as_ref()?;
    if outcomes.len() != prices.len() || outcomes.is_empty() {
        return None;
    }
    let no_idx = outcomes
        .iter()
        .position(|o| o.eq_ignore_ascii_case("no"))?;
    let parsed: Vec<f64> = prices.iter().map(|p| p.parse().unwrap_or(0.0)).collect();

    // A decisive resolution has exactly one winning outcome priced ≈ 1.0.
    // Voided/invalid markets show e.g. ["0","0"] and must be skipped.
    let has_decisive_winner = parsed.iter().any(|&p| p >= 0.99);
    if !has_decisive_winner {
        return None;
    }
    Some(parsed[no_idx] >= 0.99)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GammaMarket {
    id: String,
    question: Option<String>,
    /// Per-market slug. NOTE: for grouped markets this carries a numeric suffix
    /// (e.g. `...-739`) and is NOT a valid public URL — the event slug is.
    #[serde(default)]
    slug: Option<String>,
    /// Parent event(s); `events[0].slug` is the canonical public URL slug.
    #[serde(default)]
    events: Option<Vec<GammaEvent>>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    condition_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_clob_token_ids")]
    clob_token_ids: Option<Vec<String>>,
    /// `outcomes` mirrors the order of `clobTokenIds` — used to detect when
    /// the API returns them in the unexpected [No, Yes] order.
    #[serde(default, deserialize_with = "deserialize_outcomes")]
    outcomes: Option<Vec<String>>,
    /// `outcomePrices` mirrors `outcomes`; on a resolved market the winning
    /// outcome is 1.0 and the loser 0.0.
    #[serde(default, deserialize_with = "deserialize_outcomes")]
    outcome_prices: Option<Vec<String>>,
    #[serde(default)]
    enable_order_book: Option<bool>,
    #[serde(default)]
    end_date: Option<String>,
    #[serde(default)]
    liquidity_num: Option<f64>,
    /// True once the market has been resolved and settled.
    #[serde(default)]
    closed: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct GammaEvent {
    #[serde(default)]
    slug: Option<String>,
}

/// `outcomes` can arrive as a native JSON array or as a JSON-encoded string.
fn deserialize_outcomes<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_string_or_array(deserializer)
}

/// Gamma returns `clobTokenIds` as a JSON-encoded string, not a native array.
fn deserialize_clob_token_ids<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_string_or_array(deserializer)
}

/// Shared helper: deserializes a field that Gamma may send either as a native
/// JSON array of strings or as a JSON-encoded string of an array.
fn deserialize_string_or_array<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
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

/// Classify a market by category and primary underlying asset.
pub fn classify_market(question: &str, tags: &[String]) -> (String, String) {
    let q = question.to_lowercase();
    let tag_str = tags.join(" ").to_lowercase();
    let combined = format!("{q} {tag_str}");

    // --- Underlying asset ---
    let underlying = if combined.contains("btc") || combined.contains("bitcoin") {
        "BTC"
    } else if combined.contains("eth") || combined.contains("ethereum") {
        "ETH"
    } else if combined.contains("sol") || combined.contains("solana") {
        "SOL"
    } else if combined.contains("xrp") || combined.contains("ripple") {
        "XRP"
    } else if combined.contains("doge") || combined.contains("dogecoin") {
        "DOGE"
    } else if combined.contains("trump") || combined.contains("biden") || combined.contains("harris") {
        "US_POL"
    } else if combined.contains("fed") || combined.contains("fomc") || combined.contains("powell") {
        "FED"
    } else if combined.contains("s&p") || combined.contains("sp500") || combined.contains("nasdaq") {
        "EQUITIES"
    } else if combined.contains("oil") || combined.contains("crude") || combined.contains("wti") {
        "OIL"
    } else if combined.contains("gold") || combined.contains("xau") {
        "GOLD"
    } else {
        "OTHER"
    };

    // --- Category ---
    let category = if tag_str.contains("crypto")
        || combined.contains("bitcoin")
        || combined.contains("btc")
        || combined.contains("ethereum")
        || combined.contains("eth ")
        || combined.contains("solana")
        || combined.contains("xrp")
        || combined.contains("doge")
        || combined.contains("defi")
        || combined.contains("nft")
        || combined.contains("blockchain")
        || combined.contains("token")
        || combined.contains("altcoin")
    {
        "crypto"
    } else if tag_str.contains("politic")
        || tag_str.contains("election")
        || combined.contains("election")
        || combined.contains("president")
        || combined.contains("senate")
        || combined.contains("congress")
        || combined.contains("democrat")
        || combined.contains("republican")
        || combined.contains("trump")
        || combined.contains("biden")
        || combined.contains("harris")
        || combined.contains("elon musk")
        || combined.contains("white house")
    {
        "politics"
    } else if tag_str.contains("macro")
        || tag_str.contains("economy")
        || combined.contains("fed ")
        || combined.contains("fomc")
        || combined.contains("cpi")
        || combined.contains("inflation")
        || combined.contains("gdp")
        || combined.contains("recession")
        || combined.contains("interest rate")
        || combined.contains("treasury")
        || combined.contains("s&p")
        || combined.contains("sp500")
        || combined.contains("nasdaq")
        || combined.contains("oil")
        || combined.contains("gold")
    {
        "macro"
    } else if tag_str.contains("sport")
        || tag_str.contains("nfl")
        || tag_str.contains("nba")
        || tag_str.contains("mlb")
        || tag_str.contains("soccer")
        || tag_str.contains("football")
        || tag_str.contains("tennis")
        || tag_str.contains("ufc")
        || combined.contains("super bowl")
        || combined.contains("world cup")
        || combined.contains("championship")
        || combined.contains("playoff")
    {
        "sports"
    } else if tag_str.contains("entertain")
        || tag_str.contains("celebrity")
        || tag_str.contains("award")
        || combined.contains("oscar")
        || combined.contains("grammy")
        || combined.contains("emmy")
        || combined.contains("taylor swift")
    {
        "entertainment"
    } else if combined.contains("climate")
        || combined.contains("weather")
        || combined.contains("hurricane")
        || combined.contains("earthquake")
    {
        "science"
    } else {
        "other"
    };

    (category.to_string(), underlying.to_string())
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
            "slug": "will-btc-exceed-100k-739",
            "events": [{"slug": "btc-price-2026"}],
            "conditionId": "0xabc",
            "clobTokenIds": "[\"111\", \"222\"]",
            "outcomes": "[\"Yes\", \"No\"]",
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
        // outcomes[0]="Yes" → tokens[0]=YES, tokens[1]=NO (normal order)
        assert_eq!(meta.yes_token_id, "111");
        assert_eq!(meta.no_token_id, "222");
        assert_eq!(meta.category, "crypto");
        // Event slug is preferred over the per-market slug for the public URL.
        assert_eq!(meta.slug, "btc-price-2026");
    }

    #[test]
    fn falls_back_to_market_slug_without_event() {
        let raw = r#"{
            "id": "1", "question": "Standalone?", "slug": "standalone-market",
            "conditionId": "0x1", "clobTokenIds": "[\"1\", \"2\"]",
            "outcomes": "[\"Yes\", \"No\"]", "enableOrderBook": true,
            "endDate": "2026-07-31T12:00:00Z", "liquidityNum": 100.0
        }"#;
        let market: GammaMarket = serde_json::from_str(raw).unwrap();
        let client = GammaClient::new(&ExchangeConfig {
            gamma_base_url: "https://gamma-api.polymarket.com".into(),
            data_api_base_url: String::new(),
            clob_host: String::new(),
            chain_id: 137,
            market_discovery_limit: 200,
        });
        let meta = client.to_market_meta(market).expect("valid market");
        assert_eq!(meta.slug, "standalone-market");
    }

    #[test]
    fn detects_no_resolution_from_outcome_prices() {
        // NO won: outcomes=[Yes,No], prices=[0,1]
        let raw_no_won = r#"{
            "id": "1", "conditionId": "0x1", "closed": true,
            "outcomes": "[\"Yes\", \"No\"]", "outcomePrices": "[\"0\", \"1\"]"
        }"#;
        let m: GammaMarket = serde_json::from_str(raw_no_won).unwrap();
        assert_eq!(resolved_no_outcome(&m), Some(true));

        // YES won: prices=[1,0]
        let raw_yes_won = r#"{
            "id": "2", "conditionId": "0x2", "closed": true,
            "outcomes": "[\"Yes\", \"No\"]", "outcomePrices": "[\"1\", \"0\"]"
        }"#;
        let m: GammaMarket = serde_json::from_str(raw_yes_won).unwrap();
        assert_eq!(resolved_no_outcome(&m), Some(false));

        // Voided / indecisive: prices=[0,0]
        let raw_void = r#"{
            "id": "3", "conditionId": "0x3", "closed": true,
            "outcomes": "[\"Yes\", \"No\"]", "outcomePrices": "[\"0\", \"0\"]"
        }"#;
        let m: GammaMarket = serde_json::from_str(raw_void).unwrap();
        assert_eq!(resolved_no_outcome(&m), None);
    }

    #[test]
    fn swaps_yes_no_when_outcomes_inverted() {
        let raw = r#"{
            "id": "540818",
            "question": "Will it rain tomorrow?",
            "conditionId": "0xdef",
            "clobTokenIds": "[\"aaa\", \"bbb\"]",
            "outcomes": "[\"No\", \"Yes\"]",
            "enableOrderBook": true,
            "endDate": "2026-07-31T12:00:00Z",
            "liquidityNum": 5000.0
        }"#;
        let market: GammaMarket = serde_json::from_str(raw).unwrap();
        let client = GammaClient::new(&ExchangeConfig {
            gamma_base_url: "https://gamma-api.polymarket.com".into(),
            data_api_base_url: String::new(),
            clob_host: String::new(),
            chain_id: 137,
            market_discovery_limit: 200,
        });
        let meta = client.to_market_meta(market).expect("valid market");
        // outcomes[0]="No" → tokens[0] is actually the NO token; swap
        assert_eq!(meta.yes_token_id, "bbb");
        assert_eq!(meta.no_token_id, "aaa");
    }
}
