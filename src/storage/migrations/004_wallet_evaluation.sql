-- Add evaluation columns to copy_wallets table
ALTER TABLE copy_wallets ADD COLUMN last_evaluation_status TEXT;
ALTER TABLE copy_wallets ADD COLUMN last_evaluation_at TEXT;
ALTER TABLE copy_wallets ADD COLUMN consecutive_weak_count INTEGER NOT NULL DEFAULT 0;
