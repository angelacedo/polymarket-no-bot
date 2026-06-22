mod live;
mod paper;

use async_trait::async_trait;
use std::sync::Arc;

use anyhow::Result;

pub use live::LiveBackend;
pub use paper::PaperBackend;

use crate::types::{Balances, ExecutionMode, OrderRequest, OrderResult, Position};

#[async_trait]
pub trait ExecutionBackend: Send + Sync {
    async fn place_order(&self, req: OrderRequest) -> Result<OrderResult>;
    async fn cancel_order(&self, order_id: &str) -> Result<()>;
    async fn open_positions(&self) -> Result<Vec<Position>>;
    async fn balances(&self) -> Result<Balances>;
    fn mode(&self) -> ExecutionMode;
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
}

pub type SharedBackend = Arc<dyn ExecutionBackend>;
