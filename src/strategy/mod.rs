mod filters;
mod scanner;

pub use filters::{apply_filters, compute_size_usd, scan_market};
pub use scanner::{StrategyEngine, run_strategy_loop};

use std::time::Instant;

use crate::types::{MarketMeta, Side, SignalSource, TradeSignal};

pub fn signal_from_copy(
    market: MarketMeta,
    entry_price: f64,
    stake_usd: f64,
    wallet: &str,
) -> TradeSignal {
    TradeSignal {
        market,
        side: Side::No, // INVARIANT: always NO
        entry_price,
        suggested_size_usd: stake_usd,
        source: SignalSource::Copy {
            wallet: wallet.to_string(),
        },
        signal_ts: Instant::now(),
    }
}

pub fn signal_from_scan(market: MarketMeta, entry_price: f64, size_usd: f64) -> TradeSignal {
    TradeSignal {
        market,
        side: Side::No, // INVARIANT: always NO
        entry_price,
        suggested_size_usd: size_usd,
        source: SignalSource::Strategy,
        signal_ts: Instant::now(),
    }
}
