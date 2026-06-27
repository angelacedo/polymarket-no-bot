use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{BotConfig, CopyWalletConfig};
use crate::exchange::ExchangeHub;
use crate::strategy::{apply_filters, signal_from_copy};
use crate::types::{Side, TradeSignal, WalletTradeEvent};

pub struct CopyTradeMonitor {
    config: BotConfig,
    hub: Arc<ExchangeHub>,
    seen: HashSet<String>,
}

impl CopyTradeMonitor {
    pub fn new(config: BotConfig, hub: Arc<ExchangeHub>) -> Self {
        Self {
            config,
            hub,
            seen: HashSet::new(),
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

    pub async fn poll_wallet(
        &mut self,
        wallet_cfg: &CopyWalletConfig,
        markets: &std::collections::HashMap<String, crate::types::MarketMeta>,
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
            .filter_map(|t| self.process_trade(wallet_cfg, t, markets))
            .collect()
    }

    fn process_trade(
        &mut self,
        wallet_cfg: &CopyWalletConfig,
        trade: WalletTradeEvent,
        markets: &std::collections::HashMap<String, crate::types::MarketMeta>,
    ) -> Option<TradeSignal> {
        let dedup_key = format!("{}:{}:{}", trade.tx_hash, trade.asset_id, trade.timestamp);
        if !self.seen.insert(dedup_key) {
            return None;
        }

        // INVARIANT: force all copytrade signals to NO side
        if trade.side != Side::No {
            debug!(
                "copytrade signal overridden to NO (original was {:?}, wallet={}, market={})",
                trade.side, wallet_cfg.address, trade.condition_id
            );
        }

        if trade.size_usd < wallet_cfg.min_trade_size_usd {
            return None;
        }

        let market = markets
            .values()
            .find(|m| m.no_token_id == trade.asset_id || m.condition_id == trade.condition_id)?;

        if !self.wallet_allowed(wallet_cfg, &market.category) {
            return None;
        }

        let no_ask = trade.price;
        if apply_filters(&self.config, market, no_ask, market.liquidity_usd).is_err() {
            return None;
        }

        let scale = self
            .config
            .effective_wallet_scale(&wallet_cfg.address, wallet_cfg.scale_factor);
        let stake = (trade.size_usd * scale).min(wallet_cfg.max_daily_exposure_usd);

        debug!(
            wallet = %wallet_cfg.address,
            market = %market.condition_id,
            stake,
            "copytrade signal"
        );

        Some(signal_from_copy(market.clone(), no_ask, stake, &wallet_cfg.address))
    }
}

pub async fn run_copytrade_loop(
    config: BotConfig,
    hub: Arc<ExchangeHub>,
    markets: Arc<parking_lot::RwLock<std::collections::HashMap<String, crate::types::MarketMeta>>>,
    signal_tx: mpsc::Sender<TradeSignal>,
) {
    let mut monitor = CopyTradeMonitor::new(config.clone(), hub);
    let interval = std::time::Duration::from_millis(config.copytrade.poll_interval_ms);

    loop {
        let wallets = config.copytrade.wallets.clone();
        if wallets.is_empty() {
            tokio::time::sleep(interval).await;
            continue;
        }
        let market_map = markets.read().clone();
        for wallet_cfg in &wallets {
            let signals = monitor.poll_wallet(wallet_cfg, &market_map).await;
            for sig in signals {
                let _ = signal_tx.send(sig).await;
            }
        }
        tokio::time::sleep(interval).await;
    }
}
