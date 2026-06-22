use chrono::{DateTime, Utc};

use crate::types::TradeRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriceBucket {
    B75_80,
    B80_90,
    B90_95,
    B95_99,
}

impl PriceBucket {
    pub fn from_price(p: f64) -> Option<Self> {
        match p {
            p if (0.75..0.80).contains(&p) => Some(Self::B75_80),
            p if (0.80..0.90).contains(&p) => Some(Self::B80_90),
            p if (0.90..0.95).contains(&p) => Some(Self::B90_95),
            p if (0.95..=0.99).contains(&p) => Some(Self::B95_99),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpiryBucket {
    Under7,
    Days7_30,
    Days30_90,
    Over90,
}

impl ExpiryBucket {
    pub fn from_expiry(expiry: DateTime<Utc>, entry: DateTime<Utc>) -> Self {
        let days = (expiry - entry).num_days();
        if days < 7 {
            Self::Under7
        } else if days < 30 {
            Self::Days7_30
        } else if days < 90 {
            Self::Days30_90
        } else {
            Self::Over90
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct BucketStats {
    pub count: u32,
    pub wins: u32,
    pub total_pnl: f64,
}

impl BucketStats {
    pub fn win_rate(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.wins as f64 / self.count as f64
        }
    }

    pub fn record(&mut self, pnl: f64) {
        self.count += 1;
        if pnl > 0.0 {
            self.wins += 1;
        }
        self.total_pnl += pnl;
    }
}

pub fn aggregate_trades(trades: &[TradeRecord]) -> (Vec<(PriceBucket, BucketStats)>, Vec<(ExpiryBucket, BucketStats)>) {
    use std::collections::HashMap;

    let mut by_price: HashMap<PriceBucket, BucketStats> = HashMap::new();
    let mut by_expiry: HashMap<ExpiryBucket, BucketStats> = HashMap::new();

    for t in trades {
        if let Some(pnl) = t.realized_pnl {
            if let Some(pb) = PriceBucket::from_price(t.entry_price) {
                by_price.entry(pb).or_default().record(pnl);
            }
            let eb = ExpiryBucket::from_expiry(t.expiry, t.ts);
            by_expiry.entry(eb).or_default().record(pnl);
        }
    }

    (
        by_price.into_iter().collect(),
        by_expiry.into_iter().collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExecutionMode, Side};

    #[test]
    fn price_buckets() {
        assert_eq!(PriceBucket::from_price(0.85), Some(PriceBucket::B80_90));
        assert_eq!(PriceBucket::from_price(0.98), Some(PriceBucket::B95_99));
    }

    #[test]
    fn aggregates_win_rate() {
        let trades = vec![TradeRecord {
            id: None,
            ts: Utc::now(),
            mode: ExecutionMode::Paper,
            market_id: "m".into(),
            category: "crypto".into(),
            underlying: "BTC".into(),
            expiry: Utc::now() + chrono::Duration::days(30),
            side: Side::No,
            entry_price: 0.85,
            size_shares: 10.0,
            source: "strategy".into(),
            copy_wallet: None,
            exit_price: Some(1.0),
            realized_pnl: Some(1.5),
        }];
        let (price, _) = aggregate_trades(&trades);
        assert!(!price.is_empty());
    }
}
