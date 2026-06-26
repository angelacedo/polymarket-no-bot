CREATE TABLE IF NOT EXISTS trades (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL,
    mode TEXT NOT NULL,
    market_id TEXT NOT NULL,
    category TEXT NOT NULL,
    underlying TEXT NOT NULL,
    expiry TEXT NOT NULL,
    side TEXT NOT NULL,
    entry_price REAL NOT NULL,
    size_shares REAL NOT NULL,
    source TEXT NOT NULL,
    copy_wallet TEXT,
    exit_price REAL,
    realized_pnl REAL
);

CREATE TABLE IF NOT EXISTS orders (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    client_order_id TEXT NOT NULL,
    order_id TEXT NOT NULL,
    token_id TEXT NOT NULL,
    condition_id TEXT NOT NULL,
    side TEXT NOT NULL,
    limit_price REAL NOT NULL,
    size_shares REAL NOT NULL,
    filled_shares REAL NOT NULL,
    avg_fill_price REAL NOT NULL,
    status TEXT NOT NULL,
    mode TEXT NOT NULL,
    source TEXT NOT NULL,
    category TEXT NOT NULL,
    underlying TEXT NOT NULL,
    signal_to_order_ms INTEGER NOT NULL,
    fill_latency_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS positions_snapshot (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL,
    condition_id TEXT NOT NULL,
    token_id TEXT NOT NULL,
    side TEXT NOT NULL,
    size_shares REAL NOT NULL,
    avg_entry_price REAL NOT NULL,
    category TEXT NOT NULL,
    underlying TEXT NOT NULL,
    source TEXT NOT NULL,
    copy_wallet TEXT,
    mode TEXT NOT NULL,
    unrealized_pnl REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS equity_curve (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL,
    mode TEXT NOT NULL,
    realized_pnl REAL NOT NULL,
    unrealized_pnl REAL NOT NULL,
    equity REAL NOT NULL,
    peak_equity REAL NOT NULL,
    daily_pnl REAL NOT NULL,
    drawdown_fraction REAL NOT NULL
);

CREATE TABLE IF NOT EXISTS tuning_audit (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    ts TEXT NOT NULL,
    parameter TEXT NOT NULL,
    old_value TEXT NOT NULL,
    new_value TEXT NOT NULL,
    reason TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS markets_cache (
    condition_id TEXT PRIMARY KEY,
    question TEXT NOT NULL,
    category TEXT NOT NULL,
    underlying TEXT NOT NULL,
    end_date TEXT NOT NULL,
    yes_token TEXT NOT NULL,
    no_token TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    slug TEXT
);

CREATE INDEX IF NOT EXISTS idx_trades_ts ON trades(ts);
CREATE INDEX IF NOT EXISTS idx_orders_created ON orders(created_at);
CREATE INDEX IF NOT EXISTS idx_equity_ts ON equity_curve(ts);
