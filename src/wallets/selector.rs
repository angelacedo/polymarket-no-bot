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
        reasons.push(format!("win_rate {:.1}% < 60%", w.win_rate * 100.0));
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
            reasons.push(format!("max_drawdown {:.1}% < {:.1}%", dd * 100.0, MIN_DRAWDOWN * 100.0));
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
pub async fn fetch_wallet_metrics(address: &str) -> Result<CandidateWallet> {
    let client = Client::builder()
        .user_agent("polymarket-no-bot/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;
    
    let url = format!(
        "https://data-api.polymarket.com/activity?user={}&limit=500",
        address
    );
    
    let resp = client
        .get(&url)
        .send()
        .await
        .context("fetching wallet activity from Data API")?;
    
    if !resp.status().is_success() {
        anyhow::bail!("Data API returned status {}", resp.status());
    }
    
    let trades: Vec<serde_json::Value> = resp
        .json()
        .await
        .context("parsing Data API response")?;
    
    if trades.is_empty() {
        anyhow::bail!("no activity found for wallet {}", address);
    }
    
    // Calculate metrics from activity data
    let total_trades = trades.len() as f64;
    
    // NO trade ratio
    let no_trades: Vec<_> = trades.iter()
        .filter(|t| t.get("outcome").and_then(|v| v.as_str()) == Some("No"))
        .collect();
    let no_trade_ratio = no_trades.len() as f64 / total_trades;
    
    // Resolved trades for win rate calculation
    let resolved_trades: Vec<_> = trades.iter()
        .filter(|t| t.get("resolved").and_then(|v| v.as_bool()) == Some(true))
        .collect();
    
    let win_rate = if resolved_trades.is_empty() {
        0.0
    } else {
        let wins = resolved_trades.iter()
            .filter(|t| t.get("won").and_then(|v| v.as_bool()) == Some(true))
            .count() as f64;
        wins / resolved_trades.len() as f64
    };
    
    // NO win rate
    let resolved_no_trades: Vec<_> = no_trades.iter()
        .filter(|t| t.get("resolved").and_then(|v| v.as_bool()) == Some(true))
        .collect();
    
    let no_win_rate = if resolved_no_trades.is_empty() {
        None
    } else {
        let no_wins = resolved_no_trades.iter()
            .filter(|t| t.get("won").and_then(|v| v.as_bool()) == Some(true))
            .count() as f64;
        Some(no_wins / resolved_no_trades.len() as f64)
    };
    
    // Closed markets (unique condition_ids with resolved=true)
    let closed_markets = resolved_trades.iter()
        .filter_map(|t| t.get("condition_id").and_then(|v| v.as_str()))
        .collect::<std::collections::HashSet<_>>()
        .len() as u32;
    
    // Average hold time (in days)
    let avg_hold_days = {
        let hold_times: Vec<f64> = resolved_trades.iter()
            .filter_map(|t| {
                let created = t.get("created_at").and_then(|v| v.as_str())?;
                let resolved = t.get("resolved_at").and_then(|v| v.as_str())?;
                let created_dt = DateTime::parse_from_rfc3339(created).ok()?;
                let resolved_dt = DateTime::parse_from_rfc3339(resolved).ok()?;
                let days = (resolved_dt - created_dt).num_seconds() as f64 / 86400.0;
                Some(days)
            })
            .collect();
        
        if hold_times.is_empty() {
            None
        } else {
            Some(hold_times.iter().sum::<f64>() / hold_times.len() as f64)
        }
    };
    
    // Total PnL (sum of profit across resolved trades)
    let total_pnl = resolved_trades.iter()
        .filter_map(|t| t.get("profit").and_then(|v| v.as_f64()))
        .sum::<f64>();
    
    Ok(CandidateWallet {
        address: address.to_string(),
        win_rate,
        closed_markets,
        avg_hold_days,
        max_drawdown: None,
        total_pnl,
        sharpe: None,
        source: "data_api_direct".to_string(),
        no_trade_ratio: Some(no_trade_ratio),
        no_win_rate,
    })
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
}
