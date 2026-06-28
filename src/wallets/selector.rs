use anyhow::{Context, Result};
use chrono::DateTime;
use reqwest::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Candidate wallet from external leaderboards
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateWallet {
    pub address: String,
    pub win_rate: f64,
    pub closed_markets: u32,
    pub avg_hold_days: Option<f64>,
    pub max_drawdown: Option<f64>,
    pub total_pnl: f64,
    pub sharpe: Option<f64>,
    pub source: String,
    pub no_trade_ratio: Option<f64>,  // fraction of trades that are NO (0.0–1.0)
    pub no_win_rate: Option<f64>,     // win rate specifically on NO trades (0.0–1.0)
}

/// Filters for short-NO strategy wallet selection
const MIN_WIN_RATE: f64 = 0.60;
const MIN_CLOSED_MARKETS: u32 = 50;
const MAX_AVG_HOLD_DAYS: f64 = 7.0;
const MIN_DRAWDOWN: f64 = -0.30;
const MIN_NO_TRADE_RATIO: f64 = 0.40; // at least 40% of trades must be NO
const MIN_RESOLVED_NO_TRADES_FOR_NO_WIN_RATE: usize = 10;
const MIN_TOTAL_USABLE_TRADES: usize = 20;

// ─── Safe JSON extraction helpers ───────────────────────────────────

/// Extract a boolean from a JSON value, trying multiple field names
fn extract_bool(val: &serde_json::Value, keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Some(b) = val.get(*key).and_then(|v| v.as_bool()) {
            return Some(b);
        }
    }
    None
}

/// Extract an f64 from a JSON value, trying multiple field names
fn extract_f64(val: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(f) = val.get(*key).and_then(|v| v.as_f64()) {
            return Some(f);
        }
    }
    None
}

/// Extract a string from a JSON value, trying multiple field names
fn extract_string(val: &serde_json::Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(s) = val.get(*key).and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

/// Extract a unix timestamp (as f64 seconds) from a JSON value
fn extract_timestamp(val: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(ts) = val.get(*key) {
            // Try as number first (unix timestamp)
            if let Some(n) = ts.as_f64() {
                return Some(n);
            }
            // Try as string (ISO 8601 or unix string)
            if let Some(s) = ts.as_str() {
                if let Ok(n) = s.parse::<f64>() {
                    return Some(n);
                }
                if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
                    return Some(dt.timestamp() as f64);
                }
            }
        }
    }
    None
}

/// Fetch raw activity data from Polymarket Data API
async fn fetch_wallet_activity(client: &Client, address: &str) -> Result<Vec<serde_json::Value>> {
    let url = format!(
        "https://data-api.polymarket.com/activity?user={}&limit=500",
        address
    );
    
    debug!(wallet = %address, "fetching wallet activity from Data API");
    
    let resp = client
        .get(&url)
        .send()
        .await
        .context("fetching wallet activity")?;
    
    if !resp.status().is_success() {
        anyhow::bail!("Data API returned status {}", resp.status());
    }
    
    let activities: Vec<serde_json::Value> = resp
        .json()
        .await
        .context("parsing activity response")?;
    
    info!(
        wallet = %address,
        count = activities.len(),
        "fetched wallet activity"
    );
    
    Ok(activities)
}

/// Build a CandidateWallet from raw activity data
fn candidate_from_activity(
    address: &str,
    activities: &[serde_json::Value],
) -> Result<CandidateWallet> {
    // Filter to TRADE entries only (skip YIELD, etc.)
    let trades: Vec<_> = activities
        .iter()
        .filter(|a| {
            extract_string(a, &["type"]).map(|t| t.eq_ignore_ascii_case("TRADE")).unwrap_or(false)
        })
        .collect();
    
    if trades.is_empty() {
        anyhow::bail!("wallet has no usable trade activity");
    }
    
    let total_trades = trades.len();
    debug!(
        wallet = %address,
        total_trades,
        "processing trade activity"
    );
    
    // NO trade ratio
    let no_trades: Vec<_> = trades
        .iter()
        .filter(|t| {
            extract_string(t, &["outcome"])
                .map(|o| o.eq_ignore_ascii_case("No"))
                .unwrap_or(false)
        })
        .collect();
    
    let no_trade_ratio = no_trades.len() as f64 / total_trades as f64;
    debug!(
        wallet = %address,
        no_trade_ratio = format!("{:.2}", no_trade_ratio),
        "computed NO trade ratio"
    );
    
    // Unique markets (conditionIds)
    let unique_markets: std::collections::HashSet<_> = trades
        .iter()
        .filter_map(|t| extract_string(t, &["conditionId", "condition_id", "market_id"]))
        .filter(|s| !s.is_empty())
        .collect();
    
    let closed_markets = unique_markets.len() as u32;
    
    // Average hold time estimation
    // Group trades by conditionId, look for BUY/SELL pairs
    let mut condition_timestamps: HashMap<String, Vec<(f64, String)>> = HashMap::new();
    for trade in &trades {
        let condition_id = match extract_string(trade, &["conditionId", "condition_id"]) {
            Some(id) if !id.is_empty() => id,
            _ => continue,
        };
        let timestamp = match extract_timestamp(trade, &["timestamp", "createdAt", "created_at"]) {
            Some(ts) => ts,
            None => continue,
        };
        let side = extract_string(trade, &["side"]).unwrap_or_default();
        condition_timestamps
            .entry(condition_id)
            .or_default()
            .push((timestamp, side));
    }
    
    let hold_times: Vec<f64> = condition_timestamps
        .values()
        .filter_map(|entries| {
            if entries.len() < 2 {
                return None;
            }
            let mut sorted = entries.clone();
            sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let first = sorted.first()?.0;
            let last = sorted.last()?.0;
            let days = (last - first) / 86400.0;
            if days > 0.0 { Some(days) } else { None }
        })
        .collect();
    
    let avg_hold_days = if hold_times.is_empty() {
        None
    } else {
        Some(hold_times.iter().sum::<f64>() / hold_times.len() as f64)
    };
    
    // Note: win_rate, total_pnl, no_win_rate cannot be reliably computed from activity alone
    // They require resolution data which the activity endpoint doesn't provide
    // These will be set to conservative defaults
    
    Ok(CandidateWallet {
        address: address.to_string(),
        win_rate: 0.0, // Cannot compute from activity
        closed_markets,
        avg_hold_days,
        max_drawdown: None,
        total_pnl: 0.0, // Cannot compute from activity
        sharpe: None,
        source: "data_api_direct".to_string(),
        no_trade_ratio: Some(no_trade_ratio),
        no_win_rate: None, // Cannot compute without resolution data
    })
}

/// Compute composite score for ranking wallets
/// Higher is better: combines Sharpe, win_rate, PnL, closed_markets, and NO-side quality
pub fn compute_score(w: &CandidateWallet) -> f64 {
    let sharpe_score = w.sharpe.map(|s| 1.0 / (1.0 + (-s).exp())).unwrap_or(0.5);
    
    // Use no_win_rate if available, fall back to global win_rate
    let effective_wr = w.no_win_rate.unwrap_or(w.win_rate);
    let wr_score = effective_wr;
    
    let pnl_score = w.total_pnl.max(0.0).ln_1p() / 10.0_f64.ln_1p();
    let markets_score = (w.closed_markets as f64).sqrt() / 500_f64.sqrt();
    
    // NO trade ratio bonus: wallets that heavily trade NO get up to 20% boost
    let no_ratio_bonus = w.no_trade_ratio.unwrap_or(0.5).min(1.0) * 0.20;
    
    // Weights: Sharpe 30%, win_rate 30%, pnl 10%, markets 10%, NO ratio bonus 20%
    0.30 * sharpe_score + 0.30 * wr_score + 0.10 * pnl_score.min(1.0) + 0.10 * markets_score.min(1.0) + no_ratio_bonus
}

/// Fetch candidate wallets from multiple external sources
pub async fn fetch_candidate_wallets() -> Result<Vec<String>> {
    let client = Client::builder()
        .user_agent("polymarket-no-bot/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    let mut all_wallets: Vec<CandidateWallet> = Vec::new();

    // Fetch from Polymarket Official Leaderboard
    match fetch_polymarket_official_wallets(&client).await {
        Ok(wallets) => {
            info!(count = wallets.len(), "fetched wallets from Polymarket Official");
            all_wallets.extend(wallets);
        }
        Err(e) => {
            warn!(error = %e, "failed to fetch Polymarket Official wallets");
        }
    }

    // Fetch from Polyburg
    match fetch_polyburg_wallets(&client).await {
        Ok(wallets) => {
            info!(count = wallets.len(), "fetched wallets from Polyburg");
            all_wallets.extend(wallets);
        }
        Err(e) => {
            warn!(error = %e, "failed to fetch Polyburg wallets");
        }
    }

    // Fetch from Polyscalping
    match fetch_polyscalping_wallets(&client).await {
        Ok(wallets) => {
            info!(count = wallets.len(), "fetched wallets from Polyscalping");
            all_wallets.extend(wallets);
        }
        Err(e) => {
            warn!(error = %e, "failed to fetch Polyscalping wallets");
        }
    }

    // Fetch from PolyAlertHub
    match fetch_polyalerthub_wallets(&client).await {
        Ok(wallets) => {
            info!(count = wallets.len(), "fetched wallets from PolyAlertHub");
            all_wallets.extend(wallets);
        }
        Err(e) => {
            warn!(error = %e, "failed to fetch PolyAlertHub wallets");
        }
    }

    let total = all_wallets.len();

    // Fallback if all sources failed
    if total == 0 {
        warn!("all external sources failed — returning empty wallet list; using config static wallets as fallback");
        return Ok(vec![]);
    }

    // Deduplicate by address (keep entry with most non-None fields)
    let deduped = deduplicate_wallets(all_wallets);
    info!(
        before_dedup = total,
        after_dedup = deduped.len(),
        "deduplicated wallets across sources"
    );

    // Apply filters
    let filtered: Vec<CandidateWallet> = deduped
        .into_iter()
        .filter(|w| passes_filters(w))
        .collect();

    info!(
        total_seen = total,
        passed_filters = filtered.len(),
        ?filtered,
        "wallet discovery: filtered candidate wallets with high winrate"
    );

    // Sort by compute_score (descending)
    let mut sorted = filtered;
    sorted.sort_by(|a, b| {
        let score_a = compute_score(a);
        let score_b = compute_score(b);
        score_b.partial_cmp(&score_a).unwrap_or(std::cmp::Ordering::Equal)
    });

    // Log top 10
    let top10: Vec<_> = sorted.iter().take(10).collect();
    for (i, w) in top10.iter().enumerate() {
        info!(
            rank = i + 1,
            wallet = %w.address,
            source = %w.source,
            win_rate = format!("{:.1}%", w.win_rate * 100.0),
            closed_markets = w.closed_markets,
            total_pnl = w.total_pnl,
            sharpe = ?w.sharpe,
            score = compute_score(w),
            "top candidate wallet"
        );
    }

    // Return addresses in lowercase
    Ok(sorted.into_iter().map(|w| w.address.to_lowercase()).collect())
}

/// Deduplicate wallets by address, keeping the entry with most data
fn deduplicate_wallets(wallets: Vec<CandidateWallet>) -> Vec<CandidateWallet> {
    let mut map: HashMap<String, CandidateWallet> = HashMap::new();

    for w in wallets {
        let key = w.address.to_lowercase();
        let existing = map.get(&key);

        let should_replace = match existing {
            None => true,
            Some(e) => count_non_none_fields(&w) > count_non_none_fields(e),
        };

        if should_replace {
            map.insert(key, w);
        }
    }

    map.into_values().collect()
}

/// Count non-None fields for deduplication priority
fn count_non_none_fields(w: &CandidateWallet) -> usize {
    let mut count = 0;
    if !w.address.is_empty() { count += 1; }
    if w.win_rate > 0.0 { count += 1; }
    if w.closed_markets > 0 { count += 1; }
    if w.avg_hold_days.is_some() { count += 1; }
    if w.max_drawdown.is_some() { count += 1; }
    if w.total_pnl != 0.0 { count += 1; }
    if w.sharpe.is_some() { count += 1; }
    if w.no_trade_ratio.is_some() { count += 1; }
    if w.no_win_rate.is_some() { count += 1; }
    count
}

/// Fetch wallets from Polymarket Official Leaderboard (Gamma API)
async fn fetch_polymarket_official_wallets(client: &Client) -> Result<Vec<CandidateWallet>> {
    let mut all_wallets = Vec::new();
    let mut offset = 0;
    let limit = 100;
    let max_pages = 3;

    for _ in 0..max_pages {
        let url = format!(
            "https://gamma-api.polymarket.com/leaderboard?window=all&limit={}&offset={}",
            limit, offset
        );

        let resp = client
            .get(&url)
            .send()
            .await
            .context("fetching Polymarket Official leaderboard")?;

        if !resp.status().is_success() {
            anyhow::bail!("Polymarket Official returned status {}", resp.status());
        }

        let entries: Vec<PolymarketOfficialEntry> = resp
            .json()
            .await
            .context("parsing Polymarket Official JSON")?;

        if entries.is_empty() {
            break;
        }

        for entry in entries {
            let address = entry.proxy_wallet_address.or(entry.address);
            if address.is_none() {
                continue;
            }

            let addr = address.unwrap();
            
            // Try to enrich with NO-side data (best-effort, with short timeout)
            let (no_trade_ratio, no_win_rate) = tokio::time::timeout(
                Duration::from_secs(5),
                fetch_no_side_ratio(client, &addr)
            )
            .await
            .ok()
            .flatten()
            .unwrap_or((0.5, None)); // Default to 50% if fetch fails

            all_wallets.push(CandidateWallet {
                address: addr,
                win_rate: entry.win_rate.unwrap_or(0.0),
                closed_markets: entry.markets_traded.unwrap_or(0),
                avg_hold_days: None,
                max_drawdown: entry.drawdown,
                total_pnl: entry.profit.unwrap_or(0.0),
                sharpe: entry.sharpe,
                source: "polymarket_official".to_string(),
                no_trade_ratio: Some(no_trade_ratio),
                no_win_rate,
            });
        }

        offset += limit;
    }

    Ok(all_wallets)
}

#[derive(Debug, Deserialize)]
struct PolymarketOfficialEntry {
    address: Option<String>,
    #[serde(alias = "proxyWalletAddress", alias = "proxy_wallet")]
    proxy_wallet_address: Option<String>,
    profit: Option<f64>,
    volume: Option<f64>,
    #[serde(alias = "marketsTraded")]
    markets_traded: Option<u32>,
    #[serde(alias = "winRate")]
    win_rate: Option<f64>,
    sharpe: Option<f64>,
    drawdown: Option<f64>,
}

/// Fetch NO-side trade ratio and win rate from Polymarket Data API
/// Returns (no_trade_ratio, no_win_rate) or None on failure
async fn fetch_no_side_ratio(client: &Client, address: &str) -> Option<(f64, Option<f64>)> {
    let url = format!(
        "https://data-api.polymarket.com/activity?user={}&limit=200",
        address
    );
    let resp = client.get(&url).send().await.ok()?;
    if !resp.status().is_success() { return None; }
    
    let trades: Vec<serde_json::Value> = resp.json().await.ok()?;
    if trades.is_empty() { return None; }
    
    let total = trades.len() as f64;
    let no_trades: Vec<_> = trades.iter()
        .filter(|t| t.get("outcome").and_then(|v| v.as_str()) == Some("No"))
        .collect();
    
    let no_count = no_trades.len() as f64;
    let no_ratio = no_count / total;
    
    // Calculate win rate on NO trades (resolved markets only)
    let resolved_no: Vec<_> = no_trades.iter()
        .filter(|t| t.get("resolved").and_then(|v| v.as_bool()) == Some(true))
        .collect();
    
    let no_wr = if resolved_no.is_empty() {
        None
    } else {
        let wins = resolved_no.iter()
            .filter(|t| t.get("won").and_then(|v| v.as_bool()) == Some(true))
            .count() as f64;
        Some(wins / resolved_no.len() as f64)
    };
    
    Some((no_ratio, no_wr))
}

/// Fetch wallets from Polyburg leaderboard
async fn fetch_polyburg_wallets(client: &Client) -> Result<Vec<CandidateWallet>> {
    let url = "https://polyburg.com/leaderboard";
    let resp = client
        .get(url)
        .send()
        .await
        .context("fetching Polyburg leaderboard")?;

    if !resp.status().is_success() {
        anyhow::bail!("Polyburg returned status {}", resp.status());
    }

    let html = resp.text().await.context("reading Polyburg response")?;
    parse_polyburg_html(&html)
}

/// Parse Polyburg leaderboard HTML
fn parse_polyburg_html(html: &str) -> Result<Vec<CandidateWallet>> {
    let document = Html::parse_document(html);
    let mut wallets = Vec::new();

    let row_selector = Selector::parse("table tbody tr, .leaderboard-row, .trader-row")
        .unwrap_or(Selector::parse("tr").unwrap());
    let cell_selector = Selector::parse("td").unwrap();

    for row in document.select(&row_selector) {
        let cells: Vec<_> = row.select(&cell_selector).collect();
        if cells.len() < 4 {
            continue;
        }

        let address = extract_address_from_row(&row, &cells);
        if address.is_none() {
            continue;
        }

        wallets.push(CandidateWallet {
            address: address.unwrap(),
            win_rate: parse_cell_f64(&cells, 2).map(|v| if v > 1.0 { v / 100.0 } else { v }).unwrap_or(0.0),
            closed_markets: parse_cell_u32(&cells, 3).unwrap_or(0),
            avg_hold_days: parse_cell_f64(&cells, 4),
            max_drawdown: parse_cell_f64(&cells, 5).map(|v| if v > 0.0 { -v } else { v }),
            total_pnl: parse_cell_f64(&cells, 6).unwrap_or(0.0),
            sharpe: parse_cell_f64(&cells, 7),
            source: "polyburg".to_string(),
            no_trade_ratio: None,
            no_win_rate: None,
        });
    }

    Ok(wallets)
}

/// Fetch wallets from Polyscalping leaderboard
async fn fetch_polyscalping_wallets(client: &Client) -> Result<Vec<CandidateWallet>> {
    let url = "https://polyscalping.com/traders";
    let resp = client
        .get(url)
        .send()
        .await
        .context("fetching Polyscalping traders")?;

    if !resp.status().is_success() {
        // Try alternative URL
        let url = "https://polyscalping.com/leaderboard";
        let resp = client
            .get(url)
            .send()
            .await
            .context("fetching Polyscalping leaderboard")?;

        if !resp.status().is_success() {
            anyhow::bail!("Polyscalping returned status {}", resp.status());
        }

        return parse_polyscalping_response(resp).await;
    }

    parse_polyscalping_response(resp).await
}

/// Parse Polyscalping response (JSON or HTML)
async fn parse_polyscalping_response(resp: reqwest::Response) -> Result<Vec<CandidateWallet>> {
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("application/json") {
        let json: serde_json::Value = resp.json().await?;
        parse_generic_json(&json, "polyscalping")
    } else {
        let html = resp.text().await?;
        parse_generic_html(&html, "polyscalping")
    }
}

/// Fetch wallets from PolyAlertHub top traders
async fn fetch_polyalerthub_wallets(client: &Client) -> Result<Vec<CandidateWallet>> {
    let url = "https://polyalerthub.com/traders";
    let resp = client
        .get(url)
        .send()
        .await
        .context("fetching PolyAlertHub traders")?;

    if !resp.status().is_success() {
        // Try alternative URL
        let url = "https://polyalerthub.com/leaderboard";
        let resp = client
            .get(url)
            .send()
            .await
            .context("fetching PolyAlertHub leaderboard")?;

        if !resp.status().is_success() {
            anyhow::bail!("PolyAlertHub returned status {}", resp.status());
        }

        return parse_polyalerthub_response(resp).await;
    }

    parse_polyalerthub_response(resp).await
}

/// Parse PolyAlertHub response (JSON or HTML)
async fn parse_polyalerthub_response(resp: reqwest::Response) -> Result<Vec<CandidateWallet>> {
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("application/json") {
        let json: serde_json::Value = resp.json().await?;
        parse_generic_json(&json, "polyalerthub")
    } else {
        let html = resp.text().await?;
        parse_generic_html(&html, "polyalerthub")
    }
}

/// Parse generic JSON response from any source
fn parse_generic_json(json: &serde_json::Value, source: &str) -> Result<Vec<CandidateWallet>> {
    let mut wallets = Vec::new();

    let entries = json
        .get("data")
        .or_else(|| json.get("traders"))
        .and_then(|v| v.as_array())
        .or_else(|| json.as_array())
        .context("could not find traders array in JSON")?;

    for entry in entries {
        let address = entry
            .get("address")
            .or_else(|| entry.get("wallet"))
            .or_else(|| entry.get("user"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if address.is_none() {
            continue;
        }

        wallets.push(CandidateWallet {
            address: address.unwrap(),
            win_rate: entry.get("win_rate").and_then(|v| v.as_f64()).unwrap_or(0.0),
            closed_markets: entry
                .get("closed_markets")
                .or_else(|| entry.get("markets"))
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
                .unwrap_or(0),
            avg_hold_days: entry.get("avg_hold_days").and_then(|v| v.as_f64()),
            max_drawdown: entry.get("max_drawdown").and_then(|v| v.as_f64()),
            total_pnl: entry
                .get("total_pnl")
                .or_else(|| entry.get("pnl"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            sharpe: entry.get("sharpe").and_then(|v| v.as_f64()),
            source: source.to_string(),
            no_trade_ratio: None,
            no_win_rate: None,
        });
    }

    Ok(wallets)
}

/// Parse generic HTML response from any source
fn parse_generic_html(html: &str, source: &str) -> Result<Vec<CandidateWallet>> {
    let document = Html::parse_document(html);
    let mut wallets = Vec::new();

    let row_selector = Selector::parse("table tbody tr, .trader-row, .leaderboard-item").unwrap();
    let cell_selector = Selector::parse("td, .metric, .stat").unwrap();

    for row in document.select(&row_selector) {
        let cells: Vec<_> = row.select(&cell_selector).collect();
        if cells.len() < 3 {
            continue;
        }

        let address = extract_address_from_row(&row, &cells);
        if address.is_none() {
            continue;
        }

        wallets.push(CandidateWallet {
            address: address.unwrap(),
            win_rate: parse_cell_f64(&cells, 2).map(|v| if v > 1.0 { v / 100.0 } else { v }).unwrap_or(0.0),
            closed_markets: parse_cell_u32(&cells, 3).unwrap_or(0),
            avg_hold_days: parse_cell_f64(&cells, 4),
            max_drawdown: parse_cell_f64(&cells, 5),
            total_pnl: parse_cell_f64(&cells, 6).unwrap_or(0.0),
            sharpe: parse_cell_f64(&cells, 7),
            source: source.to_string(),
            no_trade_ratio: None,
            no_win_rate: None,
        });
    }

    Ok(wallets)
}

/// Extract wallet address from row/cells
fn extract_address_from_row(
    row: &scraper::ElementRef,
    cells: &[scraper::ElementRef],
) -> Option<String> {
    // Try data attributes first
    if let Some(addr) = row.value().attr("data-wallet")
        .or_else(|| row.value().attr("data-address"))
        .or_else(|| row.value().attr("data-trader"))
    {
        if addr.starts_with("0x") && addr.len() == 42 {
            return Some(addr.to_string());
        }
    }

    // Try links
    let link_selector = Selector::parse("a[href*='0x']").unwrap();
    for link in row.select(&link_selector) {
        if let Some(href) = link.value().attr("href") {
            if let Some(addr) = extract_address_from_url(href) {
                return Some(addr);
            }
        }
    }

    // Try cell text content
    for cell in cells.iter().take(3) {
        let text = cell.text().collect::<String>();
        if text.starts_with("0x") && text.len() >= 42 {
            let addr = &text[..42];
            if addr.starts_with("0x") {
                return Some(addr.to_string());
            }
        }
    }

    None
}

/// Extract address from URL
fn extract_address_from_url(url: &str) -> Option<String> {
    let start = url.find("0x")?;
    let addr = &url[start..];
    if addr.len() >= 42 {
        let addr = &addr[..42];
        if addr.starts_with("0x") {
            return Some(addr.to_string());
        }
    }
    None
}

/// Parse f64 from cell text
fn parse_cell_f64(cells: &[scraper::ElementRef], idx: usize) -> Option<f64> {
    cells.get(idx).and_then(|cell| {
        let text = cell.text().collect::<String>();
        let cleaned = text
            .replace('$', "")
            .replace(',', "")
            .replace('%', "")
            .trim()
            .to_string();
        cleaned.parse().ok()
    })
}

/// Parse u32 from cell text
fn parse_cell_u32(cells: &[scraper::ElementRef], idx: usize) -> Option<u32> {
    cells.get(idx).and_then(|cell| {
        let text = cell.text().collect::<String>();
        let cleaned = text.replace(',', "").trim().to_string();
        cleaned.parse().ok()
    })
}

/// Check if a wallet passes filters
fn passes_filters(w: &CandidateWallet) -> bool {
    // Win rate must be >= 60%
    if w.win_rate < MIN_WIN_RATE {
        debug!(
            wallet = %w.address,
            win_rate = w.win_rate,
            "rejecting: low win rate"
        );
        return false;
    }

    // Must have at least 50 closed markets
    if w.closed_markets < MIN_CLOSED_MARKETS {
        debug!(
            wallet = %w.address,
            closed_markets = w.closed_markets,
            "rejecting: insufficient closed markets"
        );
        return false;
    }

    // Average hold filter (if available)
    if let Some(hold) = w.avg_hold_days {
        if hold > MAX_AVG_HOLD_DAYS {
            debug!(
                wallet = %w.address,
                avg_hold = hold,
                "rejecting: long avg hold"
            );
            return false;
        }
    }

    // Max drawdown filter (if available)
    if let Some(dd) = w.max_drawdown {
        if dd < MIN_DRAWDOWN {
            debug!(
                wallet = %w.address,
                drawdown = dd,
                "rejecting: excessive drawdown"
            );
            return false;
        }
    }

    // Must trade NO side at least 40% of the time (if data available)
    if let Some(no_ratio) = w.no_trade_ratio {
        if no_ratio < MIN_NO_TRADE_RATIO {
            debug!(
                wallet = %w.address,
                no_trade_ratio = no_ratio,
                "rejecting: insufficient NO trade ratio"
            );
            return false;
        }
    }

    true
}

/// Result of evaluating a wallet's quality
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletEvaluation {
    pub address: String,
    pub win_rate: Option<f64>,
    pub closed_markets: Option<u32>,
    pub avg_hold_days: Option<f64>,
    pub max_drawdown: Option<f64>,
    pub total_pnl: Option<f64>,
    pub sharpe: Option<f64>,
    pub score: f64,
    pub status: String,
    pub reasons: Vec<String>,
}

/// Evaluate a wallet's quality and return detailed results
pub fn evaluate_wallet(w: &CandidateWallet) -> WalletEvaluation {
    let mut reasons = Vec::new();
    
    if w.win_rate < MIN_WIN_RATE {
        reasons.push(format!("win_rate {:.1}% < {:.0}%", w.win_rate * 100.0, MIN_WIN_RATE * 100.0));
    }
    
    if w.closed_markets < MIN_CLOSED_MARKETS {
        reasons.push(format!("closed_markets {} < {}", w.closed_markets, MIN_CLOSED_MARKETS));
    }
    
    if let Some(hold) = w.avg_hold_days {
        if hold > MAX_AVG_HOLD_DAYS {
            reasons.push(format!("avg_hold_days {:.1} > {:.1}", hold, MAX_AVG_HOLD_DAYS));
        }
    }
    
    if let Some(dd) = w.max_drawdown {
        if dd < MIN_DRAWDOWN {
            reasons.push(format!("max_drawdown {:.1}% < {:.0}%", dd * 100.0, MIN_DRAWDOWN * 100.0));
        }
    }
    
    // NO-side checks
    if let Some(no_ratio) = w.no_trade_ratio {
        if no_ratio < MIN_NO_TRADE_RATIO {
            reasons.push(format!("no_trade_ratio {:.1}% < {:.0}%", no_ratio * 100.0, MIN_NO_TRADE_RATIO * 100.0));
        }
    } else {
        reasons.push("no_trade_ratio unknown".to_string());
    }
    
    // NO win rate check (only if we have enough resolved NO trades)
    if let Some(no_wr) = w.no_win_rate {
        // Use a slightly lower threshold for NO win rate since NO bets have higher base probability
        const MIN_NO_WIN_RATE: f64 = 0.55;
        if no_wr < MIN_NO_WIN_RATE {
            reasons.push(format!("no_win_rate {:.1}% < {:.0}%", no_wr * 100.0, MIN_NO_WIN_RATE * 100.0));
        }
    }
    
    let status = if reasons.is_empty() { "OK".to_string() } else { "WEAK".to_string() };
    
    WalletEvaluation {
        address: w.address.clone(),
        win_rate: Some(w.win_rate),
        closed_markets: Some(w.closed_markets),
        avg_hold_days: w.avg_hold_days,
        max_drawdown: w.max_drawdown,
        total_pnl: Some(w.total_pnl),
        sharpe: w.sharpe,
        score: compute_score(w),
        status,
        reasons,
    }
}

/// Fetch metrics for a specific wallet address directly from Polymarket Data API
/// Uses /activity for trade patterns and /positions for PnL/resolution data
pub async fn fetch_wallet_metrics(address: &str) -> Result<CandidateWallet> {
    let client = Client::builder()
        .user_agent("polymarket-no-bot/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;
    
    // Fetch activity data for trade patterns
    let activities = fetch_wallet_activity(&client, address).await?;
    
    if activities.is_empty() {
        anyhow::bail!("wallet has no usable activity history");
    }
    
    // Build base metrics from activity
    let mut wallet = candidate_from_activity(address, &activities)?;
    
    // Supplement with positions data for PnL and resolution metrics
    let positions_url = format!(
        "https://data-api.polymarket.com/positions?user={}&limit=500",
        address
    );
    
    if let Ok(resp) = client.get(&positions_url).send().await {
        if resp.status().is_success() {
            if let Ok(positions) = resp.json::<Vec<serde_json::Value>>().await {
                if !positions.is_empty() {
                    enrich_from_positions(&mut wallet, &positions);
                }
            }
        }
    }
    
    info!(
        wallet = %address,
        win_rate = wallet.win_rate,
        closed_markets = wallet.closed_markets,
        total_pnl = wallet.total_pnl,
        no_trade_ratio = ?wallet.no_trade_ratio,
        no_win_rate = ?wallet.no_win_rate,
        "computed wallet metrics"
    );
    
    Ok(wallet)
}

/// Enrich a CandidateWallet with data from positions endpoint
fn enrich_from_positions(wallet: &mut CandidateWallet, positions: &[serde_json::Value]) {
    let total_positions = positions.len() as f64;
    if total_positions == 0.0 {
        return;
    }
    
    // Resolved positions (redeemable=true)
    let resolved: Vec<_> = positions
        .iter()
        .filter(|p| extract_bool(p, &["redeemable"]).unwrap_or(false))
        .collect();
    
    // Win rate from resolved positions
    if !resolved.is_empty() {
        let wins = resolved
            .iter()
            .filter(|p| extract_f64(p, &["cashPnl", "realizedPnl"]).unwrap_or(0.0) > 0.0)
            .count() as f64;
        wallet.win_rate = wins / resolved.len() as f64;
    }
    
    // Total PnL from all positions
    wallet.total_pnl = positions
        .iter()
        .filter_map(|p| extract_f64(p, &["cashPnl", "realizedPnl"]))
        .sum();
    
    // Closed markets from resolved positions
    let unique_resolved: std::collections::HashSet<_> = resolved
        .iter()
        .filter_map(|p| extract_string(p, &["conditionId", "condition_id"]))
        .collect();
    if !unique_resolved.is_empty() {
        wallet.closed_markets = unique_resolved.len() as u32;
    }
    
    // NO win rate from resolved NO positions
    let resolved_no: Vec<_> = positions
        .iter()
        .filter(|p| {
            extract_bool(p, &["redeemable"]).unwrap_or(false)
                && extract_string(p, &["outcome"])
                    .map(|o| o.eq_ignore_ascii_case("No"))
                    .unwrap_or(false)
        })
        .collect();
    
    if resolved_no.len() >= MIN_RESOLVED_NO_TRADES_FOR_NO_WIN_RATE {
        let no_wins = resolved_no
            .iter()
            .filter(|p| extract_f64(p, &["cashPnl", "realizedPnl"]).unwrap_or(0.0) > 0.0)
            .count() as f64;
        wallet.no_win_rate = Some(no_wins / resolved_no.len() as f64);
    }
    
    debug!(
        wallet = %wallet.address,
        resolved_count = resolved.len(),
        resolved_no_count = resolved_no.len(),
        "enriched from positions"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_wallet(
        win_rate: f64,
        closed_markets: u32,
        avg_hold: Option<f64>,
        drawdown: Option<f64>,
    ) -> CandidateWallet {
        CandidateWallet {
            address: "0x1234567890123456789012345678901234567890".to_string(),
            win_rate,
            closed_markets,
            avg_hold_days: avg_hold,
            max_drawdown: drawdown,
            total_pnl: 1000.0,
            sharpe: Some(1.0),
            source: "test".to_string(),
            no_trade_ratio: None,
            no_win_rate: None,
        }
    }

    #[test]
    fn test_passes_filters_good_wallet() {
        let wallet = sample_wallet(0.65, 100, Some(5.0), Some(-0.15));
        assert!(passes_filters(&wallet));
    }

    #[test]
    fn test_rejects_low_win_rate() {
        let wallet = sample_wallet(0.45, 100, Some(5.0), Some(-0.15));
        assert!(!passes_filters(&wallet));
    }

    #[test]
    fn test_rejects_insufficient_markets() {
        let wallet = sample_wallet(0.65, 10, Some(5.0), Some(-0.15));
        assert!(!passes_filters(&wallet));
    }

    #[test]
    fn test_rejects_long_hold() {
        let wallet = sample_wallet(0.65, 100, Some(10.0), Some(-0.15));
        assert!(!passes_filters(&wallet));
    }

    #[test]
    fn test_rejects_excessive_drawdown() {
        let wallet = sample_wallet(0.65, 100, Some(5.0), Some(-0.40));
        assert!(!passes_filters(&wallet));
    }

    #[test]
    fn test_boundary_conditions() {
        // Exactly at minimum thresholds should pass
        let boundary = sample_wallet(MIN_WIN_RATE, MIN_CLOSED_MARKETS, Some(MAX_AVG_HOLD_DAYS), Some(MIN_DRAWDOWN));
        assert!(passes_filters(&boundary));

        // Just below minimums should fail
        let below_wr = sample_wallet(MIN_WIN_RATE - 0.01, MIN_CLOSED_MARKETS, Some(MAX_AVG_HOLD_DAYS), Some(MIN_DRAWDOWN));
        assert!(!passes_filters(&below_wr));
    }

    #[test]
    fn test_rejects_low_no_trade_ratio() {
        let mut w = sample_wallet(0.65, 100, Some(5.0), Some(-0.15));
        w.no_trade_ratio = Some(0.10); // only 10% NO trades
        w.no_win_rate = None;
        assert!(!passes_filters(&w));
    }

    #[test]
    fn test_accepts_high_no_trade_ratio() {
        let mut w = sample_wallet(0.65, 100, Some(5.0), Some(-0.15));
        w.no_trade_ratio = Some(0.60);
        w.no_win_rate = Some(0.70);
        assert!(passes_filters(&w));
    }

    #[test]
    fn test_extract_address_from_url() {
        assert_eq!(
            extract_address_from_url("https://polymarket.com/profile/0x1234567890123456789012345678901234567890"),
            Some("0x1234567890123456789012345678901234567890".to_string())
        );
        assert_eq!(extract_address_from_url("https://example.com"), None);
    }

    #[test]
    fn test_compute_score_no_division_by_zero() {
        let w = sample_wallet(0.65, 100, Some(5.0), Some(-0.15));
        let score = compute_score(&w);
        assert!(score > 0.0 && score <= 1.0, "score={score}");
        
        // wallet con pnl=0 y sin sharpe no debe explotar
        let mut w2 = w.clone();
        w2.total_pnl = 0.0;
        w2.sharpe = None;
        let score2 = compute_score(&w2);
        assert!(score2 >= 0.0 && score2 <= 1.0, "score2={score2}");
        
        // wallet con closed_markets=0
        let w3 = sample_wallet(0.60, 0, None, None);
        let score3 = compute_score(&w3);
        assert!(score3 >= 0.0, "score3={score3}");
    }

    #[test]
    fn test_deduplicate_wallets() {
        let w1 = CandidateWallet {
            address: "0x1234567890123456789012345678901234567890".to_string(),
            win_rate: 0.65,
            closed_markets: 100,
            avg_hold_days: None,
            max_drawdown: None,
            total_pnl: 1000.0,
            sharpe: Some(1.0),
            source: "source1".to_string(),
            no_trade_ratio: None,
            no_win_rate: None,
        };

        let w2 = CandidateWallet {
            address: "0x1234567890123456789012345678901234567890".to_string(),
            win_rate: 0.70,
            closed_markets: 150,
            avg_hold_days: Some(5.0),
            max_drawdown: Some(-0.10),
            total_pnl: 2000.0,
            sharpe: Some(1.5),
            source: "source2".to_string(),
            no_trade_ratio: Some(0.65),
            no_win_rate: Some(0.75),
        };

        let wallets = vec![w1, w2];
        let deduped = deduplicate_wallets(wallets);
        
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].win_rate, 0.70); // Should keep w2 (more fields)
    }

    #[test]
    fn test_evaluate_wallet_with_no_side_metrics() {
        let mut w = sample_wallet(0.65, 100, Some(5.0), Some(-0.15));
        w.no_trade_ratio = Some(0.70);
        w.no_win_rate = Some(0.65);
        
        let eval = evaluate_wallet(&w);
        assert_eq!(eval.status, "OK");
        assert!(eval.reasons.is_empty());
    }

    #[test]
    fn test_evaluate_wallet_rejects_low_no_trade_ratio() {
        let mut w = sample_wallet(0.65, 100, Some(5.0), Some(-0.15));
        w.no_trade_ratio = Some(0.20);
        w.no_win_rate = Some(0.65);
        
        let eval = evaluate_wallet(&w);
        assert_eq!(eval.status, "WEAK");
        assert!(eval.reasons.iter().any(|r| r.contains("no_trade_ratio")));
    }

    #[test]
    fn test_evaluate_wallet_rejects_low_no_win_rate() {
        let mut w = sample_wallet(0.65, 100, Some(5.0), Some(-0.15));
        w.no_trade_ratio = Some(0.70);
        w.no_win_rate = Some(0.40);
        
        let eval = evaluate_wallet(&w);
        assert_eq!(eval.status, "WEAK");
        assert!(eval.reasons.iter().any(|r| r.contains("no_win_rate")));
    }

    #[test]
    fn test_evaluate_wallet_unknown_no_trade_ratio() {
        let mut w = sample_wallet(0.65, 100, Some(5.0), Some(-0.15));
        w.no_trade_ratio = None;
        w.no_win_rate = None;
        
        let eval = evaluate_wallet(&w);
        assert_eq!(eval.status, "WEAK");
        assert!(eval.reasons.iter().any(|r| r.contains("no_trade_ratio unknown")));
    }

    #[test]
    fn test_candidate_from_activity_basic() {
        let activities = vec![
            serde_json::json!({
                "type": "TRADE",
                "outcome": "No",
                "side": "BUY",
                "conditionId": "0xabc123",
                "timestamp": 1700000000.0
            }),
            serde_json::json!({
                "type": "TRADE",
                "outcome": "No",
                "side": "BUY",
                "conditionId": "0xdef456",
                "timestamp": 1700100000.0
            }),
            serde_json::json!({
                "type": "TRADE",
                "outcome": "Yes",
                "side": "BUY",
                "conditionId": "0xghi789",
                "timestamp": 1700200000.0
            }),
        ];
        
        let result = candidate_from_activity("0x1234567890123456789012345678901234567890", &activities);
        assert!(result.is_ok());
        
        let wallet = result.unwrap();
        assert_eq!(wallet.closed_markets, 3);
        assert!((wallet.no_trade_ratio.unwrap() - 0.666).abs() < 0.01);
        assert_eq!(wallet.source, "data_api_direct");
    }

    #[test]
    fn test_candidate_from_activity_filters_non_trades() {
        let activities = vec![
            serde_json::json!({
                "type": "YIELD",
                "outcome": "",
                "timestamp": 1700000000.0
            }),
            serde_json::json!({
                "type": "TRADE",
                "outcome": "No",
                "side": "BUY",
                "conditionId": "0xabc123",
                "timestamp": 1700100000.0
            }),
        ];
        
        let result = candidate_from_activity("0x1234567890123456789012345678901234567890", &activities);
        assert!(result.is_ok());
        
        let wallet = result.unwrap();
        assert_eq!(wallet.closed_markets, 1);
        assert!((wallet.no_trade_ratio.unwrap() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_candidate_from_activity_empty_trades() {
        let activities = vec![
            serde_json::json!({
                "type": "YIELD",
                "outcome": "",
                "timestamp": 1700000000.0
            }),
        ];
        
        let result = candidate_from_activity("0x1234567890123456789012345678901234567890", &activities);
        assert!(result.is_err());
    }

    #[test]
    fn test_compute_score_with_no_metrics() {
        let mut w = sample_wallet(0.65, 100, Some(5.0), Some(-0.15));
        w.no_trade_ratio = None;
        w.no_win_rate = None;
        
        let score = compute_score(&w);
        assert!(score > 0.0 && score <= 1.0, "score={score}");
    }

    #[test]
    fn test_extract_helpers() {
        let val = serde_json::json!({
            "name": "test",
            "value": 42.5,
            "flag": true,
            "timestamp": 1700000000.0
        });
        
        assert_eq!(extract_string(&val, &["name", "other"]), Some("test".to_string()));
        assert_eq!(extract_f64(&val, &["value", "other"]), Some(42.5));
        assert_eq!(extract_bool(&val, &["flag", "other"]), Some(true));
        assert_eq!(extract_timestamp(&val, &["timestamp"]), Some(1700000000.0));
        
        // Test fallback
        assert_eq!(extract_string(&val, &["missing", "name"]), Some("test".to_string()));
        assert_eq!(extract_string(&val, &["missing"]), None);
    }
}
