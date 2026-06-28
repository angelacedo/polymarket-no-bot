-- Migration flags table for tracking one-time migrations
CREATE TABLE IF NOT EXISTS migration_flags (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    migrated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
