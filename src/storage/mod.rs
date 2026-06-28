const INIT_SQL: &str = include_str!("migrations/001_init.sql");
const MIGRATION_002_SQL: &str = include_str!("migrations/002_wallets.sql");
const MIGRATION_003_SQL: &str = include_str!("migrations/003_migration_flags.sql");
const MIGRATION_004_SQL: &str = include_str!("migrations/004_wallet_evaluation.sql");

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use crate::types::{
    ExecutionMode, OrderRequest, OrderResult, OrderStatus, PnlSnapshot, Position, Side,
    TradeView, TuningAuditRecord, TradeRecord, WalletRecord, WalletView,
};

const POLYMARKET_EVENT_BASE: &str = "https://polymarket.com/event/";

/// Apply incremental, idempotent schema migrations on top of INIT_SQL.
fn migrate(conn: &Connection) -> Result<()> {
    // Add markets_cache.slug if it doesn't exist yet (older databases).
    let has_slug: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('markets_cache') WHERE name = 'slug'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_slug {
        conn.execute("ALTER TABLE markets_cache ADD COLUMN slug TEXT", [])?;
    }

    // Migration 002: copy_wallets table
    let has_wallets: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='copy_wallets'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_wallets {
        conn.execute_batch(MIGRATION_002_SQL)?;
    }

    // Migration 003: migration_flags table
    let has_flags: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='migration_flags'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_flags {
        conn.execute_batch(MIGRATION_003_SQL)?;
    }

    // Migration 004: wallet evaluation columns
    let has_eval_cols: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('copy_wallets') WHERE name = 'last_evaluation_status'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);
    if !has_eval_cols {
        conn.execute_batch(MIGRATION_004_SQL)?;
    }

    Ok(())
}

#[derive(Clone)]
pub struct Storage {
    conn: Arc<Mutex<Connection>>,
}

impl Storage {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("open db {}", path.display()))?;
        conn.execute_batch(INIT_SQL)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(INIT_SQL)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn insert_order(
        &self,
        req: &OrderRequest,
        result: &OrderResult,
        signal_to_order_ms: u64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO orders (client_order_id, order_id, token_id, condition_id, side, limit_price,
             size_shares, filled_shares, avg_fill_price, status, mode, source, category, underlying,
             signal_to_order_ms, fill_latency_ms, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                req.client_order_id,
                result.order_id,
                req.token_id,
                req.condition_id,
                side_str(req.side),
                req.limit_price,
                req.size_shares,
                result.filled_shares,
                result.avg_fill_price,
                status_str(result.status),
                req.mode.to_string(),
                req.source,
                req.category,
                req.underlying,
                signal_to_order_ms as i64,
                result.latency_ms as i64,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn insert_trade(&self, trade: &TradeRecord) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO trades (ts, mode, market_id, category, underlying, expiry, side, entry_price,
             size_shares, source, copy_wallet, exit_price, realized_pnl)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                trade.ts.to_rfc3339(),
                trade.mode.to_string(),
                trade.market_id,
                trade.category,
                trade.underlying,
                trade.expiry.to_rfc3339(),
                side_str(trade.side),
                trade.entry_price,
                trade.size_shares,
                trade.source,
                trade.copy_wallet,
                trade.exit_price,
                trade.realized_pnl,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_market_cache(
        &self,
        condition_id: &str,
        question: &str,
        slug: &str,
        category: &str,
        underlying: &str,
        end_date: DateTime<Utc>,
        yes_token: &str,
        no_token: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO markets_cache (condition_id, question, slug, category, underlying, end_date, yes_token, no_token, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(condition_id) DO UPDATE SET
               question=excluded.question, slug=excluded.slug, category=excluded.category, underlying=excluded.underlying,
               end_date=excluded.end_date, yes_token=excluded.yes_token, no_token=excluded.no_token,
               updated_at=excluded.updated_at",
            params![
                condition_id,
                question,
                slug,
                category,
                underlying,
                end_date.to_rfc3339(),
                yes_token,
                no_token,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn snapshot_positions(&self, positions: &[Position], unrealized: f64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let ts = Utc::now().to_rfc3339();
        for p in positions {
            conn.execute(
                "INSERT INTO positions_snapshot (ts, condition_id, token_id, side, size_shares, avg_entry_price,
                 category, underlying, source, copy_wallet, mode, unrealized_pnl)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    ts,
                    p.condition_id,
                    p.token_id,
                    side_str(p.side),
                    p.size_shares,
                    p.avg_entry_price,
                    p.category,
                    p.underlying,
                    p.source,
                    p.copy_wallet,
                    p.mode.to_string(),
                    unrealized / positions.len().max(1) as f64,
                ],
            )?;
        }
        Ok(())
    }

    pub fn record_equity(&self, mode: ExecutionMode, pnl: &PnlSnapshot) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO equity_curve (ts, mode, realized_pnl, unrealized_pnl, equity, peak_equity, daily_pnl, drawdown_fraction)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                Utc::now().to_rfc3339(),
                mode.to_string(),
                pnl.realized_pnl,
                pnl.unrealized_pnl,
                pnl.equity,
                pnl.peak_equity,
                pnl.daily_pnl,
                pnl.drawdown_fraction,
            ],
        )?;
        Ok(())
    }

    pub fn insert_tuning_audit(&self, record: &TuningAuditRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO tuning_audit (ts, parameter, old_value, new_value, reason)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                record.ts.to_rfc3339(),
                record.parameter,
                record.old_value,
                record.new_value,
                record.reason,
            ],
        )?;
        Ok(())
    }

    pub fn recent_trades(&self, limit: usize) -> Result<Vec<TradeRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, ts, mode, market_id, category, underlying, expiry, side, entry_price, size_shares,
                    source, copy_wallet, exit_price, realized_pnl
             FROM trades ORDER BY ts DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(TradeRecord {
                id: Some(row.get(0)?),
                ts: parse_ts(row.get::<_, String>(1)?),
                mode: parse_mode(row.get::<_, String>(2)?),
                market_id: row.get(3)?,
                category: row.get(4)?,
                underlying: row.get(5)?,
                expiry: parse_ts(row.get::<_, String>(6)?),
                side: parse_side(row.get::<_, String>(7)?),
                entry_price: row.get(8)?,
                size_shares: row.get(9)?,
                source: row.get(10)?,
                copy_wallet: row.get(11)?,
                exit_price: row.get(12)?,
                realized_pnl: row.get(13)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Recent trades joined with cached market metadata (question + slug) so the
    /// dashboard can show readable names and link to the Polymarket market.
    pub fn recent_trades_enriched(&self, limit: usize) -> Result<Vec<TradeView>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT t.id, t.ts, t.mode, t.market_id, t.category, t.underlying, t.expiry, t.side,
                    t.entry_price, t.size_shares, t.source, t.copy_wallet, t.exit_price, t.realized_pnl,
                    m.question, m.slug
             FROM trades t
             LEFT JOIN markets_cache m ON t.market_id = m.condition_id
             ORDER BY t.ts DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            let trade = TradeRecord {
                id: Some(row.get(0)?),
                ts: parse_ts(row.get::<_, String>(1)?),
                mode: parse_mode(row.get::<_, String>(2)?),
                market_id: row.get(3)?,
                category: row.get(4)?,
                underlying: row.get(5)?,
                expiry: parse_ts(row.get::<_, String>(6)?),
                side: parse_side(row.get::<_, String>(7)?),
                entry_price: row.get(8)?,
                size_shares: row.get(9)?,
                source: row.get(10)?,
                copy_wallet: row.get(11)?,
                exit_price: row.get(12)?,
                realized_pnl: row.get(13)?,
            };
            let question: Option<String> = row.get(14)?;
            let slug: Option<String> = row.get(15)?;
            let market_url = slug
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|s| format!("{POLYMARKET_EVENT_BASE}{s}"));
            Ok(TradeView {
                trade,
                market_name: question.filter(|q| !q.is_empty()),
                market_url,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn recent_tuning(&self, limit: usize) -> Result<Vec<TuningAuditRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ts, parameter, old_value, new_value, reason FROM tuning_audit ORDER BY ts DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok(TuningAuditRecord {
                ts: parse_ts(row.get::<_, String>(0)?),
                parameter: row.get(1)?,
                old_value: row.get(2)?,
                new_value: row.get(3)?,
                reason: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn latest_equity(&self, mode: ExecutionMode) -> Result<Option<PnlSnapshot>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT realized_pnl, unrealized_pnl, equity, peak_equity, daily_pnl, drawdown_fraction
             FROM equity_curve WHERE mode = ?1 ORDER BY ts DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![mode.to_string()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(PnlSnapshot {
                realized_pnl: row.get(0)?,
                unrealized_pnl: row.get(1)?,
                equity: row.get(2)?,
                peak_equity: row.get(3)?,
                daily_pnl: row.get(4)?,
                drawdown_fraction: row.get(5)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn trades_for_analysis(&self) -> Result<Vec<TradeRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, ts, mode, market_id, category, underlying, expiry, side, entry_price, size_shares,
                    source, copy_wallet, exit_price, realized_pnl FROM trades",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(TradeRecord {
                id: Some(row.get(0)?),
                ts: parse_ts(row.get::<_, String>(1)?),
                mode: parse_mode(row.get::<_, String>(2)?),
                market_id: row.get(3)?,
                category: row.get(4)?,
                underlying: row.get(5)?,
                expiry: parse_ts(row.get::<_, String>(6)?),
                side: parse_side(row.get::<_, String>(7)?),
                entry_price: row.get(8)?,
                size_shares: row.get(9)?,
                source: row.get(10)?,
                copy_wallet: row.get(11)?,
                exit_price: row.get(12)?,
                realized_pnl: row.get(13)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn open_position_count(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT condition_id) FROM positions_snapshot
             WHERE ts = (SELECT MAX(ts) FROM positions_snapshot)",
            [],
            |r| r.get(0),
        )?;
        Ok(count as usize)
    }

    /// Wipe ALL persisted state: trading history, statistics, tuning/learning
    /// audit log, and the market discovery cache. Leaves a pristine database.
    /// Note: copy_wallets are NOT deleted as they are configuration, not history.
    pub fn reset_trading_history(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch(
            "DELETE FROM trades;
             DELETE FROM orders;
             DELETE FROM equity_curve;
             DELETE FROM positions_snapshot;
             DELETE FROM tuning_audit;
             DELETE FROM markets_cache;
             VACUUM;",
        )?;
        Ok(())
    }

    // ─── Copy Wallets CRUD ─────────────────────────────────────────────

    pub fn list_wallets(&self) -> Result<Vec<WalletRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT address, label, scale_factor, max_daily_exposure_usd, min_trade_size_usd,
                    allowed_categories, blocked_categories, source, enabled, created_at
             FROM copy_wallets ORDER BY source, created_at",
        )?;
        let rows = stmt.query_map([], |row| {
            let allowed_json: String = row.get(5)?;
            let blocked_json: String = row.get(6)?;
            let enabled_int: i64 = row.get(8)?;
            Ok(WalletRecord {
                address: row.get(0)?,
                label: row.get(1)?,
                scale_factor: row.get(2)?,
                max_daily_exposure_usd: row.get(3)?,
                min_trade_size_usd: row.get(4)?,
                allowed_categories: serde_json::from_str(&allowed_json).unwrap_or_default(),
                blocked_categories: serde_json::from_str(&blocked_json).unwrap_or_default(),
                source: row.get(7)?,
                enabled: enabled_int != 0,
                created_at: parse_ts(row.get(9)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_enabled_wallets(&self) -> Result<Vec<WalletRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT address, label, scale_factor, max_daily_exposure_usd, min_trade_size_usd,
                    allowed_categories, blocked_categories, source, enabled, created_at
             FROM copy_wallets WHERE enabled = 1 ORDER BY source, created_at",
        )?;
        let rows = stmt.query_map([], |row| {
            let allowed_json: String = row.get(5)?;
            let blocked_json: String = row.get(6)?;
            let enabled_int: i64 = row.get(8)?;
            Ok(WalletRecord {
                address: row.get(0)?,
                label: row.get(1)?,
                scale_factor: row.get(2)?,
                max_daily_exposure_usd: row.get(3)?,
                min_trade_size_usd: row.get(4)?,
                allowed_categories: serde_json::from_str(&allowed_json).unwrap_or_default(),
                blocked_categories: serde_json::from_str(&blocked_json).unwrap_or_default(),
                source: row.get(7)?,
                enabled: enabled_int != 0,
                created_at: parse_ts(row.get(9)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn add_wallet(&self, wallet: &WalletRecord) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO copy_wallets (address, label, scale_factor, max_daily_exposure_usd,
             min_trade_size_usd, allowed_categories, blocked_categories, source, enabled, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                wallet.address.to_lowercase(),
                wallet.label,
                wallet.scale_factor,
                wallet.max_daily_exposure_usd,
                wallet.min_trade_size_usd,
                serde_json::to_string(&wallet.allowed_categories).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&wallet.blocked_categories).unwrap_or_else(|_| "[]".into()),
                wallet.source,
                wallet.enabled as i64,
                wallet.created_at.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn remove_wallet(&self, address: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count = conn.execute(
            "DELETE FROM copy_wallets WHERE address = ?1",
            params![address.to_lowercase()],
        )?;
        Ok(count > 0)
    }

    pub fn update_wallet(&self, wallet: &WalletRecord) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count = conn.execute(
            "UPDATE copy_wallets SET label=?1, scale_factor=?2, max_daily_exposure_usd=?3,
             min_trade_size_usd=?4, allowed_categories=?5, blocked_categories=?6,
             enabled=?7 WHERE address=?8",
            params![
                wallet.label,
                wallet.scale_factor,
                wallet.max_daily_exposure_usd,
                wallet.min_trade_size_usd,
                serde_json::to_string(&wallet.allowed_categories).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&wallet.blocked_categories).unwrap_or_else(|_| "[]".into()),
                wallet.enabled as i64,
                wallet.address.to_lowercase(),
            ],
        )?;
        Ok(count > 0)
    }

    pub fn wallet_exists(&self, address: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM copy_wallets WHERE address = ?1",
            params![address.to_lowercase()],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn wallet_stats(&self) -> Result<Vec<(String, u32, f64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT copy_wallet, COUNT(*) as trades, COALESCE(SUM(realized_pnl), 0.0) as total_pnl
             FROM trades
             WHERE source LIKE 'copy:%'
             GROUP BY copy_wallet",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn list_wallets_with_stats(&self) -> Result<Vec<WalletView>> {
        let wallets = self.list_wallets()?;
        let stats = self.wallet_stats()?;

        let stats_map: std::collections::HashMap<String, (u32, f64)> = stats
            .into_iter()
            .map(|(addr, trades, pnl)| (addr.to_lowercase(), (trades, pnl)))
            .collect();

        Ok(wallets
            .into_iter()
            .map(|w| {
                let (trades_copied, total_pnl) = stats_map
                    .get(&w.address.to_lowercase())
                    .copied()
                    .unwrap_or((0, 0.0));
                WalletView {
                    wallet: w,
                    trades_copied,
                    total_pnl,
                }
            })
            .collect())
    }

    // ─── Migration Flags ─────────────────────────────────────────────

    /// Check if a migration flag exists in the database
    pub fn migration_flag_exists(&self, key: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM migration_flags WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )?;
        Ok(count > 0)
    }

    /// Set a migration flag in the database
    pub fn set_migration_flag(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO migration_flags (key, value, migrated_at) VALUES (?1, ?2, ?3)",
            params![key, value, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    // ─── Wallet Evaluation ───────────────────────────────────────────

    /// Update wallet evaluation status
    pub fn update_wallet_evaluation(
        &self,
        address: &str,
        status: &str,
        consecutive_weak_count: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE copy_wallets SET last_evaluation_status = ?1, last_evaluation_at = ?2, consecutive_weak_count = ?3 WHERE address = ?4",
            params![status, Utc::now().to_rfc3339(), consecutive_weak_count, address.to_lowercase()],
        )?;
        Ok(())
    }

    /// Get wallets that need evaluation (manual wallets without recent evaluation)
    pub fn get_wallets_for_evaluation(&self) -> Result<Vec<WalletRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT address, label, scale_factor, max_daily_exposure_usd, min_trade_size_usd,
                    allowed_categories, blocked_categories, source, enabled, created_at
             FROM copy_wallets WHERE source = 'manual' AND enabled = 1",
        )?;
        let rows = stmt.query_map([], |row| {
            let allowed_json: String = row.get(5)?;
            let blocked_json: String = row.get(6)?;
            let enabled_int: i64 = row.get(8)?;
            Ok(WalletRecord {
                address: row.get(0)?,
                label: row.get(1)?,
                scale_factor: row.get(2)?,
                max_daily_exposure_usd: row.get(3)?,
                min_trade_size_usd: row.get(4)?,
                allowed_categories: serde_json::from_str(&allowed_json).unwrap_or_default(),
                blocked_categories: serde_json::from_str(&blocked_json).unwrap_or_default(),
                source: row.get(7)?,
                enabled: enabled_int != 0,
                created_at: parse_ts(row.get(9)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

fn side_str(side: Side) -> &'static str {
    match side {
        Side::Yes => "yes",
        Side::No => "no",
    }
}

fn status_str(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::Filled => "filled",
        OrderStatus::PartiallyFilled => "partial",
        OrderStatus::Rejected => "rejected",
        OrderStatus::Pending => "pending",
    }
}

fn parse_side(s: String) -> Side {
    if s.eq_ignore_ascii_case("yes") {
        Side::Yes
    } else {
        Side::No
    }
}

fn parse_mode(s: String) -> ExecutionMode {
    if s.eq_ignore_ascii_case("live") {
        ExecutionMode::Live
    } else {
        ExecutionMode::Paper
    }
}

fn parse_ts(s: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}
