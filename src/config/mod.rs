use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::types::ExecutionMode;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotConfig {
    pub execution: ExecutionConfig,
    pub metrics: MetricsConfig,
    pub storage: StorageConfig,
    pub risk: RiskConfig,
    pub strategy: StrategyConfig,
    pub copytrade: CopyTradeConfig,
    pub learning: LearningConfig,
    pub exchange: ExchangeConfig,
    #[serde(default)]
    pub effective: EffectiveConfigOverlay,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EffectiveConfigOverlay {
    #[serde(default)]
    pub allowed_price_range_no: Option<[f64; 2]>,
    #[serde(default)]
    pub min_time_to_expiry_days: Option<u32>,
    #[serde(default)]
    pub max_time_to_expiry_days: Option<f64>,
    #[serde(default)]
    pub wallet_scale_factors: std::collections::HashMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionConfig {
    pub mode: ExecutionMode,
    pub baseline_latency_ms: u64,
    pub latency_jitter_ms: u64,
    /// Close a NO position once its mark price reaches this level (resolution-win proxy).
    #[serde(default = "default_take_profit_price")]
    pub take_profit_price: f64,
    /// Legacy absolute stop-loss price. Deprecated in favor of `stop_loss_fraction`.
    #[serde(default)]
    pub stop_loss_price: Option<f64>,
    /// Relative stop-loss: close if price drops more than this fraction below entry.
    /// Example: 0.20 means close if position loses 20% from entry price.
    #[serde(default = "default_stop_loss_fraction")]
    pub stop_loss_fraction: f64,
    /// Slippage tolerance for paper fills: accept prices up to this fraction above limit_price.
    /// Example: 0.05 means accept prices up to 5% above limit_price.
    #[serde(default = "default_slippage_tolerance")]
    pub slippage_tolerance: f64,
}

fn default_slippage_tolerance() -> f64 {
    0.05
}

fn default_take_profit_price() -> f64 {
    // Close near full resolution value; actual resolution is handled by
    // settle_resolved_markets() which settles at exactly 1.0 or 0.0.
    0.99
}

fn default_stop_loss_fraction() -> f64 {
    // Relative stop: close if position loses 20% from entry price.
    0.20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetricsConfig {
    pub bind_addr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub database_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskConfig {
    pub total_capital: f64,
    pub max_invested_capital_fraction: f64,
    pub max_daily_loss_fraction: f64,
    pub max_drawdown_fraction: f64,
    pub max_risk_per_trade_fraction: f64,
    pub max_notional_per_market: f64,
    pub allowed_price_range_no: [f64; 2],
    pub min_time_to_expiry_days: u32,
    /// Optional upper bound on time-to-expiry (in days, fractional allowed).
    /// When set, only markets resolving within this window are traded — used to
    /// target short-duration events. `None` means no upper bound.
    #[serde(default)]
    pub max_time_to_expiry_days: Option<f64>,
    pub max_category_risk_fraction: f64,
    pub max_asset_risk_fraction: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyConfig {
    pub scan_interval_secs: u64,
    pub min_liquidity_usd: f64,
    pub slippage_band: f64,
    pub prefer_no_momentum: bool,
    pub momentum_window_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyTradeConfig {
    pub poll_interval_ms: u64,
    #[serde(default)]
    pub wallets: Vec<CopyWalletConfig>,
    /// Enable automatic wallet discovery from Polymarket leaderboard
    #[serde(default = "default_auto_discover")]
    pub auto_discover_wallets: bool,
    /// How often to refresh the candidate wallet list (seconds)
    #[serde(default = "default_discovery_interval")]
    pub discovery_interval_secs: u64,
    /// Maximum number of auto-discovered wallets to copy
    #[serde(default = "default_max_candidates")]
    pub max_candidate_wallets: u32,
}

fn default_auto_discover() -> bool {
    true
}

fn default_discovery_interval() -> u64 {
    3600 // 1 hour
}

fn default_max_candidates() -> u32 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopyWalletConfig {
    pub address: String,
    pub tier: u8,
    pub scale_factor: f64,
    pub max_daily_exposure_usd: f64,
    pub min_trade_size_usd: f64,
    #[serde(default)]
    pub allowed_categories: Vec<String>,
    #[serde(default)]
    pub blocked_categories: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningConfig {
    pub analysis_interval_hours: u64,
    pub min_sample_size: u32,
    pub win_rate_threshold: f64,
    pub scale_adjust_pct: f64,
    pub bounds: LearningBounds,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningBounds {
    pub min_price_no_min: f64,
    pub min_price_no_max: f64,
    pub max_price_no_min: f64,
    pub max_price_no_max: f64,
    pub min_time_to_expiry_days_min: u32,
    pub min_time_to_expiry_days_max: u32,
    pub scale_factor_min: f64,
    pub scale_factor_max: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeConfig {
    pub gamma_base_url: String,
    pub data_api_base_url: String,
    pub clob_host: String,
    pub chain_id: u64,
    pub market_discovery_limit: u32,
}

impl BotConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let mut config: BotConfig = toml::from_str(&raw).context("parsing config TOML")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.risk.total_capital <= 0.0 {
            bail!("risk.total_capital must be positive");
        }
        validate_fraction(self.risk.max_invested_capital_fraction, "max_invested_capital_fraction")?;
        validate_fraction(self.risk.max_daily_loss_fraction, "max_daily_loss_fraction")?;
        validate_fraction(self.risk.max_drawdown_fraction, "max_drawdown_fraction")?;
        validate_fraction(self.risk.max_risk_per_trade_fraction, "max_risk_per_trade_fraction")?;
        validate_fraction(self.risk.max_category_risk_fraction, "max_category_risk_fraction")?;
        validate_fraction(self.risk.max_asset_risk_fraction, "max_asset_risk_fraction")?;

        let [min_p, max_p] = self.effective_price_range();
        if min_p >= max_p || min_p <= 0.0 || max_p > 1.0 {
            bail!("allowed_price_range_no must satisfy 0 < min < max <= 1");
        }
        if self.risk.max_notional_per_market <= 0.0 {
            bail!("max_notional_per_market must be positive");
        }
        // A minimum of 0 is allowed (e.g. short-duration testing); when an
        // upper bound is set it must be positive and not below the minimum.
        if let Some(max_days) = self.risk.max_time_to_expiry_days {
            if max_days <= 0.0 {
                bail!("max_time_to_expiry_days must be positive when set");
            }
            if max_days < self.effective_min_time_to_expiry_days() as f64 {
                bail!("max_time_to_expiry_days must be >= min_time_to_expiry_days");
            }
        }
        // Validate overlay values if present
        if let Some(days) = self.effective.min_time_to_expiry_days {
            if days == 0 {
                bail!("effective.min_time_to_expiry_days must be >= 1 when set via overlay");
            }
        }
        Ok(())
    }

    pub fn effective_price_range(&self) -> [f64; 2] {
        self.effective
            .allowed_price_range_no
            .unwrap_or(self.risk.allowed_price_range_no)
    }

    pub fn effective_min_time_to_expiry_days(&self) -> u32 {
        self.effective
            .min_time_to_expiry_days
            .unwrap_or(self.risk.min_time_to_expiry_days)
    }

    pub fn effective_max_time_to_expiry_days(&self) -> Option<f64> {
        self.effective.max_time_to_expiry_days.or(self.risk.max_time_to_expiry_days)
    }

    pub fn effective_wallet_scale(&self, address: &str, base: f64) -> f64 {
        self.effective
            .wallet_scale_factors
            .get(&address.to_lowercase())
            .copied()
            .unwrap_or(base)
    }

    pub fn with_mode(mut self, mode: ExecutionMode) -> Self {
        self.execution.mode = mode;
        self
    }
}

fn validate_fraction(v: f64, name: &str) -> Result<()> {
    if !(0.0..=1.0).contains(&v) {
        bail!("{name} must be between 0 and 1");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_config_validates() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config/sample.toml");
        BotConfig::load(&path).expect("sample config should validate");
    }
}
