use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
};
use parking_lot::RwLock;
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::info;

use crate::execution::ExecutionBackend;
use crate::risk::RiskEngine;
use crate::storage::Storage;
use crate::types::{ExecutionMode, LatencyTracker, TuningAuditRecord, TradeRecord};

use super::MetricsRegistry;

#[derive(Clone)]
pub struct AppState {
    pub storage: Storage,
    pub registry: MetricsRegistry,
    pub risk: Arc<RwLock<RiskEngine>>,
    pub backend: Arc<dyn ExecutionBackend>,
    pub admin_reset_token: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct StatusSnapshot {
    pub mode: String,
    pub paper_pnl: crate::types::PnlSnapshot,
    pub live_pnl: crate::types::PnlSnapshot,
    pub circuit_breaker: crate::types::CircuitBreakerState,
    pub exposure: crate::types::ExposureSnapshot,
    pub open_positions: usize,
    pub latency_p50_decision_ms: u64,
    pub latency_p99_decision_ms: u64,
    pub latency_p50_fill_ms: u64,
    pub recent_trades: Vec<TradeRecord>,
    pub recent_tuning_events: Vec<TuningAuditRecord>,
}

pub struct MetricsServer {
    bind_addr: String,
}

impl MetricsServer {
    pub fn new(bind_addr: impl Into<String>) -> Self {
        Self {
            bind_addr: bind_addr.into(),
        }
    }

    pub async fn serve(self, state: AppState) -> anyhow::Result<()> {
        let app = Router::new()
            .route("/metrics", get(prometheus_metrics))
            .route("/api/status", get(json_status))
            .route("/api/admin/reset", post(admin_reset))
            .route("/dashboard", get(dashboard_page))
            .with_state(state);

        let listener = TcpListener::bind(&self.bind_addr).await?;
        info!(addr = %self.bind_addr, "metrics server listening");
        axum::serve(listener, app).await?;
        Ok(())
    }
}

async fn json_status(State(state): State<AppState>) -> impl IntoResponse {
    match build_snapshot(&state) {
        Ok(s) => axum::Json(s).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn prometheus_metrics(State(state): State<AppState>) -> impl IntoResponse {
    match build_snapshot(&state) {
        Ok(s) => {
            let body = format!(
                "# HELP bot_equity Current equity\n\
                 # TYPE bot_equity gauge\n\
                 bot_equity{{mode=\"paper\"}} {}\n\
                 bot_equity{{mode=\"live\"}} {}\n\
                 # HELP bot_realized_pnl Realized PnL\n\
                 # TYPE bot_realized_pnl gauge\n\
                 bot_realized_pnl{{mode=\"paper\"}} {}\n\
                 bot_realized_pnl{{mode=\"live\"}} {}\n\
                 # HELP bot_unrealized_pnl Unrealized PnL\n\
                 # TYPE bot_unrealized_pnl gauge\n\
                 bot_unrealized_pnl{{mode=\"paper\"}} {}\n\
                 bot_unrealized_pnl{{mode=\"live\"}} {}\n\
                 # HELP bot_drawdown_fraction Drawdown from peak\n\
                 # TYPE bot_drawdown_fraction gauge\n\
                 bot_drawdown_fraction {}\n\
                 # HELP bot_open_positions Open position count\n\
                 # TYPE bot_open_positions gauge\n\
                 bot_open_positions {}\n\
                 # HELP bot_latency_decision_p50_ms Decision to order p50\n\
                 # TYPE bot_latency_decision_p50_ms gauge\n\
                 bot_latency_decision_p50_ms {}\n\
                 # HELP bot_latency_fill_p50_ms Order to fill p50\n\
                 # TYPE bot_latency_fill_p50_ms gauge\n\
                 bot_latency_fill_p50_ms {}\n",
                s.paper_pnl.equity,
                s.live_pnl.equity,
                s.paper_pnl.realized_pnl,
                s.live_pnl.realized_pnl,
                s.paper_pnl.unrealized_pnl,
                s.live_pnl.unrealized_pnl,
                s.paper_pnl.drawdown_fraction,
                s.open_positions,
                s.latency_p50_decision_ms,
                s.latency_p50_fill_ms,
            );
            body
        }
        Err(e) => e.to_string(),
    }
}

async fn dashboard_page() -> impl IntoResponse {
    Html(super::dashboard::page())
}

async fn admin_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(expected) = state.admin_reset_token.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "reset endpoint disabled (ADMIN_RESET_TOKEN not set)".to_string(),
        )
            .into_response();
    };

    let provided = headers
        .get("X-Admin-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if provided != expected {
        return (StatusCode::UNAUTHORIZED, "invalid token".to_string()).into_response();
    }

    if let Err(e) = state.storage.reset_trading_history() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("db reset failed: {e}"),
        )
            .into_response();
    }

    state.backend.reset_paper_portfolio();
    {
        let mut rk = state.risk.write();
        rk.reset();
        state.registry.update_from_risk(&rk, ExecutionMode::Paper);
    }

    info!("paper trading reset via admin endpoint");
    (
        StatusCode::OK,
        serde_json::json!({
            "ok": true,
            "message": "all trade history cleared; paper portfolio reset to starting capital"
        })
        .to_string(),
    )
        .into_response()
}

fn build_snapshot(state: &AppState) -> anyhow::Result<StatusSnapshot> {
    let mode = *state.registry.mode.read();
    let paper = state.registry.paper_pnl.read().clone();
    let live = state.registry.live_pnl.read().clone();
    let circuit = state.registry.circuit.read().clone();
    let risk = state.risk.read();
    let exposure = risk.exposure().clone();
    let lat = state.registry.latency.read();
    let recent_trades = state.storage.recent_trades(20)?;
    let recent_tuning = state.storage.recent_tuning(10)?;
    let open_positions = state.storage.open_position_count().unwrap_or(0);

    Ok(StatusSnapshot {
        mode: mode.to_string(),
        paper_pnl: paper,
        live_pnl: live,
        circuit_breaker: circuit,
        exposure,
        open_positions,
        latency_p50_decision_ms: LatencyTracker::percentile(&lat.decision_to_order_ms, 0.50),
        latency_p99_decision_ms: LatencyTracker::percentile(&lat.decision_to_order_ms, 0.99),
        latency_p50_fill_ms: LatencyTracker::percentile(&lat.order_to_fill_ms, 0.50),
        recent_trades,
        recent_tuning_events: recent_tuning,
    })
}
