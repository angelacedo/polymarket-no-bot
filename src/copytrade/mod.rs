use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{BotConfig, CopyWalletConfig};
use crate::exchange::ExchangeHub;
use crate::strategy::{apply_filters, signal_from_copy};
use crate::types::{MarketMeta, Side, TradeSignal, WalletTradeEvent};
use crate::wallets::fetch_candidate_wallets;

pub struct CopyTradeMonitor {
    config: BotConfig,
    hub: Arc<ExchangeHub>,
    seen: HashSet<String>,
    markets: HashMap<String, MarketMeta>,
    last_market_refresh: Option<Instant>,
    candidate_wallets: Vec<String>,
    last_wallet_discovery: Option<Instant>,
}

impl CopyTradeMonitor {
    pub fn new(config: BotConfig, hub: Arc<ExchangeHub>) -> Self {
        Self {
            config,
            hub,
            seen: HashSet::new(),
            markets: HashMap::new(),
            last_market_refresh: None,
            candidate_wallets: Vec::new(),
            last_wallet_discovery: None,
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
            Ok(wallets) => {
                let max = self.config.copytrade.max_candidate_wallets as usize;
                self.candidate_wallets = wallets.into_iter().take(max).collect();
                self.last_wallet_discovery = Some(Instant::now());
                info!(
                    count = self.candidate_wallets.len(),
                    "copytrade: discovered {} candidate wallets",
                    self.candidate_wallets.len()
                );
                for (i, addr) in self.candidate_wallets.iter().enumerate() {
                    info!(rank = i + 1, wallet = %addr, "candidate wallet");
                }
            }
            Err(e) => {
                warn!(error = %e, "copytrade: failed to discover candidate wallets");
            }
        }
    }

    fn wallet_allowed(&self, wallet_cfg: &CopyWalletConfig, category: &str) -> bool {
        if wallet_cfg
            .blocked_categories
            .iter()
            .any(|c| c.eq_ignore_ascii_case(category))
        {
            return false;
        }
        if wallet_cfg.allowed_categories.is_empty() {
            return true;
        }
        wallet_cfg
            .allowed_categories
            .iter()
            .any(|c| c.eq_ignore_ascii_case(category))
    }

    async fn poll_configured_wallet(
        &mut self,
        wallet_cfg: &CopyWalletConfig,
    ) -> Vec<TradeSignal> {
        let trades = match self.hub.fetch_wallet_trades(&wallet_cfg.address, 50).await {
            Ok(t) => t,
            Err(e) => {
                warn!(wallet = %wallet_cfg.address, error = %e, "copytrade poll failed");
                return vec![];
            }
        };

        trades
            .into_iter()
            .filter_map(|t| self.process_trade(t, Some(wallet_cfg)))
            .collect()
    }

    async fn poll_candidate_wallet(&mut self, address: &str) -> Vec<TradeSignal> {
        let trades = match self.hub.fetch_wallet_trades(address, 50).await {
            Ok(t) => t,
            Err(e) => {
                warn!(wallet = %address, error = %e, "copytrade candidate poll failed");
                return vec![];
            }
        };

        trades
            .into_iter()
            .filter_map(|t| self.process_trade(t, None))
            .collect()
    }

    fn process_trade(
        &mut self,
        trade: WalletTradeEvent,
        wallet_cfg: Option<&CopyWalletConfig>,
    ) -> Option<TradeSignal> {
        let dedup_key = format!("{}:{}:{}", trade.tx_hash, trade.asset_id, trade.timestamp);
        if !self.seen.insert(dedup_key) {
            return None;
        }

        // INVARIANT: force all copytrade signals to NO side
        if trade.side != Side::No {
            debug!(
                "copytrade signal overridden to NO (original was {:?}, wallet={}, market={})",
                trade.side, trade.wallet, trade.condition_id
            );
        }

        // Apply wallet-specific min_trade_size if configured
        let min_size = wallet_cfg.map(|w| w.min_trade_size_usd).unwrap_or(25.0);
        if trade.size_usd < min_size {
            return None;
        }

        let market = self.markets
            .values()
            .find(|m| m.no_token_id == trade.asset_id || m.condition_id == trade.condition_id)?;

        // Check category filters if wallet config exists
        if let Some(cfg) = wallet_cfg {
            if !self.wallet_allowed(cfg, &market.category) {
                return None;
            }
        }

        let no_ask = trade.price;
        if apply_filters(&self.config, market, no_ask, market.liquidity_usd).is_err() {
            return None;
        }

        // Calculate stake: use wallet config scale or default
        let (scale, max_exposure) = wallet_cfg
            .map(|w| (w.scale_factor, w.max_daily_exposure_usd))
            .unwrap_or((0.1, 500.0));
        let stake = (trade.size_usd * scale).min(max_exposure);

        debug!(
            wallet = %trade.wallet,
            market = %market.condition_id,
            stake,
            "copytrade signal"
        );

        Some(signal_from_copy(market.clone(), no_ask, stake, &trade.wallet))
    }
}

pub async fn run_copytrade_loop(
    config: BotConfig,
    hub: Arc<ExchangeHub>,
    _markets: Arc<parking_lot::RwLock<HashMap<String, MarketMeta>>>,
    signal_tx: mpsc::Sender<TradeSignal>,
) {
    let mut monitor = CopyTradeMonitor::new(config.clone(), hub);
    let interval = std::time::Duration::from_millis(config.copytrade.poll_interval_ms);

    // Initial wallet discovery
    if config.copytrade.auto_discover_wallets {
        monitor.refresh_candidate_wallets().await;
    }

    loop {
        // Refresh markets periodically
        monitor.refresh_markets().await;

        // Refresh candidate wallets periodically
        monitor.refresh_candidate_wallets().await;

        // Poll configured wallets
        let wallets = config.copytrade.wallets.clone();
        for wallet_cfg in &wallets {
            let signals = monitor.poll_configured_wallet(wallet_cfg).await;
            for sig in signals {
                let _ = signal_tx.send(sig).await;
            }
        }

        // Poll candidate wallets
        let candidates = monitor.candidate_wallets.clone();
        for address in &candidates {
            let signals = monitor.poll_candidate_wallet(address).await;
            for sig in signals {
                let _ = signal_tx.send(sig).await;
            }
        }

        tokio::time::sleep(interval).await;
    }
}
