use anyhow::{Context, Result};
use clap::Parser;
use reqwest::Client;
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;
use tabled::{Table, settings::Style};
use tracing::info;

use polymarket_no_bot::config::BotConfig;
use polymarket_no_bot::storage::Storage;

#[derive(Parser)]
#[command(name = "evaluate_wallets")]
#[command(about = "Evaluate copytrade wallets against external metrics")]
struct Cli {
    #[arg(long, default_value = "config/sample.toml")]
    config: String,

    #[arg(long, default_value = "30")]
    days: u32,
}

#[derive(Debug, Clone)]
struct WalletMetrics {
    address: String,
    win_rate: Option<f64>,
    closed_markets: Option<u32>,
    avg_hold_days: Option<f64>,
    total_pnl: Option<f64>,
}

impl WalletMetrics {
    fn status(&self) -> &'static str {
        let wr = self.win_rate.unwrap_or(0.0);
        let cm = self.closed_markets.unwrap_or(0);
        let ahd = self.avg_hold_days.unwrap_or(f64::INFINITY);

        if wr >= 0.60 && cm >= 50 && ahd <= 7.0 {
            "OK"
        } else {
            "WEAK"
        }
    }
}

#[derive(Debug, Deserialize)]
struct StructTrader {
    address: Option<String>,
    #[serde(alias = "winRate", alias = "win_rate")]
    win_rate: Option<f64>,
    #[serde(alias = "closedMarkets", alias = "closed_markets")]
    closed_markets: Option<u32>,
    #[serde(alias = "avgHoldDays", alias = "avg_hold_days")]
    avg_hold_days: Option<f64>,
    #[serde(alias = "totalPnl", alias = "total_pnl", alias = "pnl")]
    total_pnl: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct PolysyncerTrader {
    address: Option<String>,
    #[serde(alias = "winRate", alias = "win_rate")]
    win_rate: Option<f64>,
    #[serde(alias = "markets", alias = "closedMarkets")]
    closed_markets: Option<u32>,
    #[serde(alias = "avgHold", alias = "avg_hold_days")]
    avg_hold_days: Option<f64>,
    #[serde(alias = "pnl", alias = "profit")]
    total_pnl: Option<f64>,
}

async fn fetch_struct_metrics(client: &Client, address: &str) -> Result<WalletMetrics> {
    let url = format!(
        "https://explorer.struct.to/api/traders?platform=polymarket&address={}",
        address
    );

    let resp = client
        .get(&url)
        .send()
        .await
        .context("fetching Struct.to metrics")?;

    if !resp.status().is_success() {
        anyhow::bail!("Struct.to returned status {}", resp.status());
    }

    let traders: Vec<StructTrader> = resp.json().await?;
    let trader = traders
        .into_iter()
        .find(|t| t.address.as_deref() == Some(address))
        .ok_or_else(|| anyhow::anyhow!("wallet not found in Struct.to"))?;

    Ok(WalletMetrics {
        address: address.to_string(),
        win_rate: trader.win_rate,
        closed_markets: trader.closed_markets,
        avg_hold_days: trader.avg_hold_days,
        total_pnl: trader.total_pnl,
    })
}

async fn fetch_polysyncer_metrics(client: &Client, address: &str) -> Result<WalletMetrics> {
    let url = format!(
        "https://www.polysyncer.com/api/leaderboard?address={}",
        address
    );

    let resp = client
        .get(&url)
        .send()
        .await
        .context("fetching Polysyncer metrics")?;

    if !resp.status().is_success() {
        anyhow::bail!("Polysyncer returned status {}", resp.status());
    }

    let traders: Vec<PolysyncerTrader> = resp.json().await?;
    let trader = traders
        .into_iter()
        .find(|t| t.address.as_deref() == Some(address))
        .ok_or_else(|| anyhow::anyhow!("wallet not found in Polysyncer"))?;

    Ok(WalletMetrics {
        address: address.to_string(),
        win_rate: trader.win_rate,
        closed_markets: trader.closed_markets,
        avg_hold_days: trader.avg_hold_days,
        total_pnl: trader.total_pnl,
    })
}

async fn fetch_wallet_metrics(client: &Client, address: &str) -> Result<WalletMetrics> {
    match fetch_struct_metrics(client, address).await {
        Ok(m) => Ok(m),
        Err(e) => {
            tracing::warn!(error = %e, "Struct.to failed, trying Polysyncer");
            fetch_polysyncer_metrics(client, address).await
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let config = BotConfig::load(Path::new(&cli.config))?;
    let storage = Storage::open(std::path::Path::new(&config.storage.database_path))?;

    let wallets = storage.list_enabled_wallets()?;
    info!(count = wallets.len(), "loaded enabled wallets");

    let client = Client::builder()
        .user_agent("polymarket-no-bot-evaluator/0.1")
        .timeout(Duration::from_secs(30))
        .build()?;

    let mut metrics_vec = Vec::new();
    for wallet in &wallets {
        print!("Evaluating {}... ", &wallet.address[..10]);
        match fetch_wallet_metrics(&client, &wallet.address).await {
            Ok(m) => {
                println!("OK");
                metrics_vec.push(m);
            }
            Err(e) => {
                println!("FAILED: {}", e);
                metrics_vec.push(WalletMetrics {
                    address: wallet.address.clone(),
                    win_rate: None,
                    closed_markets: None,
                    avg_hold_days: None,
                    total_pnl: None,
                });
            }
        }
    }

    println!("\n{}", "=".repeat(100));
    println!("WALLET EVALUATION RESULTS");
    println!("{}", "=".repeat(100));

    let table_data: Vec<_> = metrics_vec
        .iter()
        .map(|m| {
            (
                format!("{}...{}", &m.address[..6], &m.address[m.address.len()-4..]),
                m.win_rate.map(|v| format!("{:.1}%", v * 100.0)).unwrap_or_else(|| "N/A".to_string()),
                m.closed_markets.map(|v| v.to_string()).unwrap_or_else(|| "N/A".to_string()),
                m.avg_hold_days.map(|v| format!("{:.1}d", v)).unwrap_or_else(|| "N/A".to_string()),
                m.total_pnl.map(|v| format!("${:.2}", v)).unwrap_or_else(|| "N/A".to_string()),
                m.status(),
            )
        })
        .collect();

    let table = Table::new(table_data)
        .with(Style::rounded())
        .to_string();

    println!("\n{table}");

    let ok_count = metrics_vec.iter().filter(|m| m.status() == "OK").count();
    let weak_count = metrics_vec.len() - ok_count;

    println!("\n{}", "=".repeat(100));
    println!("SUMMARY");
    println!("{}", "=".repeat(100));
    println!("Total wallets: {}", metrics_vec.len());
    println!("OK (meets criteria): {}", ok_count);
    println!("WEAK (needs replacement): {}", weak_count);

    if weak_count > 0 {
        println!("\nRecommendation: Replace {} weak wallets with auto-discovered candidates", weak_count);
        println!("Run the bot with auto_discover_wallets=true to find better wallets");
    } else {
        println!("\nAll wallets meet the criteria. No replacement needed.");
    }

    Ok(())
}
