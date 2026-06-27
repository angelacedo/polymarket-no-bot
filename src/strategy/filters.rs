use chrono::Utc;

use crate::config::{BotConfig, RiskConfig, StrategyConfig};
use crate::exchange::BookCache;
use crate::types::{MarketMeta, Side, TradeSignal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterReject {
    NoOrderBook,
    PriceOutOfRange,
    ExpiryTooSoon,
    ExpiryTooFar,
    InsufficientLiquidity,
    WrongSide,
}

pub fn apply_filters(
    config: &BotConfig,
    market: &MarketMeta,
    no_ask: f64,
    book_depth_usd: f64,
) -> Result<(), FilterReject> {
    if !market.enable_order_book {
        return Err(FilterReject::NoOrderBook);
    }

    let [min_p, max_p] = config.effective_price_range();
    if no_ask < min_p || no_ask > max_p {
        return Err(FilterReject::PriceOutOfRange);
    }

    // Fractional days so sub-day (hourly / minute) markets are handled
    // precisely instead of being truncated to 0 by integer-day arithmetic.
    let days_left = (market.end_date - Utc::now()).num_seconds() as f64 / 86_400.0;
    if days_left < config.effective_min_time_to_expiry_days() as f64 {
        return Err(FilterReject::ExpiryTooSoon);
    }
    if let Some(max_days) = config.effective_max_time_to_expiry_days() {
        if days_left > max_days {
            return Err(FilterReject::ExpiryTooFar);
        }
    }

    if book_depth_usd < config.strategy.min_liquidity_usd {
        return Err(FilterReject::InsufficientLiquidity);
    }

    Ok(())
}

pub fn compute_size_usd(risk: &RiskConfig, entry_price: f64) -> f64 {
    let capital = risk.total_capital;
    let max_risk = capital * risk.max_risk_per_trade_fraction;
    let shares_at_risk = max_risk / (1.0 - entry_price).max(0.01);
    let notional = shares_at_risk * entry_price;
    notional.min(risk.max_notional_per_market)
}

pub fn scan_market(
    config: &BotConfig,
    cache: &BookCache,
    market: &MarketMeta,
) -> Option<TradeSignal> {
    let book = cache.get(&market.no_token_id)?;
    let no_ask = book.best_ask()?;
    let depth = book.depth_usd_within_band(true, config.strategy.slippage_band);

    apply_filters(config, market, no_ask, depth).ok()?;

    if config.strategy.prefer_no_momentum {
        if let Some(bid) = book.best_bid() {
            if no_ask - bid > config.strategy.slippage_band * 2.0 {
                return None;
            }
        }
    }

    let size = compute_size_usd(&config.risk, no_ask);
    Some(super::signal_from_scan(market.clone(), no_ask, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BookLevel;
    use std::path::Path;
    use std::time::Instant;

    fn test_config() -> BotConfig {
        BotConfig::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("config/sample.toml")).unwrap()
    }

    fn sample_market() -> MarketMeta {
        MarketMeta {
            condition_id: "c1".into(),
            question: "test".into(),
            slug: "test-market".into(),
            yes_token_id: "y1".into(),
            no_token_id: "n1".into(),
            category: "crypto".into(),
            underlying: "BTC".into(),
            end_date: Utc::now() + chrono::Duration::days(30),
            enable_order_book: true,
            liquidity_usd: 1000.0,
        }
    }

    #[test]
    fn rejects_low_no_price() {
        let cfg = test_config();
        assert_eq!(
            apply_filters(&cfg, &sample_market(), 0.50, 1000.0),
            Err(FilterReject::PriceOutOfRange)
        );
    }

    #[test]
    fn accepts_valid_entry() {
        let cfg = test_config();
        assert!(apply_filters(&cfg, &sample_market(), 0.85, 1000.0).is_ok());
    }

    #[test]
    fn rejects_short_dated() {
        let cfg = test_config();
        let mut m = sample_market();
        m.end_date = Utc::now() + chrono::Duration::days(1);
        assert_eq!(
            apply_filters(&cfg, &m, 0.85, 1000.0),
            Err(FilterReject::ExpiryTooSoon)
        );
    }
}
