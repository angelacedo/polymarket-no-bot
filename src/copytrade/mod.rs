use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::BotConfig;
use crate::exchange::ExchangeHub;
use crate::storage::Storage;
use crate::strategy::{apply_filters, signal_from_copy};
use crate::types::{MarketMeta, Side, TradeSignal, WalletRecord, WalletTradeEvent};
use crate::wallets::{evaluate_wallet, fetch_candidate_wallets, fetch_wallet_metrics};

pub struct CopyTradeMonitor {
    config: BotConfig,
    hub: Arc<ExchangeHub>,
    storage: Storage,
    seen: HashSet<String>,
    markets: HashMap<String, MarketMeta>,
    last_market_refresh: Option<Instant>,
    last_wallet_discovery: Option<Instant>,
    last_wallet_db_refresh: Option<Instant>,
    last_wallet_evaluation: Option<Instant>,
    db_wallets: Vec<WalletRecord>,
}

impl CopyTradeMonitor {
    pub fn new(config: BotConfig, hub: Arc<ExchangeHub>, storage: Storage) -> Self {
        Self {
            config,
            hub,
            storage,
            seen: HashSet::new(),
            markets: HashMap::new(),
            last_market_refresh: None,
            last_wallet_discovery: None,
            last_wallet_db_refresh: None,
            last_wallet_evaluation: None,
            db_wallets: Vec::new(),
        }
    }

    // NOTE: Config wallets are only migrated ONCE (first run).
    // To add/remove wallets after first run, use the Dashboard UI
    // or POST/DELETE /api/wallets — changes to [copytrade.wallets] in
    // TOML will be IGNORED after first run.
    // To force a fresh migration: DELETE FROM migration_flags WHERE key='config_wallets_migrated';
    fn migrate_config_wallets(&self) {
        // Check if migration already done
        match self.storage.migration_flag_exists("config_wallets_migrated") {
            Ok(true) => {
                info!("config wallets already migrated, skipping");
                return;
            }
            Ok(false) => {
                // Proceed with migration
            }
            Err(e) => {
                warn!(error = %e, "failed to check migration flag, proceeding with migration");
            }
        }

        // Migrate wallets from config
        let config_wallets = &self.config.copytrade.wallets;
        if config_wallets.is_empty() {
            info!("no config wallets to migrate");
            // Mark as migrated even if empty to avoid retrying
            let _ = self.storage.set_migration_flag("config_wallets_migrated", "true");
            return;
        }

        info!(count = config_wallets.len(), "migrating {} config wallets to DB for the first time", config_wallets.len());
        for w in config_wallets {
            let record = WalletRecord {
                address: w.address.to_lowercase(),
                label: None,
                scale_factor: w.scale_factor,
                max_daily_exposure_usd: w.max_daily_exposure_usd,
                min_trade_size_usd: w.min_trade_size_usd,
                allowed_categories: w.allowed_categories.clone(),
                blocked_categories: w.blocked_categories.clone(),
                source: "manual".to_string(),
                enabled: true,
                created_at: Utc::now(),
            };
            if let Err(e) = self.storage.add_wallet(&record) {
                warn!(wallet = %w.address, error = %e, "failed to migrate wallet");
            }
        }

        // Mark migration as done
        if let Err(e) = self.storage.set_migration_flag("config_wallets_migrated", "true") {
            warn!(error = %e, "failed to set migration flag");
        }
    }

    async fn refresh_markets(&mut self) {
        let ttl = std::time::Duration::from_secs(
            self.config.strategy.scan_interval_secs.max(60),
        );
        if let Some(last) = self.last_market_refresh {
            if last.elapsed() < ttl {
                return;
            }
        }

        let max_expiry = self.config.effective_max_time_to_expiry_days();
        let limit = self.config.exchange.market_discovery_limit;
        match self.hub.discover_markets(limit, max_expiry).await {
            Ok(markets) => {
                self.markets.clear();
                for m in markets {
                    self.markets.insert(m.condition_id.clone(), m);
                }
                self.last_market_refresh = Some(Instant::now());
                info!(count = self.markets.len(), "copytrade: refreshed markets");
            }
            Err(e) => {
                warn!(error = %e, "copytrade: failed to refresh markets");
            }
        }
    }

    /// Refresh wallets from database
    fn refresh_wallets_from_db(&mut self) {
        // Refresh every 30 seconds
        let ttl = std::time::Duration::from_secs(30);
        if let Some(last) = self.last_wallet_db_refresh {
            if last.elapsed() < ttl {
                return;
            }
        }

        match self.storage.list_enabled_wallets() {
            Ok(wallets) => {
                let count = wallets.len();
                self.db_wallets = wallets;
                self.last_wallet_db_refresh = Some(Instant::now());
                debug!(count, "copytrade: loaded wallets from database");
            }
            Err(e) => {
                warn!(error = %e, "copytrade: failed to load wallets from database");
            }
        }
    }

    /// Discover and store candidate wallets from leaderboard
    async fn refresh_candidate_wallets(&mut self) {
        if !self.config.copytrade.auto_discover_wallets {
            return;
        }

        let ttl = std::time::Duration::from_secs(
            self.config.copytrade.discovery_interval_secs,
        );
        if let Some(last) = self.last_wallet_discovery {
            if last.elapsed() < ttl {
                return;
            }
        }

        info!("copytrade: discovering candidate wallets from leaderboard...");
        match fetch_candidate_wallets().await {
            Ok(candidates) => {
                let max = self.config.copytrade.max_candidate_wallets as usize;
                let selected: Vec<_> = candidates.into_iter().take(max).collect();
                let mut added = 0;
                let mut updated = 0;

                for address in &selected {
                    let addr_lower = address.to_lowercase();
                    match self.storage.wallet_exists(&addr_lower) {
                        Ok(true) => {
                            // Wallet exists, could update if needed
                            updated += 1;
                        }
                        Ok(false) => {
                            // Add new auto-discovered wallet
                            let record = WalletRecord {
                                address: addr_lower,
                                label: None,
                                scale_factor: 0.1,
                                max_daily_exposure_usd: 200.0,
                                min_trade_size_usd: 25.0,
                                allowed_categories: vec![],
                                blocked_categories: vec!["sports".to_string()],
                                source: "auto_discovered".to_string(),
                                enabled: true,
                                created_at: Utc::now(),
                            };
                            if self.storage.add_wallet(&record).is_ok() {
                                added += 1;
                            }
                        }
                        Err(_) => {}
                    }
                }

                self.last_wallet_discovery = Some(Instant::now());
                info!(
                    added,
                    updated,
                    total = selected.len(),
                    "copytrade: auto-discovered wallets"
                );
            }
            Err(e) => {
                warn!(error = %e, "copytrade: failed to discover candidate wallets");
            }
        }
    }

    /// Evaluate manual wallets periodically and warn if underperforming
    async fn evaluate_manual_wallets(&mut self) {
        // Evaluate every discovery_interval_secs * 3 seconds
        let eval_interval = std::time::Duration::from_secs(
            self.config.copytrade.discovery_interval_secs * 3,
        );
        if let Some(last) = self.last_wallet_evaluation {
            if last.elapsed() < eval_interval {
                return;
            }
        }

        let wallets = match self.storage.get_wallets_for_evaluation() {
            Ok(w) => w,
            Err(e) => {
                warn!(error = %e, "copytrade: failed to get wallets for evaluation");
                return;
            }
        };

        if wallets.is_empty() {
            self.last_wallet_evaluation = Some(Instant::now());
            return;
        }

        info!(count = wallets.len(), "copytrade: evaluating manual wallets");

        for wallet in &wallets {
            match fetch_wallet_metrics(&wallet.address).await {
                Ok(candidate) => {
                    let evaluation = evaluate_wallet(&candidate);
                    
                    // Get current consecutive_weak_count
                    let current_count = self.storage
                        .list_wallets()
                        .ok()
                        .and_then(|ws| ws.into_iter().find(|w| w.address == wallet.address))
                        .map(|_| 0i64) // TODO: need to read consecutive_weak_count from DB
                        .unwrap_or(0);

                    let new_count = if evaluation.status == "WEAK" {
                        current_count + 1
                    } else {
                        0
                    };

                    // Update evaluation status in DB
                    if let Err(e) = self.storage.update_wallet_evaluation(
                        &wallet.address,
                        &evaluation.status,
                        new_count,
                    ) {
                        warn!(error = %e, wallet = %wallet.address, "failed to update evaluation status");
                    }

                    // Warn if underperforming for 2+ consecutive evaluations
                    if new_count >= 2 {
                        warn!(
                            wallet = %wallet.address,
                            reasons = evaluation.reasons.join(", "),
                            consecutive_weak = new_count,
                            "copytrade: manual wallet is underperforming"
                        );
                    }

                    debug!(
                        wallet = %wallet.address,
                        status = %evaluation.status,
                        score = evaluation.score,
                        "copytrade: evaluated wallet"
                    );
                }
                Err(e) => {
                    debug!(
                        wallet = %wallet.address,
                        error = %e,
                        "copytrade: could not fetch metrics for evaluation"
                    );
                }
            }
        }

        self.last_wallet_evaluation = Some(Instant::now());
    }

    fn wallet_allowed(&self, wallet: &WalletRecord, category: &str) -> bool {
        if wallet
            .blocked_categories
            .iter()
            .any(|c| c.eq_ignore_ascii_case(category))
        {
            return false;
        }
        if wallet.allowed_categories.is_empty() {
            return true;
        }
        wallet
            .allowed_categories
            .iter()
            .any(|c| c.eq_ignore_ascii_case(category))
    }

    async fn poll_wallet(&mut self, wallet: &WalletRecord) -> Vec<TradeSignal> {
        let trades = match self.hub.fetch_wallet_trades(&wallet.address, 50).await {
            Ok(t) => t,
            Err(e) => {
                warn!(wallet = %wallet.address, error = %e, "copytrade poll failed");
                return vec![];
            }
        };

        trades
            .into_iter()
            .filter_map(|t| self.process_trade(t, wallet))
            .collect()
    }

    fn process_trade(
        &mut self,
        trade: WalletTradeEvent,
        wallet: &WalletRecord,
    ) -> Option<TradeSignal> {
        let dedup_key = format!("{}:{}:{}", trade.tx_hash, trade.asset_id, trade.timestamp);
        if !self.seen.insert(dedup_key) {
            return None;
        }

        if trade.side != Side::No {
            debug!(
                "copytrade: skipping non-NO trade (side={:?}, wallet={}, market={})",
                trade.side, trade.wallet, trade.condition_id
            );
            return None;
        }

        // Apply wallet-specific min_trade_size
        if trade.size_usd < wallet.min_trade_size_usd {
            return None;
        }

        let market = self.markets
            .values()
            .find(|m| m.no_token_id == trade.asset_id || m.condition_id == trade.condition_id)?;

        // Check category filters
        if !self.wallet_allowed(wallet, &market.category) {
            return None;
        }

        let no_ask = trade.price;
        if apply_filters(&self.config, market, no_ask, market.liquidity_usd).is_err() {
            return None;
        }

        // Calculate stake based on wallet configuration
        let stake = (trade.size_usd * wallet.scale_factor).min(wallet.max_daily_exposure_usd);

        debug!(
            wallet = %wallet.address,
            label = ?wallet.label,
            market = %market.condition_id,
            stake,
            "copytrade signal"
        );

        Some(signal_from_copy(market.clone(), no_ask, stake, &wallet.address))
    }
}

pub async fn run_copytrade_loop(
    config: BotConfig,
    hub: Arc<ExchangeHub>,
    storage: Storage,
    _markets: Arc<parking_lot::RwLock<HashMap<String, MarketMeta>>>,
    signal_tx: mpsc::Sender<TradeSignal>,
) {
    let mut monitor = CopyTradeMonitor::new(config.clone(), hub, storage);
    let interval = std::time::Duration::from_millis(config.copytrade.poll_interval_ms);

    // Migrate wallets from config to DB if needed
    monitor.migrate_config_wallets();

    // Initial wallet discovery
    if config.copytrade.auto_discover_wallets {
        monitor.refresh_candidate_wallets().await;
    }

    // Load wallets from DB
    monitor.refresh_wallets_from_db();

    loop {
        // Refresh markets periodically
        monitor.refresh_markets().await;

        // Refresh wallets from DB periodically
        monitor.refresh_wallets_from_db();

        // Discover new candidate wallets periodically
        monitor.refresh_candidate_wallets().await;

        // Evaluate manual wallets periodically
        monitor.evaluate_manual_wallets().await;

        // Poll all enabled wallets from database
        let wallets = monitor.db_wallets.clone();
        for wallet in &wallets {
            let signals = monitor.poll_wallet(wallet).await;
            for sig in signals {
                let _ = signal_tx.send(sig).await;
            }
        }

        tokio::time::sleep(interval).await;
    }
}
