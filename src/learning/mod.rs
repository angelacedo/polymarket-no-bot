mod analyzer;
mod rules;

pub use analyzer::AutoTuner;
pub use rules::{BucketStats, PriceBucket, ExpiryBucket};

use chrono::Utc;
use tracing::info;

use crate::config::BotConfig;
use crate::storage::Storage;
use crate::types::TuningAuditRecord;

pub async fn run_learning_loop(
    mut config: BotConfig,
    storage: Storage,
    config_tx: tokio::sync::watch::Sender<BotConfig>,
) {
    let hours = config.learning.analysis_interval_hours.max(1);
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(hours * 3600));

    loop {
        interval.tick().await;
        let mut tuner = AutoTuner::new(&config, storage.clone());
        match tuner.run_analysis() {
            Ok(changes) => {
                if !changes.is_empty() {
                    config = tuner.into_config();
                    info!(count = changes.len(), "auto-tuning applied changes");
                    let _ = config_tx.send(config.clone());
                }
            }
            Err(e) => tracing::warn!(error = %e, "learning analysis failed"),
        }
    }
}
