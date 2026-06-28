-- Copy wallets table for managing copytrade targets
CREATE TABLE IF NOT EXISTS copy_wallets (
    address TEXT PRIMARY KEY,
    label TEXT,
    scale_factor REAL NOT NULL DEFAULT 0.1,
    max_daily_exposure_usd REAL NOT NULL DEFAULT 500.0,
    min_trade_size_usd REAL NOT NULL DEFAULT 25.0,
    allowed_categories TEXT NOT NULL DEFAULT '[]',
    blocked_categories TEXT NOT NULL DEFAULT '[]',
    source TEXT NOT NULL DEFAULT 'manual',
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_copy_wallets_source ON copy_wallets(source);
CREATE INDEX IF NOT EXISTS idx_copy_wallets_enabled ON copy_wallets(enabled);
