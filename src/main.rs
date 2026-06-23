use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, Subcommand};
use parking_lot::RwLock;
use tokio::sync::{mpsc, watch};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use polymarket_no_bot::config::BotConfig;
use polymarket_no_bot::copytrade::run_copytrade_loop;
use polymarket_no_bot::exchange::ExchangeHub;
use polymarket_no_bot::execution::{Backend, ExecutionBackend, LiveBackend, PaperBackend};
use polymarket_no_bot::learning::run_learning_loop;
use polymarket_no_bot::metrics::{MetricsRegistry, MetricsServer, build_state};
use polymarket_no_bot::risk::RiskEngine;
use polymarket_no_bot::storage::Storage;
use polymarket_no_bot::strategy::{StrategyEngine, run_strategy_loop};
use polymarket_no_bot::types::{
    ExecutionMode, OrderIntent, OrderRequest, RiskDecision, Side, TradeRecord, TradeSignal,
};

#[derive(Parser)]
#[command(name = "polymarket-no-bot")]
#[command(about = "Polymarket short-NO trading bot with copy-trading")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the trading bot
    Run {
        #[arg(long, default_value = "config/sample.toml")]
        config: PathBuf,
        #[arg(long)]
        mode: Option<String>,
    },
    /// Print current status from database
    Status {
        #[arg(long, default_value = "config/sample.toml")]
        config: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Validate configuration file
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    Validate {
        #[arg(long, default_value = "config/sample.toml")]
        path: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();
    let cli = Cli::parse();

    match cli.command {
        Commands::Run { config, mode } => run_bot(config, mode).await,
        Commands::Status { config, json } => print_status(config, json).await,
        Commands::Config { action } => match action {
            ConfigAction::Validate { path } => {
                BotConfig::load(&path)?;
                println!("Configuration valid: {}", path.display());
                Ok(())
            }
        },
    }
}

fn init_logging() {
    let json = std::env::var("LOG_FORMAT").map(|v| v == "json").unwrap_or(false);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if json {
        tracing_subscriber::fmt().json().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
}

async fn run_bot(config_path: PathBuf, mode_override: Option<String>) -> Result<()> {
    let mut config = BotConfig::load(&config_path)?;
    if let Some(m) = mode_override {
        config.execution.mode = parse_mode(&m)?;
    }
    let mode = config.execution.mode;
    info!(mode = %mode, "starting polymarket-no-bot");

    let storage = Storage::open(std::path::Path::new(&config.storage.database_path))?;
    let hub = Arc::new(ExchangeHub::new(&config.exchange));
    let registry = MetricsRegistry::new();
    let risk = Arc::new(RwLock::new(RiskEngine::new(config.risk.clone())));

    let backend: Arc<dyn ExecutionBackend> = match mode {
        ExecutionMode::Paper => {
            let paper = PaperBackend::new(
                config.execution.clone(),
                config.risk.total_capital,
                hub.book_cache.clone(),
                storage.clone(),
            );
            Arc::new(Backend::Paper(paper))
        }
        ExecutionMode::Live => {
            if risk.read().circuit().live_disabled {
                warn!("live disabled by circuit breaker, falling back to paper");
                let paper = PaperBackend::new(
                    config.execution.clone(),
                    config.risk.total_capital,
                    hub.book_cache.clone(),
                    storage.clone(),
                );
                Arc::new(Backend::Paper(paper))
            } else {
                let live = LiveBackend::new(storage.clone(), config.exchange.clone())?;
                Arc::new(Backend::Live(live))
            }
        }
    };

    let (signal_tx, mut signal_rx) = mpsc::channel::<TradeSignal>(1024);
    let (book_tx, book_rx) = mpsc::channel::<polymarket_no_bot::types::BookUpdate>(4096);
    let (copy_tx, copy_rx) = mpsc::channel::<TradeSignal>(256);
    let (config_tx, config_rx) = watch::channel(config.clone());

    let mut strategy_engine = StrategyEngine::new(config.clone(), hub.clone(), storage.clone());
    let token_ids = strategy_engine.refresh_markets().await.unwrap_or_default();
    let markets_shared = Arc::new(RwLock::new(strategy_engine.markets().clone()));

    // Dynamic WS subscription: strategy loop sends new token lists here when
    // refresh_markets() discovers new markets after startup.
    let (token_ids_tx, token_ids_rx) = tokio::sync::watch::channel(token_ids.clone());

    let _book_feed = hub.start_book_feed(token_ids_rx, book_tx);

    let strategy_handle = {
        let cfg = config.clone();
        let markets_shared = markets_shared.clone();
        tokio::spawn(async move {
            run_strategy_loop(
                strategy_engine,
                book_rx,
                copy_rx,
                signal_tx.clone(),
                cfg.strategy.scan_interval_secs,
                Some(markets_shared.clone()),
                Some(token_ids_tx),
            )
            .await;
        })
    };

    let copy_handle = {
        let cfg = config.clone();
        let hub = hub.clone();
        let markets = markets_shared.clone();
        tokio::spawn(async move {
            run_copytrade_loop(cfg, hub, markets, copy_tx).await;
        });
    };

    let learning_handle = {
        let cfg = config.clone();
        let storage = storage.clone();
        tokio::spawn(async move {
            run_learning_loop(cfg, storage, config_tx).await;
        });
    };

    let metrics_state = build_state(storage.clone(), registry.clone(), risk.clone(), backend.clone());
    let metrics_handle = {
        let server = MetricsServer::new(config.metrics.bind_addr.clone());
        tokio::spawn(async move {
            if let Err(e) = server.serve(metrics_state).await {
                tracing::error!(error = %e, "metrics server failed");
            }
        });
    };

    let execution_handle = {
        let backend = backend.clone();
        let storage = storage.clone();
        let registry = registry.clone();
        let risk = risk.clone();
        let mut config_rx = config_rx;
        tokio::spawn(async move {
            let mut active_config = config_rx.borrow().clone();
            loop {
                tokio::select! {
                    _ = config_rx.changed() => {
                        active_config = config_rx.borrow().clone();
                        info!("effective config updated by learning module");
                    }
                    sig = signal_rx.recv() => {
                        let Some(signal) = sig else { break };
                        if let Err(e) = process_signal(
                            &active_config,
                            &backend,
                            &storage,
                            &registry,
                            &risk,
                            signal,
                        ).await {
                            warn!(error = %e, "signal processing failed");
                        }
                    }
                }
            }
        })
    };

    let risk_monitor = {
        let risk = risk.clone();
        let registry = registry.clone();
        let storage = storage.clone();
        let backend = backend.clone();
        let exec_cfg = config.execution.clone();
        let mode = backend.mode();
        let hub_for_resolution = hub.clone();
        tokio::spawn(async move {
            let mut mtm_interval = tokio::time::interval(std::time::Duration::from_secs(15));
            // Check for market resolutions every 5 minutes.
            let mut resolution_interval =
                tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                tokio::select! {
                    _ = mtm_interval.tick() => {
                        // Mark positions to market and settle take-profit / stop-loss exits.
                        let mark = backend.mark_and_settle(&exec_cfg);
                        {
                            let mut rk = risk.write();
                            rk.maybe_reset_daily(chrono::Utc::now());
                            // Reconcile exposure from actual open positions so capital
                            // frees up as positions close (avoids permanent "no headroom").
                            rk.update_exposure(mark.exposure.clone());
                            rk.update_mtm(mark.unrealized_pnl, mark.realized_pnl);
                            registry.update_from_risk(&rk, mode);
                            let _ = storage.record_equity(mode, rk.pnl());
                        }
                        let _ = storage.snapshot_positions(&mark.positions, mark.unrealized_pnl);
                    }
                    _ = resolution_interval.tick() => {
                        // Settle any positions whose markets have resolved on-chain.
                        let open = backend.open_positions().await.unwrap_or_default();
                        if !open.is_empty() {
                            let condition_ids: Vec<String> =
                                open.iter().map(|p| p.condition_id.clone()).collect();
                            match hub_for_resolution
                                .gamma
                                .fetch_market_resolutions(&condition_ids)
                                .await
                            {
                                Ok(resolutions) if !resolutions.is_empty() => {
                                    backend.settle_resolved_markets(&resolutions);
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    warn!(error = %e, "resolution check failed");
                                }
                            }
                        }
                    }
                }
            }
        })
    };

    tokio::signal::ctrl_c().await?;
    info!("shutdown signal received");
    drop(execution_handle);
    drop(strategy_handle);
    drop(copy_handle);
    drop(learning_handle);
    drop(metrics_handle);
    drop(risk_monitor);
    Ok(())
}

async fn process_signal(
    config: &BotConfig,
    backend: &Arc<dyn ExecutionBackend>,
    storage: &Storage,
    registry: &MetricsRegistry,
    risk: &Arc<RwLock<RiskEngine>>,
    signal: TradeSignal,
) -> Result<()> {
    // Dedup: skip if we already hold a position in this market.
    let existing = backend.open_positions().await.unwrap_or_default();
    if existing.iter().any(|p| p.condition_id == signal.market.condition_id) {
        return Ok(());
    }

    let token_id = if signal.side == Side::No {
        signal.market.no_token_id.clone()
    } else {
        signal.market.yes_token_id.clone()
    };

    let intent = OrderIntent {
        market: signal.market.clone(),
        side: signal.side,
        token_id: token_id.clone(),
        limit_price: signal.entry_price,
        size_shares: signal.suggested_size_usd / signal.entry_price.max(1e-9),
        notional_usd: signal.suggested_size_usd,
        source: signal.source.clone(),
        signal_ts: signal.signal_ts,
        reducing: false,
    };

    let risk_start = Instant::now();
    let decision = {
        let rk = risk.read();
        rk.evaluate(&intent, config.effective_price_range())
    };
    let decision_ms = risk_start.elapsed().as_millis() as u64;
    registry
        .latency
        .write()
        .record_decision_to_order(decision_ms);

    let (size_shares, notional) = match decision {
        RiskDecision::Approved {
            size_shares,
            notional_usd,
        } => (size_shares, notional_usd),
        RiskDecision::Downsized {
            size_shares,
            notional_usd,
            ..
        } => (size_shares, notional_usd),
        RiskDecision::Rejected { reason } => {
            info!(reason, market = %intent.market.condition_id, "order rejected by risk");
            return Ok(());
        }
    };

    let req = OrderRequest {
        client_order_id: uuid::Uuid::new_v4().to_string(),
        token_id,
        condition_id: intent.market.condition_id.clone(),
        side: intent.side,
        limit_price: intent.limit_price,
        size_shares,
        mode: backend.mode(),
        source: intent.source.as_db_str(),
        category: intent.market.category.clone(),
        underlying: intent.market.underlying.clone(),
    };

    let order_start = Instant::now();
    let result = backend.place_order(req.clone()).await?;
    registry
        .latency
        .write()
        .record_order_to_fill(result.latency_ms);

    if result.filled_shares > 0.0 {
        risk.write().record_fill(&intent, notional);
        storage.insert_trade(&TradeRecord {
            id: None,
            ts: chrono::Utc::now(),
            mode: backend.mode(),
            market_id: intent.market.condition_id.clone(),
            category: intent.market.category.clone(),
            underlying: intent.market.underlying.clone(),
            expiry: intent.market.end_date,
            side: intent.side,
            entry_price: result.avg_fill_price,
            size_shares: result.filled_shares,
            source: intent.source.as_db_str(),
            copy_wallet: intent.source.copy_wallet().map(str::to_string),
            exit_price: None,
            realized_pnl: None,
        })?;
        info!(
            mode = %backend.mode(),
            market = %intent.market.condition_id,
            filled = result.filled_shares,
            price = result.avg_fill_price,
            latency_ms = order_start.elapsed().as_millis(),
            "order filled"
        );
    }

    Ok(())
}

async fn print_status(config_path: PathBuf, json: bool) -> Result<()> {
    let config = BotConfig::load(&config_path)?;
    let storage = Storage::open(std::path::Path::new(&config.storage.database_path))?;
    let paper = storage
        .latest_equity(ExecutionMode::Paper)?
        .unwrap_or_default();
    let live = storage
        .latest_equity(ExecutionMode::Live)?
        .unwrap_or_default();
    let trades = storage.recent_trades(10)?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "paper_pnl": paper,
                "live_pnl": live,
                "recent_trades": trades,
            }))?
        );
    } else {
        println!("=== Polymarket NO Bot Status ===");
        println!("Paper equity: ${:.2} (realized ${:.2}, unrealized ${:.2})", paper.equity, paper.realized_pnl, paper.unrealized_pnl);
        println!("Live equity:  ${:.2} (realized ${:.2}, unrealized ${:.2})", live.equity, live.realized_pnl, live.unrealized_pnl);
        println!("\nRecent trades:");
        println!("{:<20} {:<6} {:<12} {:<8} {:<8}", "Time", "Mode", "Market", "Price", "Size");
        for t in trades {
            println!(
                "{:<20} {:<6} {:<12} {:.4}    {:.2}",
                t.ts.format("%Y-%m-%d %H:%M"),
                t.mode,
                &t.market_id[..t.market_id.len().min(12)],
                t.entry_price,
                t.size_shares,
            );
        }
    }
    Ok(())
}

fn parse_mode(s: &str) -> Result<ExecutionMode> {
    match s.to_lowercase().as_str() {
        "paper" => Ok(ExecutionMode::Paper),
        "live" => Ok(ExecutionMode::Live),
        other => anyhow::bail!("unknown mode: {other}"),
    }
}
