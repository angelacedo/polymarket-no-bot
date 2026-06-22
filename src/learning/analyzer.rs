use anyhow::Result;
use chrono::Utc;
use tracing::info;

use super::rules::{aggregate_trades, ExpiryBucket, PriceBucket};
use crate::config::BotConfig;
use crate::storage::Storage;
use crate::types::TuningAuditRecord;

pub struct AutoTuner {
    config: BotConfig,
    storage: Storage,
}

impl AutoTuner {
    pub fn new(config: &BotConfig, storage: Storage) -> Self {
        Self {
            config: config.clone(),
            storage,
        }
    }

    pub fn run_analysis(&mut self) -> Result<Vec<TuningAuditRecord>> {
        let trades = self.storage.trades_for_analysis()?;
        let resolved: Vec<_> = trades
            .into_iter()
            .filter(|t| t.realized_pnl.is_some())
            .collect();

        if resolved.len() < self.config.learning.min_sample_size as usize {
            return Ok(vec![]);
        }

        let (price_stats, expiry_stats) = aggregate_trades(&resolved);
        let mut audits = Vec::new();
        let threshold = self.config.learning.win_rate_threshold;

        for (bucket, stats) in &price_stats {
            if stats.count < self.config.learning.min_sample_size {
                continue;
            }
            if stats.win_rate() >= threshold {
                continue;
            }
            match bucket {
                PriceBucket::B95_99 => {
                    if let Some(audit) = self.shrink_max_price(stats)? {
                        audits.push(audit);
                    }
                }
                PriceBucket::B75_80 => {
                    if let Some(audit) = self.raise_min_price(stats)? {
                        audits.push(audit);
                    }
                }
                _ => {}
            }
        }

        for (bucket, stats) in &expiry_stats {
            if stats.count < self.config.learning.min_sample_size {
                continue;
            }
            if *bucket == ExpiryBucket::Under7 && stats.win_rate() < threshold {
                if let Some(audit) = self.increase_min_expiry(stats)? {
                    audits.push(audit);
                }
            }
        }

        for audit in &audits {
            self.storage.insert_tuning_audit(audit)?;
        }

        Ok(audits)
    }

    fn shrink_max_price(&mut self, stats: &super::rules::BucketStats) -> Result<Option<TuningAuditRecord>> {
        let bounds = &self.config.learning.bounds;
        let current = self
            .config
            .effective
            .allowed_price_range_no
            .unwrap_or(self.config.risk.allowed_price_range_no);
        let new_max = (current[1] - 0.01).clamp(bounds.max_price_no_min, bounds.max_price_no_max);
        if (new_max - current[1]).abs() < 1e-9 {
            return Ok(None);
        }
        let old = format!("{:.4}", current[1]);
        let new_range = [current[0], new_max];
        self.config.effective.allowed_price_range_no = Some(new_range);
        info!(old = %old, new = %new_max, wr = stats.win_rate(), "shrinking max NO price");
        Ok(Some(TuningAuditRecord {
            ts: Utc::now(),
            parameter: "allowed_price_range_no.max".into(),
            old_value: old,
            new_value: format!("{new_max:.4}"),
            reason: format!(
                "price bucket win rate {:.2} below threshold {:.2}",
                stats.win_rate(),
                self.config.learning.win_rate_threshold
            ),
        }))
    }

    fn raise_min_price(&mut self, stats: &super::rules::BucketStats) -> Result<Option<TuningAuditRecord>> {
        let bounds = &self.config.learning.bounds;
        let current = self
            .config
            .effective
            .allowed_price_range_no
            .unwrap_or(self.config.risk.allowed_price_range_no);
        let new_min = (current[0] + 0.01).clamp(bounds.min_price_no_min, bounds.min_price_no_max);
        if (new_min - current[0]).abs() < 1e-9 {
            return Ok(None);
        }
        let old = format!("{:.4}", current[0]);
        let new_range = [new_min, current[1]];
        self.config.effective.allowed_price_range_no = Some(new_range);
        Ok(Some(TuningAuditRecord {
            ts: Utc::now(),
            parameter: "allowed_price_range_no.min".into(),
            old_value: old,
            new_value: format!("{new_min:.4}"),
            reason: format!("low bucket win rate {:.2}", stats.win_rate()),
        }))
    }

    fn increase_min_expiry(&mut self, stats: &super::rules::BucketStats) -> Result<Option<TuningAuditRecord>> {
        let bounds = &self.config.learning.bounds;
        let current = self.config.effective_min_time_to_expiry_days();
        let new_val = (current + 1).clamp(bounds.min_time_to_expiry_days_min, bounds.min_time_to_expiry_days_max);
        if new_val == current {
            return Ok(None);
        }
        self.config.effective.min_time_to_expiry_days = Some(new_val);
        Ok(Some(TuningAuditRecord {
            ts: Utc::now(),
            parameter: "min_time_to_expiry_days".into(),
            old_value: current.to_string(),
            new_value: new_val.to_string(),
            reason: format!("short-dated bucket win rate {:.2}", stats.win_rate()),
        }))
    }

    pub fn into_config(self) -> BotConfig {
        self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::*;
    use crate::types::{ExecutionMode, Side, TradeRecord};
    use std::path::Path;

    fn many_losses_storage() -> Storage {
        let s = Storage::in_memory().unwrap();
        for _ in 0..35 {
            s.insert_trade(&TradeRecord {
                id: None,
                ts: Utc::now(),
                mode: ExecutionMode::Paper,
                market_id: "m".into(),
                category: "crypto".into(),
                underlying: "BTC".into(),
                expiry: Utc::now() + chrono::Duration::days(3),
                side: Side::No,
                entry_price: 0.97,
                size_shares: 10.0,
                source: "strategy".into(),
                copy_wallet: None,
                exit_price: Some(0.0),
                realized_pnl: Some(-1.0),
            })
            .unwrap();
        }
        s
    }

    #[test]
    fn tuner_shrinks_high_no_bucket() {
        let base = BotConfig::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("config/sample.toml")).unwrap();
        let storage = many_losses_storage();
        let mut tuner = AutoTuner::new(&base, storage);
        let changes = tuner.run_analysis().unwrap();
        assert!(!changes.is_empty());
    }
}
