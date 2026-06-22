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
use crate::exchange::BookCache;
use crate::storage::Storage;
use crate::types::{
    Balances, BookLevel, ExecutionMode, OrderRequest, OrderResult, OrderStatus, Position, Side,
};

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
}

#[async_trait]
impl super::ExecutionBackend for PaperBackend {
    async fn place_order(&self, req: OrderRequest) -> Result<OrderResult> {
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
}
