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
    /// Close a NO position once its mark price falls to this level (cut losers).
    #[serde(default = "default_stop_loss_price")]
    pub stop_loss_price: f64,
}

fn default_take_profit_price() -> f64 {
    // Close near full resolution value; actual resolution is handled by
    // settle_resolved_markets() which settles at exactly 1.0 or 0.0.
    0.99
}

fn default_stop_loss_price() -> f64 {
    // Very low threshold — we want to hold NO positions to resolution.
    // Only cut if price collapses dramatically (market pivoted against us).
    0.05
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
        self.risk.max_time_to_expiry_days
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
