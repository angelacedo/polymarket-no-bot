use std::sync::Arc;

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{delete, get, patch, post},
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tracing::info;

use crate::execution::ExecutionBackend;
use crate::risk::RiskEngine;
use crate::storage::Storage;
use crate::types::{ExecutionMode, LatencyTracker, TradeView, TuningAuditRecord, WalletRecord, WalletView};
use crate::wallets::{CandidateWallet, WalletEvaluation, evaluate_wallet, fetch_wallet_metrics};

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
    pub recent_trades: Vec<TradeView>,
    pub recent_tuning_events: Vec<TuningAuditRecord>,
    pub wallets: Vec<WalletView>,
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
            .route("/api/wallets", get(list_wallets).post(add_wallet))
            .route("/api/wallets/evaluate", get(evaluate_wallets_endpoint))
            .route("/api/wallets/{address}", delete(remove_wallet).patch(update_wallet))
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
    state.registry.reset();
    {
        let mut rk = state.risk.write();
        rk.reset();
        state.registry.update_from_risk(&rk, ExecutionMode::Paper);
    }

    info!("full reset via admin endpoint (history, stats, tuning, cache, balance)");
    (
        StatusCode::OK,
        serde_json::json!({
            "ok": true,
            "message": "full reset: trades, orders, equity curve, tuning audit, market cache, latency stats and paper balance all cleared"
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
    let recent_trades = state.storage.recent_trades_enriched(20)?;
    let recent_tuning = state.storage.recent_tuning(10)?;
    let open_positions = state.storage.open_position_count().unwrap_or(0);
    let wallets = state.storage.list_wallets_with_stats().unwrap_or_default();

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
        wallets,
    })
}

// ─── Wallet API Handlers ─────────────────────────────────────────────

async fn list_wallets(State(state): State<AppState>) -> impl IntoResponse {
    match state.storage.list_wallets_with_stats() {
        Ok(wallets) => axum::Json(wallets).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct AddWalletRequest {
    address: String,
    label: Option<String>,
    scale_factor: Option<f64>,
    max_daily_exposure_usd: Option<f64>,
    min_trade_size_usd: Option<f64>,
    allowed_categories: Option<Vec<String>>,
    blocked_categories: Option<Vec<String>>,
}

async fn add_wallet(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<AddWalletRequest>,
) -> impl IntoResponse {
    // Validate address format
    if !req.address.starts_with("0x") || req.address.len() != 42 {
        return (StatusCode::BAD_REQUEST, "invalid address format").into_response();
    }

    let wallet = WalletRecord {
        address: req.address.to_lowercase(),
        label: req.label,
        scale_factor: req.scale_factor.unwrap_or(0.1),
        max_daily_exposure_usd: req.max_daily_exposure_usd.unwrap_or(500.0),
        min_trade_size_usd: req.min_trade_size_usd.unwrap_or(25.0),
        allowed_categories: req.allowed_categories.unwrap_or_default(),
        blocked_categories: req.blocked_categories.unwrap_or_default(),
        source: "manual".to_string(),
        enabled: true,
        created_at: chrono::Utc::now(),
    };

    match state.storage.add_wallet(&wallet) {
        Ok(()) => {
            let mut resp = axum::Json(serde_json::json!({"ok": true})).into_response();
            *resp.status_mut() = StatusCode::CREATED;
            resp
        }
        Err(e) => {
            if e.to_string().contains("UNIQUE constraint") {
                (StatusCode::CONFLICT, "wallet already exists").into_response()
            } else {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
        }
    }
}

async fn remove_wallet(
    State(state): State<AppState>,
    axum::extract::Path(address): axum::extract::Path<String>,
) -> impl IntoResponse {
    match state.storage.remove_wallet(&address) {
        Ok(true) => axum::Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "wallet not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct UpdateWalletRequest {
    label: Option<String>,
    scale_factor: Option<f64>,
    max_daily_exposure_usd: Option<f64>,
    min_trade_size_usd: Option<f64>,
    allowed_categories: Option<Vec<String>>,
    blocked_categories: Option<Vec<String>>,
    enabled: Option<bool>,
}

async fn update_wallet(
    State(state): State<AppState>,
    axum::extract::Path(address): axum::extract::Path<String>,
    axum::Json(req): axum::Json<UpdateWalletRequest>,
) -> impl IntoResponse {
    // Get existing wallet
    let wallets = match state.storage.list_wallets() {
        Ok(w) => w,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    };

    let existing = match wallets.into_iter().find(|w| w.address.to_lowercase() == address.to_lowercase()) {
        Some(w) => w,
        None => return (StatusCode::NOT_FOUND, "wallet not found").into_response(),
    };

    let updated = WalletRecord {
        address: existing.address,
        label: req.label.or(existing.label),
        scale_factor: req.scale_factor.unwrap_or(existing.scale_factor),
        max_daily_exposure_usd: req.max_daily_exposure_usd.unwrap_or(existing.max_daily_exposure_usd),
        min_trade_size_usd: req.min_trade_size_usd.unwrap_or(existing.min_trade_size_usd),
        allowed_categories: req.allowed_categories.unwrap_or(existing.allowed_categories),
        blocked_categories: req.blocked_categories.unwrap_or(existing.blocked_categories),
        source: existing.source,
        enabled: req.enabled.unwrap_or(existing.enabled),
        created_at: existing.created_at,
    };

    match state.storage.update_wallet(&updated) {
        Ok(true) => axum::Json(serde_json::json!({"ok": true})).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "wallet not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ─── Wallet Evaluation Endpoint ───────────────────────────────────────

#[derive(Deserialize)]
struct EvaluateWalletsQuery {
    addresses: String,
}

#[derive(Serialize)]
struct EvaluateWalletsResponse {
    evaluated_at: String,
    results: Vec<WalletEvaluation>,
}

async fn evaluate_wallets_endpoint(
    axum::extract::Query(query): axum::extract::Query<EvaluateWalletsQuery>,
) -> impl IntoResponse {
    let addresses: Vec<&str> = query.addresses.split(',').map(|s| s.trim()).collect();
    
    if addresses.is_empty() {
        return (StatusCode::BAD_REQUEST, "no addresses provided").into_response();
    }

    let mut results = Vec::new();
    
    for address in addresses {
        if !address.starts_with("0x") || address.len() != 42 {
            results.push(WalletEvaluation {
                address: address.to_string(),
                win_rate: None,
                closed_markets: None,
                avg_hold_days: None,
                max_drawdown: None,
                total_pnl: None,
                sharpe: None,
                score: 0.0,
                status: "ERROR".to_string(),
                reasons: vec!["invalid address format".to_string()],
            });
            continue;
        }

        match fetch_wallet_metrics(address).await {
            Ok(wallet) => {
                let evaluation = evaluate_wallet(&wallet);
                results.push(evaluation);
            }
            Err(e) => {
                results.push(WalletEvaluation {
                    address: address.to_string(),
                    win_rate: None,
                    closed_markets: None,
                    avg_hold_days: None,
                    max_drawdown: None,
                    total_pnl: None,
                    sharpe: None,
                    score: 0.0,
                    status: "ERROR".to_string(),
                    reasons: vec![format!("failed to fetch metrics: {}", e)],
                });
            }
        }
    }

    let response = EvaluateWalletsResponse {
        evaluated_at: chrono::Utc::now().to_rfc3339(),
        results,
    };

    axum::Json(response).into_response()
}
