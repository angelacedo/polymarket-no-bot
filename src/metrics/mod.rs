mod dashboard;
mod server;

pub use server::{AppState, MetricsServer, StatusSnapshot};

use std::sync::Arc;

use parking_lot::RwLock;

use crate::execution::ExecutionBackend;
use crate::risk::RiskEngine;
use crate::storage::Storage;
use crate::types::{CircuitBreakerState, ExecutionMode, LatencyTracker, PnlSnapshot};

#[derive(Clone, Default)]
pub struct MetricsRegistry {
    pub latency: Arc<RwLock<LatencyTracker>>,
    pub paper_pnl: Arc<RwLock<PnlSnapshot>>,
    pub live_pnl: Arc<RwLock<PnlSnapshot>>,
    pub circuit: Arc<RwLock<CircuitBreakerState>>,
    pub mode: Arc<RwLock<ExecutionMode>>,
}

impl MetricsRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update_from_risk(&self, risk: &RiskEngine, mode: ExecutionMode) {
        *self.circuit.write() = risk.circuit().clone();
        *self.mode.write() = mode;
        let pnl = risk.pnl().clone();
        match mode {
            ExecutionMode::Paper => *self.paper_pnl.write() = pnl,
            ExecutionMode::Live => *self.live_pnl.write() = pnl,
        }
    }
}

pub fn build_state(
    storage: Storage,
    registry: MetricsRegistry,
    risk: Arc<RwLock<RiskEngine>>,
    backend: Arc<dyn ExecutionBackend>,
) -> AppState {
    let admin_reset_token = std::env::var("ADMIN_RESET_TOKEN").ok();
    AppState {
        storage,
        registry,
        risk,
        backend,
        admin_reset_token,
    }
}
