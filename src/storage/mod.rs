const INIT_SQL: &str = include_str!("migrations/001_init.sql");

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use crate::types::{
    ExecutionMode, OrderRequest, OrderResult, OrderStatus, PnlSnapshot, Position, Side,
    TuningAuditRecord, TradeRecord,
};

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
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(INIT_SQL)?;
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

    pub fn upsert_market_cache(
        &self,
        condition_id: &str,
        question: &str,
        category: &str,
        underlying: &str,
        end_date: DateTime<Utc>,
        yes_token: &str,
        no_token: &str,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO markets_cache (condition_id, question, category, underlying, end_date, yes_token, no_token, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(condition_id) DO UPDATE SET
               question=excluded.question, category=excluded.category, underlying=excluded.underlying,
               end_date=excluded.end_date, yes_token=excluded.yes_token, no_token=excluded.no_token,
               updated_at=excluded.updated_at",
            params![
                condition_id,
                question,
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
