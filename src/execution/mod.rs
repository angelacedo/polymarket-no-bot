mod live;
mod paper;

use async_trait::async_trait;
use std::sync::Arc;

use anyhow::Result;

pub use live::LiveBackend;
pub use paper::PaperBackend;

use crate::config::ExecutionConfig;
use crate::types::{Balances, ExecutionMode, ExposureSnapshot, OrderRequest, OrderResult, Position};

/// Result of marking the portfolio to market and settling closed positions.
#[derive(Debug, Clone, Default)]
pub struct PortfolioMark {
    pub positions: Vec<Position>,
    pub unrealized_pnl: f64,
    pub realized_pnl: f64,
    pub exposure: ExposureSnapshot,
}

#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    async fn place_order(&self, req: OrderRequest) -> Result<OrderResult>;
    async fn cancel_order(&self, order_id: &str) -> Result<()>;
    async fn open_positions(&self) -> Result<Vec<Position>>;
    async fn balances(&self) -> Result<Balances>;
    fn mode(&self) -> ExecutionMode;

    /// Mark positions to market, settle take-profit / stop-loss exits, and
    /// return the reconciled portfolio snapshot. Defaults to an empty snapshot
    /// for backends (e.g. live) that source positions from the exchange.
    fn mark_and_settle(&self, _exec_cfg: &ExecutionConfig) -> PortfolioMark {
        PortfolioMark::default()
    }
}

pub enum Backend {
    Live(LiveBackend),
    Paper(PaperBackend),
}

#[async_trait]
impl ExecutionBackend for Backend {
    async fn place_order(&self, req: OrderRequest) -> Result<OrderResult> {
        match self {
            Self::Live(b) => b.place_order(req).await,
            Self::Paper(b) => b.place_order(req).await,
        }
    }

    async fn cancel_order(&self, order_id: &str) -> Result<()> {
        match self {
            Self::Live(b) => b.cancel_order(order_id).await,
            Self::Paper(b) => b.cancel_order(order_id).await,
        }
    }

    async fn open_positions(&self) -> Result<Vec<Position>> {
        match self {
            Self::Live(b) => b.open_positions().await,
            Self::Paper(b) => b.open_positions().await,
        }
    }

    async fn balances(&self) -> Result<Balances> {
        match self {
            Self::Live(b) => b.balances().await,
            Self::Paper(b) => b.balances().await,
        }
    }

    fn mode(&self) -> ExecutionMode {
        match self {
            Self::Live(b) => b.mode(),
            Self::Paper(b) => b.mode(),
        }
    }

    fn mark_and_settle(&self, exec_cfg: &ExecutionConfig) -> PortfolioMark {
        match self {
            Self::Live(b) => b.mark_and_settle(exec_cfg),
            Self::Paper(b) => b.mark_and_settle(exec_cfg),
        }
    }
}

pub type SharedBackend = Arc<dyn ExecutionBackend>;
