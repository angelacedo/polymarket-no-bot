use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::RwLock;
use tokio::sync::mpsc;
use tracing::{debug, info};

use crate::config::BotConfig;
use crate::exchange::{BookCache, ExchangeHub};
use crate::storage::Storage;
use crate::strategy::scan_market;
use crate::types::{BookUpdate, MarketMeta, TradeSignal};

const REFRESH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

pub struct StrategyEngine {
    config: BotConfig,
    hub: Arc<ExchangeHub>,
    storage: Storage,
    markets: HashMap<String, MarketMeta>,
    last_refresh: Option<Instant>,
    cached_token_ids: Vec<String>,
}

impl StrategyEngine {
    pub fn new(config: BotConfig, hub: Arc<ExchangeHub>, storage: Storage) -> Self {
        Self {
            config,
            hub,
            storage,
            markets: HashMap::new(),
            last_refresh: None,
            cached_token_ids: Vec::new(),
        }
    }

    pub async fn refresh_markets(&mut self) -> anyhow::Result<Vec<String>> {
        if let Some(last) = self.last_refresh {
            if last.elapsed() < REFRESH_CACHE_TTL {
                debug!("market universe cache hit, skipping refresh");
                return Ok(self.cached_token_ids.clone());
            }
        }

        let limit = self.config.exchange.market_discovery_limit;
        let markets = self.hub.discover_markets(limit).await?;
        let mut token_ids = Vec::new();
        for m in markets {
            self.storage.upsert_market_cache(
                &m.condition_id,
                &m.question,
                &m.category,
                &m.underlying,
                m.end_date,
                &m.yes_token_id,
                &m.no_token_id,
            )?;
            token_ids.push(m.no_token_id.clone());
            self.markets.insert(m.condition_id.clone(), m);
        }

        token_ids.sort();
        token_ids.dedup();

        self.cached_token_ids = token_ids.clone();
        self.last_refresh = Some(Instant::now());

        info!(count = self.markets.len(), tokens = token_ids.len(), "market universe refreshed");
        Ok(token_ids)
    }

    pub fn scan_all(&self, cache: &BookCache) -> Vec<TradeSignal> {
        self.markets
            .values()
            .filter_map(|m| scan_market(&self.config, cache, m))
            .collect()
    }

    pub fn on_book_update(&self, update: &BookUpdate, cache: &BookCache) -> Option<TradeSignal> {
        let market = self
            .markets
            .values()
            .find(|m| m.no_token_id == update.asset_id)?;
        scan_market(&self.config, cache, market)
    }

    pub fn market_for_condition(&self, condition_id: &str) -> Option<&MarketMeta> {
        self.markets.get(condition_id)
    }

    pub fn markets(&self) -> &HashMap<String, MarketMeta> {
        &self.markets
    }
}

pub async fn run_strategy_loop(
    mut engine: StrategyEngine,
    mut book_rx: mpsc::Receiver<BookUpdate>,
    mut copy_rx: mpsc::Receiver<TradeSignal>,
    signal_tx: mpsc::Sender<TradeSignal>,
    scan_interval_secs: u64,
    markets_shared: Option<Arc<RwLock<HashMap<String, MarketMeta>>>>,
) {
    let cache = engine.hub.book_cache.clone();
    let mut scan_tick = tokio::time::interval(std::time::Duration::from_secs(scan_interval_secs));

    loop {
        tokio::select! {
            _ = scan_tick.tick() => {
                if let Ok(_tokens) = engine.refresh_markets().await {
                    if let Some(ref shared) = markets_shared {
                        *shared.write() = engine.markets().clone();
                    }
                    debug!(count = engine.markets().len(), "scheduled market scan");
                }
                for sig in engine.scan_all(&cache) {
                    let _ = signal_tx.send(sig).await;
                }
            }
            Some(update) = book_rx.recv() => {
                cache.update(update.clone());
                if let Some(sig) = engine.on_book_update(&update, &cache) {
                    let _ = signal_tx.send(sig).await;
                }
            }
            Some(sig) = copy_rx.recv() => {
                let _ = signal_tx.send(sig).await;
            }
            else => break,
        }
    }
}
