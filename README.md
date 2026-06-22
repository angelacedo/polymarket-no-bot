# Polymarket Short-NO Trading Bot

Production-grade Rust trading bot for [Polymarket](https://polymarket.com) that systematically buys **NO** shares in low-probability events while optionally copy-trading profitable wallets.

## Architecture

```
Market Feeds (Gamma, WS orderbook, Data API)
        │
        ▼
Strategy Engine ◄── Copy-Trade Monitor
        │
        ▼
   Risk Engine (circuit breakers, exposure limits)
        │
        ▼
 ExecutionBackend trait
   ├── PaperBackend (simulated fills vs live book)
   └── LiveBackend (Polymarket CLOB SDK)
        │
        ▼
   SQLite + Metrics HTTP + CLI
        │
        ▼
   Learning Module (bounded auto-tuning)
```

Modules: `config`, `exchange`, `execution`, `strategy`, `copytrade`, `risk`, `learning`, `storage`, `metrics`.

## Requirements

- Rust **1.88+** (MSRV for `polymarket_client_sdk_v2`)
- Network access to Polymarket APIs

## Build

```bash
cd ~/Projects/polymarket-no-bot
cargo build --release
```

## Configuration

Copy and edit [`config/sample.toml`](config/sample.toml). Secrets via environment:

| Variable | Required for LIVE |
|----------|-------------------|
| `POLYMARKET_PRIVATE_KEY` | Yes |
| `POLYMARKET_API_KEY` | Yes |
| `POLYMARKET_API_SECRET` | Yes |
| `POLYMARKET_API_PASSPHRASE` | Yes |

Validate config:

```bash
cargo run -- config validate --path config/sample.toml
```

## Run

### PAPER mode (default, safe)

```bash
POLYMARKET_PRIVATE_KEY=0x0000000000000000000000000000000000000000000000000000000000000001 \
cargo run --release -- run --config config/sample.toml --mode paper
```

### LIVE mode

```bash
POLYMARKET_PRIVATE_KEY=0x... \
POLYMARKET_API_KEY=... \
POLYMARKET_API_SECRET=... \
POLYMARKET_API_PASSPHRASE=... \
cargo run --release -- run --config config/sample.toml --mode live
```

## Monitor

```bash
# CLI status table
cargo run -- status --config config/sample.toml

# JSON output
cargo run -- status --config config/sample.toml --json

# Web dashboard (while bot is running)
open http://127.0.0.1:8080/dashboard

# Prometheus metrics
curl http://127.0.0.1:8080/metrics
```

## Strategy

- Scans binary markets via Gamma API where `enableOrderBook = true`
- Buys **NO** when price is in `[0.75, 0.99]` (configurable), expiry ≥ N days, liquidity sufficient
- Diversifies across categories (crypto, macro, politics) and underlyings
- Copy-trades configured wallets via Data API polling

## Risk Management

Configured in `[risk]` section. The risk engine:

- Caps per-trade, per-market, per-category, and per-asset exposure
- Triggers daily loss circuit breaker (blocks new entries)
- Triggers drawdown circuit breaker (disables LIVE → PAPER)

## Self-Learning

The `learning` module analyzes resolved trades by price/expiry buckets and adjusts **whitelisted** parameters within hard bounds. All changes are logged to SQLite and visible on `/dashboard`.

## Extending

### Copy-traded wallets

Add to `config/sample.toml`:

```toml
[[copytrade.wallets]]
address = "0xYourWallet"
tier = 1
scale_factor = 0.10
max_daily_exposure_usd = 500.0
min_trade_size_usd = 25.0
allowed_categories = ["crypto", "macro"]
blocked_categories = ["sports"]
```

### Strategy filters

Edit `[strategy]` and `[risk]` in TOML. Optional Rust heuristics in `src/strategy/filters.rs`.

### Learning heuristics

Add rules in `src/learning/rules.rs` and register in `src/learning/analyzer.rs`.

### On-chain copy-trading

Implement `src/copytrade/chain.rs` as a lower-latency alternative to Data API polling.

## Polymarket Integration Points

Live order placement stubs are marked with `// POLYMARKET_INTEGRATION:` in `src/execution/live.rs` and `src/exchange/`. Wire the official SDK order builder when credentials are available.

## Tests

```bash
cargo test
```

## License

MIT
