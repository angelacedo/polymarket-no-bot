use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Candidate wallet from Polymarket leaderboard
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateWallet {
    pub address: String,
    pub proxy_wallet: Option<String>,
    pub username: Option<String>,
    pub profit: f64,
    pub volume: f64,
    pub markets_traded: u32,
    pub win_rate: f64,
    pub positions: Option<u32>,
    pub rank: Option<u32>,
}

/// Filters for short-NO strategy wallet selection
const MIN_WIN_RATE: f64 = 0.55;
const MIN_MARKETS_TRADED: u32 = 30;
const MIN_VOLUME_USD: f64 = 5000.0;
const MIN_PROFIT_USD: f64 = 0.0;
const MAX_PAGES: u32 = 5;
const PAGE_SIZE: u32 = 100;

/// Fetch candidate wallets from Polymarket official API
pub async fn fetch_candidate_wallets() -> Result<Vec<String>> {
    let client = Client::builder()
        .user_agent("polymarket-no-bot/0.1")
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")?;

    let mut all_wallets: Vec<CandidateWallet> = Vec::new();

    // Fetch from Polymarket Data API leaderboard (paginated)
    for page in 0..MAX_PAGES {
        let offset = page * PAGE_SIZE;
        match fetch_leaderboard_page(&client, offset, PAGE_SIZE).await {
            Ok(wallets) => {
                let count = wallets.len();
                info!(
                    page = page + 1,
                    count, "fetched wallets from Polymarket leaderboard"
                );
                if wallets.is_empty() {
                    break;
                }
                all_wallets.extend(wallets);
            }
            Err(e) => {
                warn!(page = page + 1, error = %e, "failed to fetch leaderboard page");
                break;
            }
        }
    }

    info!(
        total = all_wallets.len(),
        "total candidate wallets before filtering"
    );

    // Apply filters for short-NO strategy
    let filtered: Vec<CandidateWallet> = all_wallets
        .into_iter()
        .filter(|w| passes_filters(w))
        .collect();

    info!(
        passed = filtered.len(),
        "wallets passing filters"
    );

    // Sort by composite score: win_rate * log(volume) * profit_factor
    let mut sorted = filtered;
    sorted.sort_by(|a, b| {
        let score_a = compute_score(a);
        let score_b = compute_score(b);
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Log top 10
    let top10: Vec<_> = sorted.iter().take(10).collect();
    for (i, w) in top10.iter().enumerate() {
        info!(
            rank = i + 1,
            wallet = %w.address,
            username = ?w.username,
            profit = w.profit,
            volume = w.volume,
            win_rate = format!("{:.1}%", w.win_rate * 100.0),
            markets = w.markets_traded,
            "top candidate wallet"
        );
    }

    // Return addresses in lowercase
    Ok(sorted.into_iter().map(|w| w.address.to_lowercase()).collect())
}

/// Fetch a single page from Polymarket leaderboard API
async fn fetch_leaderboard_page(
    client: &Client,
    offset: u32,
    limit: u32,
) -> Result<Vec<CandidateWallet>> {
    // Polymarket Data API leaderboard endpoint
    let url = format!(
        "https://data-api.polymarket.com/leaderboard?window=all&limit={}&offset={}",
        limit, offset
    );

    let resp = client
        .get(&url)
        .send()
        .await
        .context("fetching Polymarket leaderboard")?;

    if !resp.status().is_success() {
        anyhow::bail!("Polymarket API returned status {}", resp.status());
    }

    let entries: Vec<LeaderboardEntry> = resp
        .json()
        .await
        .context("parsing Polymarket leaderboard JSON")?;

    Ok(entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| CandidateWallet {
            address: e.proxy_wallet.clone().unwrap_or_else(|| e.address.clone()),
            proxy_wallet: e.proxy_wallet.clone(),
            username: e.username.clone(),
            profit: e.profit.unwrap_or(0.0),
            volume: e.volume.unwrap_or(0.0),
            markets_traded: e.markets_traded.unwrap_or(0),
            win_rate: e.win_rate.unwrap_or(0.0),
            positions: e.positions,
            rank: Some(offset + i as u32 + 1),
        })
        .collect())
}

/// Polymarket leaderboard API response entry
#[derive(Debug, Deserialize)]
struct LeaderboardEntry {
    #[serde(alias = "proxyWallet")]
    proxy_wallet: Option<String>,
    address: String,
    username: Option<String>,
    profit: Option<f64>,
    volume: Option<f64>,
    #[serde(alias = "marketsTraded")]
    markets_traded: Option<u32>,
    #[serde(alias = "winRate")]
    win_rate: Option<f64>,
    positions: Option<u32>,
}

/// Check if a wallet passes filters for short-NO strategy
fn passes_filters(w: &CandidateWallet) -> bool {
    // Win rate must be >= 55%
    if w.win_rate < MIN_WIN_RATE {
        debug!(
            wallet = %w.address,
            win_rate = w.win_rate,
            "rejecting: low win rate"
        );
        return false;
    }

    // Must have traded in enough markets
    if w.markets_traded < MIN_MARKETS_TRADED {
        debug!(
            wallet = %w.address,
            markets = w.markets_traded,
            "rejecting: insufficient markets traded"
        );
        return false;
    }

    // Must have sufficient volume (not a small trader)
    if w.volume < MIN_VOLUME_USD {
        debug!(
            wallet = %w.address,
            volume = w.volume,
            "rejecting: low volume"
        );
        return false;
    }

    // Must be profitable
    if w.profit < MIN_PROFIT_USD {
        debug!(
            wallet = %w.address,
            profit = w.profit,
            "rejecting: not profitable"
        );
        return false;
    }

    true
}

/// Compute composite score for ranking wallets
/// Higher is better: combines win rate, volume (log scale), and profit
fn compute_score(w: &CandidateWallet) -> f64 {
    let win_factor = w.win_rate.powi(2); // Square to emphasize high win rates
    let volume_factor = (w.volume / 1000.0).ln().max(0.0); // Log scale for volume
    let profit_factor = (w.profit / 1000.0).sqrt().max(0.0); // Sqrt for diminishing returns

    win_factor * volume_factor * (1.0 + profit_factor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_wallet(
        win_rate: f64,
        markets: u32,
        volume: f64,
        profit: f64,
    ) -> CandidateWallet {
        CandidateWallet {
            address: "0x1234567890123456789012345678901234567890".to_string(),
            proxy_wallet: None,
            username: Some("test_trader".to_string()),
            profit,
            volume,
            markets_traded: markets,
            win_rate,
            positions: Some(10),
            rank: Some(1),
        }
    }

    #[test]
    fn test_passes_filters_good_wallet() {
        let wallet = sample_wallet(0.65, 100, 50000.0, 5000.0);
        assert!(passes_filters(&wallet));
    }

    #[test]
    fn test_rejects_low_win_rate() {
        let wallet = sample_wallet(0.45, 100, 50000.0, 5000.0);
        assert!(!passes_filters(&wallet));
    }

    #[test]
    fn test_rejects_insufficient_markets() {
        let wallet = sample_wallet(0.65, 10, 50000.0, 5000.0);
        assert!(!passes_filters(&wallet));
    }

    #[test]
    fn test_rejects_low_volume() {
        let wallet = sample_wallet(0.65, 100, 1000.0, 5000.0);
        assert!(!passes_filters(&wallet));
    }

    #[test]
    fn test_rejects_negative_profit() {
        let wallet = sample_wallet(0.65, 100, 50000.0, -1000.0);
        assert!(!passes_filters(&wallet));
    }

    #[test]
    fn test_compute_score_ordering() {
        let high_wr = sample_wallet(0.80, 100, 50000.0, 10000.0);
        let low_wr = sample_wallet(0.55, 100, 50000.0, 10000.0);
        assert!(compute_score(&high_wr) > compute_score(&low_wr));

        let high_vol = sample_wallet(0.65, 100, 100000.0, 10000.0);
        let low_vol = sample_wallet(0.65, 100, 10000.0, 10000.0);
        assert!(compute_score(&high_vol) > compute_score(&low_vol));
    }

    #[test]
    fn test_boundary_conditions() {
        // Exactly at minimum thresholds should pass
        let boundary = sample_wallet(
            MIN_WIN_RATE,
            MIN_MARKETS_TRADED,
            MIN_VOLUME_USD,
            MIN_PROFIT_USD,
        );
        assert!(passes_filters(&boundary));

        // Just below minimums should fail
        let below_wr = sample_wallet(
            MIN_WIN_RATE - 0.01,
            MIN_MARKETS_TRADED,
            MIN_VOLUME_USD,
            MIN_PROFIT_USD,
        );
        assert!(!passes_filters(&below_wr));
    }
}
