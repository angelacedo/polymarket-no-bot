use anyhow::{Context, Result};
use reqwest::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
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
}

/// Filters for short-NO strategy wallet selection
const MIN_WIN_RATE: f64 = 0.60;
const MIN_CLOSED_MARKETS: u32 = 50;
const MAX_AVG_HOLD_DAYS: f64 = 7.0;
const MIN_DRAWDOWN: f64 = -0.30;

/// Fetch candidate wallets from Polysyncer and Struct.to
pub async fn fetch_candidate_wallets() -> Result<Vec<String>> {
    let client = Client::builder()
        .user_agent("polymarket-no-bot/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    let mut all_wallets: Vec<CandidateWallet> = Vec::new();

    // Fetch from Polysyncer
    match fetch_polysyncer_wallets(&client).await {
        Ok(wallets) => {
            info!(count = wallets.len(), "fetched wallets from Polysyncer");
            all_wallets.extend(wallets);
        }
        Err(e) => {
            warn!(error = %e, "failed to fetch Polysyncer wallets");
        }
    }

    // Fetch from Struct.to
    match fetch_struct_wallets(&client).await {
        Ok(wallets) => {
            info!(count = wallets.len(), "fetched wallets from Struct.to");
            all_wallets.extend(wallets);
        }
        Err(e) => {
            warn!(error = %e, "failed to fetch Struct.to wallets");
        }
    }

    let total = all_wallets.len();
    info!(total, "total candidate wallets before filtering");

    // Apply filters
    let filtered: Vec<CandidateWallet> = all_wallets
        .into_iter()
        .filter(|w| passes_filters(w))
        .collect();

    info!(
        total_seen = total,
        passed_filters = filtered.len(),
        "wallet discovery: filtered candidate wallets with high winrate"
    );

    // Sort by Sharpe (if available), then win_rate, then total_pnl
    let mut sorted = filtered;
    sorted.sort_by(|a, b| {
        // Prefer higher Sharpe
        let sharpe_cmp = b.sharpe.unwrap_or(f64::NEG_INFINITY)
            .partial_cmp(&a.sharpe.unwrap_or(f64::NEG_INFINITY))
            .unwrap_or(std::cmp::Ordering::Equal);
        if sharpe_cmp != std::cmp::Ordering::Equal {
            return sharpe_cmp;
        }

        // Then win_rate
        let wr_cmp = b.win_rate.partial_cmp(&a.win_rate).unwrap_or(std::cmp::Ordering::Equal);
        if wr_cmp != std::cmp::Ordering::Equal {
            return wr_cmp;
        }

        // Then total_pnl
        b.total_pnl.partial_cmp(&a.total_pnl).unwrap_or(std::cmp::Ordering::Equal)
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
            "top candidate wallet"
        );
    }

    // Return addresses in lowercase
    Ok(sorted.into_iter().map(|w| w.address.to_lowercase()).collect())
}

/// Fetch wallets from Polysyncer leaderboard
async fn fetch_polysyncer_wallets(client: &Client) -> Result<Vec<CandidateWallet>> {
    let url = "https://www.polysyncer.com/leaderboard";
    let resp = client
        .get(url)
        .send()
        .await
        .context("fetching Polysyncer leaderboard")?;

    if !resp.status().is_success() {
        anyhow::bail!("Polysyncer returned status {}", resp.status());
    }

    let html = resp.text().await.context("reading Polysyncer response")?;
    parse_polysyncer_html(&html)
}

/// Parse Polysyncer leaderboard HTML
fn parse_polysyncer_html(html: &str) -> Result<Vec<CandidateWallet>> {
    let document = Html::parse_document(html);
    let mut wallets = Vec::new();

    // Polysyncer uses table rows with trader data
    let row_selector = Selector::parse("table tbody tr, .leaderboard-row, .trader-row")
        .unwrap_or(Selector::parse("tr").unwrap());
    let cell_selector = Selector::parse("td").unwrap();

    for row in document.select(&row_selector) {
        let cells: Vec<_> = row.select(&cell_selector).collect();
        if cells.len() < 4 {
            continue;
        }

        // Try to extract wallet address
        let address = extract_address_from_row(&row, &cells);
        if address.is_none() {
            continue;
        }
        let address = address.unwrap();

        // Parse metrics from cells (indices vary by site structure)
        let win_rate = parse_cell_f64(&cells, 2).or_else(|| parse_cell_f64(&cells, 3))
            .map(|v| if v > 1.0 { v / 100.0 } else { v });
        let closed_markets = parse_cell_u32(&cells, 3).or_else(|| parse_cell_u32(&cells, 4));
        let sharpe = parse_cell_f64(&cells, 5);
        let drawdown = parse_cell_f64(&cells, 6).map(|v| if v > 0.0 { -v } else { v });
        let pnl = parse_cell_f64(&cells, 7).or_else(|| parse_cell_f64(&cells, 4));

        wallets.push(CandidateWallet {
            address,
            win_rate: win_rate.unwrap_or(0.0),
            closed_markets: closed_markets.unwrap_or(0),
            avg_hold_days: None,
            max_drawdown: drawdown,
            total_pnl: pnl.unwrap_or(0.0),
            sharpe,
            source: "polysyncer".to_string(),
        });
    }

    Ok(wallets)
}

/// Fetch wallets from Struct.to traders page
async fn fetch_struct_wallets(client: &Client) -> Result<Vec<CandidateWallet>> {
    let url = "https://explorer.struct.to/traders?platform=polymarket";
    let resp = client
        .get(url)
        .send()
        .await
        .context("fetching Struct.to traders")?;

    if !resp.status().is_success() {
        anyhow::bail!("Struct.to returned status {}", resp.status());
    }

    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if content_type.contains("application/json") {
        let json: serde_json::Value = resp.json().await?;
        parse_struct_json(&json)
    } else {
        let html = resp.text().await?;
        parse_struct_html(&html)
    }
}

/// Parse Struct.to JSON response
fn parse_struct_json(json: &serde_json::Value) -> Result<Vec<CandidateWallet>> {
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
            source: "struct_to".to_string(),
        });
    }

    Ok(wallets)
}

/// Parse Struct.to HTML
fn parse_struct_html(html: &str) -> Result<Vec<CandidateWallet>> {
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
            source: "struct_to".to_string(),
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

    true
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
    fn test_extract_address_from_url() {
        assert_eq!(
            extract_address_from_url("https://polymarket.com/profile/0x1234567890123456789012345678901234567890"),
            Some("0x1234567890123456789012345678901234567890".to_string())
        );
        assert_eq!(extract_address_from_url("https://example.com"), None);
    }
}
