use std::collections::HashMap;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    Paper,
    Live,
}

impl std::fmt::Display for ExecutionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Paper => write!(f, "paper"),
            Self::Live => write!(f, "live"),
        }
    }
}

impl Default for ExecutionMode {
    fn default() -> Self {
        Self::Paper
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalSource {
    Strategy,
    Copy { wallet: String },
}

impl SignalSource {
    pub fn copy_wallet(&self) -> Option<&str> {
        match self {
            Self::Strategy => None,
            Self::Copy { wallet } => Some(wallet),
        }
    }

    pub fn as_db_str(&self) -> String {
        match self {
            Self::Strategy => "strategy".to_string(),
            Self::Copy { wallet } => format!("copy:{wallet}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketMeta {
    pub condition_id: String,
    pub question: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub category: String,
    pub underlying: String,
    pub end_date: DateTime<Utc>,
    pub enable_order_book: bool,
    pub liquidity_usd: f64,
}

#[derive(Debug, Clone)]
pub struct BookLevel {
    pub price: f64,
    pub size: f64,
}

#[derive(Debug, Clone)]
pub struct BookUpdate {
    pub asset_id: String,
    pub bids: Vec<BookLevel>,
    pub asks: Vec<BookLevel>,
    pub received_at: Instant,
}

impl BookUpdate {
    pub fn best_ask(&self) -> Option<f64> {
        self.asks.iter().map(|l| l.price).reduce(f64::min)
    }

    pub fn best_bid(&self) -> Option<f64> {
        self.bids.iter().map(|l| l.price).reduce(f64::max)
    }

    pub fn depth_usd_within_band(&self, side_ask: bool, band: f64) -> f64 {
        let levels = if side_ask { &self.asks } else { &self.bids };
        if levels.is_empty() {
            return 0.0;
        }
        let best = if side_ask {
            levels.iter().map(|l| l.price).fold(f64::INFINITY, f64::min)
        } else {
            levels.iter().map(|l| l.price).fold(f64::NEG_INFINITY, f64::max)
        };
        levels
            .iter()
            .filter(|l| {
                if side_ask {
                    l.price <= best + band
                } else {
                    l.price >= best - band
                }
            })
            .map(|l| l.price * l.size)
            .sum()
    }
}

#[derive(Debug, Clone)]
pub struct TradeSignal {
    pub market: MarketMeta,
    pub side: Side,
    pub entry_price: f64,
    pub suggested_size_usd: f64,
    pub source: SignalSource,
    pub signal_ts: Instant,
}

#[derive(Debug, Clone)]
pub struct OrderIntent {
    pub market: MarketMeta,
    pub side: Side,
    pub token_id: String,
    pub limit_price: f64,
    pub size_shares: f64,
    pub notional_usd: f64,
    pub source: SignalSource,
    pub signal_ts: Instant,
    pub reducing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub client_order_id: String,
    pub token_id: String,
    pub condition_id: String,
    pub side: Side,
    pub limit_price: f64,
    pub size_shares: f64,
    pub mode: ExecutionMode,
    pub source: String,
    pub category: String,
    pub underlying: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderResult {
    pub order_id: String,
    pub client_order_id: String,
    pub filled_shares: f64,
    pub avg_fill_price: f64,
    pub status: OrderStatus,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    Filled,
    PartiallyFilled,
    Rejected,
    Pending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub condition_id: String,
    pub token_id: String,
    pub side: Side,
    pub size_shares: f64,
    pub avg_entry_price: f64,
    pub category: String,
    pub underlying: String,
    pub source: String,
    pub copy_wallet: Option<String>,
    pub mode: ExecutionMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Balances {
    pub usdc_available: f64,
    pub usdc_locked: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExposureSnapshot {
    pub total_invested_usd: f64,
    pub by_market: HashMap<String, f64>,
    pub by_category: HashMap<String, f64>,
    pub by_asset: HashMap<String, f64>,
    pub by_wallet: HashMap<String, f64>,
    pub open_position_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub id: Option<i64>,
    pub ts: DateTime<Utc>,
    pub mode: ExecutionMode,
    pub market_id: String,
    pub category: String,
    pub underlying: String,
    pub expiry: DateTime<Utc>,
    pub side: Side,
    pub entry_price: f64,
    pub size_shares: f64,
    pub source: String,
    pub copy_wallet: Option<String>,
    pub exit_price: Option<f64>,
    pub realized_pnl: Option<f64>,
}

#[derive(Debug, Clone)]
pub enum RiskDecision {
    Approved { size_shares: f64, notional_usd: f64 },
    Downsized { size_shares: f64, notional_usd: f64, reason: String },
    Rejected { reason: String },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PnlSnapshot {
    pub realized_pnl: f64,
    pub unrealized_pnl: f64,
    pub equity: f64,
    pub peak_equity: f64,
    pub daily_pnl: f64,
    pub drawdown_fraction: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CircuitBreakerState {
    pub live_disabled: bool,
    pub block_new_entries: bool,
    pub block_new_entries_until: Option<DateTime<Utc>>,
    pub last_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletTradeEvent {
    pub wallet: String,
    pub asset_id: String,
    pub condition_id: String,
    pub side: Side,
    pub price: f64,
    pub size_usd: f64,
    pub tx_hash: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuningAuditRecord {
    pub ts: DateTime<Utc>,
    pub parameter: String,
    pub old_value: String,
    pub new_value: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct LatencyTracker {
    pub decision_to_order_ms: Vec<u64>,
    pub order_to_fill_ms: Vec<u64>,
}

impl LatencyTracker {
    pub fn record_decision_to_order(&mut self, ms: u64) {
        self.decision_to_order_ms.push(ms);
        if self.decision_to_order_ms.len() > 10_000 {
            self.decision_to_order_ms.remove(0);
        }
    }

    pub fn record_order_to_fill(&mut self, ms: u64) {
        self.order_to_fill_ms.push(ms);
        if self.order_to_fill_ms.len() > 10_000 {
            self.order_to_fill_ms.remove(0);
        }
    }

    pub fn percentile(values: &[u64], p: f64) -> u64 {
        if values.is_empty() {
            return 0;
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    }
}
