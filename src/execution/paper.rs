use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use parking_lot::RwLock;
use rand::Rng;
use tokio::time::{Duration, sleep};
use uuid::Uuid;

use crate::config::ExecutionConfig;
use crate::exchange::{BookCache, MarketResolution};
use crate::storage::Storage;
use crate::types::{
    Balances, BookLevel, ExecutionMode, OrderRequest, OrderResult, OrderStatus, Position, Side,
    TradeRecord,
};

use super::PortfolioMark;

pub struct PaperBackend {
    config: ExecutionConfig,
    initial_capital: f64,
    book_cache: BookCache,
    storage: Storage,
    state: Arc<RwLock<PaperState>>,
}

#[derive(Debug, Default)]
struct PaperState {
    usdc_available: f64,
    usdc_locked: f64,
    positions: HashMap<String, Position>,
    orders: HashMap<String, String>,
    realized_pnl: f64,
}

impl PaperBackend {
    pub fn new(
        config: ExecutionConfig,
        initial_capital: f64,
        book_cache: BookCache,
        storage: Storage,
    ) -> Self {
        Self {
            config,
            initial_capital,
            book_cache,
            storage,
            state: Arc::new(RwLock::new(PaperState {
                usdc_available: initial_capital,
                ..Default::default()
            })),
        }
    }

    fn simulate_latency(&self) -> u64 {
        let mut rng = rand::thread_rng();
        self.config.baseline_latency_ms
            + rng.gen_range(0..=self.config.latency_jitter_ms.max(1))
    }

    fn walk_book(
        asks: &[BookLevel],
        limit_price: f64,
        max_shares: f64,
    ) -> (f64, f64) {
        let mut remaining = max_shares;
        let mut filled = 0.0;
        let mut cost = 0.0;
        for level in asks {
            if level.price > limit_price {
                break;
            }
            let take = remaining.min(level.size);
            filled += take;
            cost += take * level.price;
            remaining -= take;
            if remaining <= 0.0 {
                break;
            }
        }
        let avg = if filled > 0.0 { cost / filled } else { 0.0 };
        (filled, avg)
    }

    /// Mark open positions to current book prices, settle any that hit
    /// take-profit / stop-loss, recompute exposure authoritatively from the
    /// surviving positions, and persist closing trades.
    fn settle(&self, exec_cfg: &ExecutionConfig) -> PortfolioMark {
        let mut closed_trades: Vec<TradeRecord> = Vec::new();
        let mut mark = PortfolioMark::default();

        {
            let mut state = self.state.write();
            let mut to_close: Vec<String> = Vec::new();

            for (cond_id, pos) in state.positions.iter() {
                // Use realistic exit price (best bid) for mark-to-market
                let mark_price = exit_price(&self.book_cache, &pos.token_id, pos.avg_entry_price);

                let hit_take_profit = mark_price >= exec_cfg.take_profit_price;
                let stop_threshold = pos.avg_entry_price * (1.0 - exec_cfg.stop_loss_fraction);
                let hit_stop_loss = mark_price <= stop_threshold;

                if hit_take_profit || hit_stop_loss {
                    let realized = (mark_price - pos.avg_entry_price) * pos.size_shares;
                    closed_trades.push(TradeRecord {
                        id: None,
                        ts: chrono::Utc::now(),
                        mode: ExecutionMode::Paper,
                        market_id: pos.condition_id.clone(),
                        category: pos.category.clone(),
                        underlying: pos.underlying.clone(),
                        expiry: chrono::Utc::now(),
                        side: pos.side,
                        entry_price: pos.avg_entry_price,
                        size_shares: pos.size_shares,
                        source: pos.source.clone(),
                        copy_wallet: pos.copy_wallet.clone(),
                        exit_price: Some(mark_price),
                        realized_pnl: Some(realized),
                    });
                    to_close.push(cond_id.clone());
                } else {
                    // Survivor: accumulate unrealized PnL and cost-basis exposure.
                    let cost_basis = pos.avg_entry_price * pos.size_shares;
                    mark.unrealized_pnl += (mark_price - pos.avg_entry_price) * pos.size_shares;
                    mark.exposure.total_invested_usd += cost_basis;
                    *mark.exposure.by_market.entry(pos.condition_id.clone()).or_default() +=
                        cost_basis;
                    *mark.exposure.by_category.entry(pos.category.clone()).or_default() +=
                        cost_basis;
                    *mark.exposure.by_asset.entry(pos.underlying.clone()).or_default() +=
                        cost_basis;
                    if let Some(wallet) = &pos.copy_wallet {
                        *mark.exposure.by_wallet.entry(wallet.to_lowercase()).or_default() +=
                            cost_basis;
                    }
                    mark.positions.push(pos.clone());
                }
            }

            for cond_id in to_close {
                if let Some(pos) = state.positions.remove(&cond_id) {
                    let mark_price = exit_price(&self.book_cache, &pos.token_id, pos.avg_entry_price);
                    // Return proceeds of the sale to available balance.
                    state.usdc_available += mark_price * pos.size_shares;
                    state.realized_pnl += (mark_price - pos.avg_entry_price) * pos.size_shares;
                }
            }

            mark.exposure.open_position_count = state.positions.len();
            mark.realized_pnl = state.realized_pnl;
        }

        for trade in &closed_trades {
            if let Err(e) = self.storage.insert_trade(trade) {
                tracing::warn!(error = %e, "failed to persist closing trade");
            }
        }

        mark
    }

    /// Clear in-memory positions and restore starting capital.
    pub fn reset_portfolio(&self) {
        let capital = self.initial_capital;
        let mut state = self.state.write();
        state.positions.clear();
        state.orders.clear();
        state.usdc_available = capital;
        state.usdc_locked = 0.0;
        state.realized_pnl = 0.0;
    }
}

/// Mid price for a token from the cached book, falling back to a one-sided
/// quote when only one side is present.
fn mid_price(cache: &BookCache, token_id: &str) -> Option<f64> {
    let book = cache.get(token_id)?;
    match (book.best_bid(), book.best_ask()) {
        (Some(bid), Some(ask)) => Some((bid + ask) / 2.0),
        (Some(bid), None) => Some(bid),
        (None, Some(ask)) => Some(ask),
        (None, None) => None,
    }
}

/// Best bid for exit (realistic close price for a long NO position).
/// Falls back to mid, then entry price.
fn exit_price(cache: &BookCache, token_id: &str, entry: f64) -> f64 {
    if let Some(book) = cache.get(token_id) {
        if let Some(bid) = book.best_bid() {
            return bid;
        }
        if let Some(ask) = book.best_ask() {
            return (ask + entry) / 2.0; // pessimistic mid if no bid
        }
    }
    entry
}

#[async_trait]
impl super::ExecutionBackend for PaperBackend {
    async fn place_order(&self, req: OrderRequest) -> Result<OrderResult> {
        debug_assert_eq!(req.side, Side::No, "invariant violated: non-NO order reached execution");
        if req.side != Side::No {
            tracing::error!(side = ?req.side, "INVARIANT VIOLATION: order side is not NO, rejecting");
            bail!("strategy invariant violated: only NO orders allowed");
        }

        let latency = self.simulate_latency();
        sleep(Duration::from_millis(latency)).await;
        let sent = Instant::now();

        let book = self
            .book_cache
            .get(&req.token_id)
            .context("no orderbook snapshot for token")?;

        let (filled, avg_price) = if req.side == Side::No {
            Self::walk_book(&book.asks, req.limit_price, req.size_shares)
        } else {
            Self::walk_book(
                &book
                    .bids
                    .iter()
                    .map(|b| BookLevel {
                        price: 1.0 - b.price,
                        size: b.size,
                    })
                    .collect::<Vec<_>>(),
                req.limit_price,
                req.size_shares,
            )
        };

        let order_id = Uuid::new_v4().to_string();
        let status = if filled <= 0.0 {
            OrderStatus::Rejected
        } else if filled + 1e-9 < req.size_shares {
            OrderStatus::PartiallyFilled
        } else {
            OrderStatus::Filled
        };

        let notional = filled * avg_price;
        {
            let mut state = self.state.write();
            if status == OrderStatus::Rejected {
                return Ok(OrderResult {
                    order_id,
                    client_order_id: req.client_order_id.clone(),
                    filled_shares: 0.0,
                    avg_fill_price: 0.0,
                    status,
                    latency_ms: sent.elapsed().as_millis() as u64 + latency,
                });
            }
            if notional > state.usdc_available {
                bail!("insufficient paper balance");
            }
            state.usdc_available -= notional;
            let key = req.condition_id.clone();
            let entry = state.positions.entry(key.clone()).or_insert_with(|| Position {
                condition_id: req.condition_id.clone(),
                token_id: req.token_id.clone(),
                side: req.side,
                size_shares: 0.0,
                avg_entry_price: 0.0,
                category: req.category.clone(),
                underlying: req.underlying.clone(),
                source: req.source.clone(),
                copy_wallet: None,
                mode: ExecutionMode::Paper,
            });
            let total_cost = entry.avg_entry_price * entry.size_shares + notional;
            entry.size_shares += filled;
            entry.avg_entry_price = total_cost / entry.size_shares;
            state.orders.insert(order_id.clone(), req.client_order_id.clone());
        }

        let result = OrderResult {
            order_id: order_id.clone(),
            client_order_id: req.client_order_id.clone(),
            filled_shares: filled,
            avg_fill_price: avg_price,
            status,
            latency_ms: sent.elapsed().as_millis() as u64 + latency,
        };

        self.storage
            .insert_order(&req, &result, 0)
            .context("persist paper order")?;

        Ok(result)
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        self.state.write().orders.remove(order_id);
        Ok(())
    }

    async fn open_positions(&self) -> Result<Vec<Position>> {
        Ok(self.state.read().positions.values().cloned().collect())
    }

    async fn balances(&self) -> Result<Balances> {
        let state = self.state.read();
        Ok(Balances {
            usdc_available: state.usdc_available,
            usdc_locked: state.usdc_locked,
        })
    }

    fn mode(&self) -> ExecutionMode {
        ExecutionMode::Paper
    }

    fn mark_and_settle(&self, exec_cfg: &ExecutionConfig) -> PortfolioMark {
        self.settle(exec_cfg)
    }

    fn reset_paper_portfolio(&self) {
        self.reset_portfolio();
    }

    fn settle_resolved_markets(&self, resolutions: &HashMap<String, MarketResolution>) {
        let mut closed_trades: Vec<TradeRecord> = Vec::new();
        {
            let mut state = self.state.write();
            // Only settle positions whose held token has a decisive resolved
            // price; markets we can't price are left open.
            let to_close: Vec<String> = state
                .positions
                .iter()
                .filter_map(|(cid, pos)| {
                    resolutions
                        .get(cid)
                        .and_then(|r| r.price_for(&pos.token_id))
                        .map(|_| cid.clone())
                })
                .collect();

            for cond_id in to_close {
                if let Some(pos) = state.positions.remove(&cond_id) {
                    // Settlement price for the exact token we hold (NO/Down/…).
                    let exit_price = resolutions
                        .get(&cond_id)
                        .and_then(|r| r.price_for(&pos.token_id))
                        .unwrap_or(0.0);
                    let realized = (exit_price - pos.avg_entry_price) * pos.size_shares;
                    state.usdc_available += exit_price * pos.size_shares;
                    state.realized_pnl += realized;
                    tracing::info!(
                        market = %cond_id,
                        token = %pos.token_id,
                        exit_price,
                        realized,
                        "position settled at resolution"
                    );
                    closed_trades.push(TradeRecord {
                        id: None,
                        ts: chrono::Utc::now(),
                        mode: ExecutionMode::Paper,
                        market_id: pos.condition_id.clone(),
                        category: pos.category.clone(),
                        underlying: pos.underlying.clone(),
                        expiry: chrono::Utc::now(),
                        side: pos.side,
                        entry_price: pos.avg_entry_price,
                        size_shares: pos.size_shares,
                        source: pos.source.clone(),
                        copy_wallet: pos.copy_wallet.clone(),
                        exit_price: Some(exit_price),
                        realized_pnl: Some(realized),
                    });
                }
            }
        }
        for trade in &closed_trades {
            if let Err(e) = self.storage.insert_trade(trade) {
                tracing::warn!(error = %e, "failed to persist resolved trade");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::ExecutionBackend;
    use crate::types::BookUpdate;
    use std::time::Instant;

    fn sample_book() -> BookUpdate {
        BookUpdate {
            asset_id: "no-1".into(),
            bids: vec![BookLevel { price: 0.84, size: 100.0 }],
            asks: vec![
                BookLevel { price: 0.85, size: 50.0 },
                BookLevel { price: 0.86, size: 100.0 },
            ],
            received_at: Instant::now(),
        }
    }

    #[tokio::test]
    async fn paper_fills_against_book() {
        let cache = BookCache::new();
        cache.update(sample_book());
        let storage = Storage::in_memory().unwrap();
        let backend = PaperBackend::new(
            ExecutionConfig {
                mode: ExecutionMode::Paper,
                baseline_latency_ms: 1,
                latency_jitter_ms: 0,
                take_profit_price: 0.99,
                stop_loss_price: None,
                stop_loss_fraction: 0.20,
            },
            10_000.0,
            cache,
            storage,
        );
        let req = OrderRequest {
            client_order_id: "c1".into(),
            token_id: "no-1".into(),
            condition_id: "cond".into(),
            side: Side::No,
            limit_price: 0.86,
            size_shares: 60.0,
            mode: ExecutionMode::Paper,
            source: "strategy".into(),
            category: "crypto".into(),
            underlying: "BTC".into(),
        };
        let result = backend.place_order(req).await.unwrap();
        assert!(result.filled_shares > 0.0);
        assert!(matches!(result.status, OrderStatus::Filled | OrderStatus::PartiallyFilled));
    }

    fn exec_config() -> ExecutionConfig {
        ExecutionConfig {
            mode: ExecutionMode::Paper,
            baseline_latency_ms: 1,
            latency_jitter_ms: 0,
            take_profit_price: 0.99,
            stop_loss_price: None,
            stop_loss_fraction: 0.20,
        }
    }

    fn book_with_bid(asset: &str, bid: f64, ask: f64) -> BookUpdate {
        BookUpdate {
            asset_id: asset.into(),
            bids: vec![BookLevel { price: bid, size: 500.0 }],
            asks: vec![BookLevel { price: ask, size: 500.0 }],
            received_at: Instant::now(),
        }
    }

    async fn open_no_position(backend: &PaperBackend, token: &str, price: f64) {
        let req = OrderRequest {
            client_order_id: Uuid::new_v4().to_string(),
            token_id: token.into(),
            condition_id: format!("cond-{token}"),
            side: Side::No,
            limit_price: price,
            size_shares: 100.0,
            mode: ExecutionMode::Paper,
            source: "strategy".into(),
            category: "crypto".into(),
            underlying: "BTC".into(),
        };
        backend.place_order(req).await.unwrap();
    }

    #[tokio::test]
    async fn mark_to_market_reports_unrealized_pnl() {
        let cache = BookCache::new();
        cache.update(book_with_bid("no-1", 0.85, 0.86));
        let storage = Storage::in_memory().unwrap();
        let backend = PaperBackend::new(exec_config(), 10_000.0, cache.clone(), storage);

        open_no_position(&backend, "no-1", 0.86).await;
        // Price rises but stays below take-profit: position stays open with gain.
        cache.update(book_with_bid("no-1", 0.92, 0.93));

        let mark = backend.mark_and_settle(&exec_config());
        assert_eq!(mark.positions.len(), 1);
        assert!(mark.unrealized_pnl > 0.0, "expected positive MTM, got {}", mark.unrealized_pnl);
        assert!(mark.exposure.total_invested_usd > 0.0);
        assert_eq!(mark.exposure.open_position_count, 1);
    }

    #[tokio::test]
    async fn take_profit_settles_and_frees_exposure() {
        let cache = BookCache::new();
        cache.update(book_with_bid("no-1", 0.85, 0.86));
        let storage = Storage::in_memory().unwrap();
        let backend = PaperBackend::new(exec_config(), 10_000.0, cache.clone(), storage);

        open_no_position(&backend, "no-1", 0.86).await;
        // NO climbs to ~resolution: should trigger take-profit close.
        cache.update(book_with_bid("no-1", 0.99, 0.995));

        let mark = backend.mark_and_settle(&exec_config());
        assert_eq!(mark.positions.len(), 0, "position should be settled");
        assert!(mark.realized_pnl > 0.0, "expected realized gain, got {}", mark.realized_pnl);
        assert_eq!(mark.exposure.total_invested_usd, 0.0, "exposure should free up");
    }
}
