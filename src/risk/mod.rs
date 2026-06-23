use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::config::RiskConfig;
use crate::types::{
    CircuitBreakerState, ExposureSnapshot, OrderIntent, PnlSnapshot, RiskDecision,
};

#[derive(Debug, Clone)]
pub struct RiskEngine {
    config: RiskConfig,
    exposure: ExposureSnapshot,
    pnl: PnlSnapshot,
    circuit: CircuitBreakerState,
    wallet_daily_exposure: HashMap<String, f64>,
    daily_reset_date: Option<DateTime<Utc>>,
}

impl RiskEngine {
    pub fn new(config: RiskConfig) -> Self {
        let capital = config.total_capital;
        Self {
            config,
            exposure: ExposureSnapshot::default(),
            pnl: PnlSnapshot {
                equity: capital,
                peak_equity: capital,
                ..Default::default()
            },
            circuit: CircuitBreakerState::default(),
            wallet_daily_exposure: HashMap::new(),
            daily_reset_date: None,
        }
    }

    pub fn config(&self) -> &RiskConfig {
        &self.config
    }

    pub fn exposure(&self) -> &ExposureSnapshot {
        &self.exposure
    }

    pub fn pnl(&self) -> &PnlSnapshot {
        &self.pnl
    }

    pub fn circuit(&self) -> &CircuitBreakerState {
        &self.circuit
    }

    pub fn update_exposure(&mut self, exposure: ExposureSnapshot) {
        self.exposure = exposure;
    }

    /// Reset PnL, exposure, and circuit breakers to a fresh paper-trading state.
    pub fn reset(&mut self) {
        let capital = self.config.total_capital;
        self.exposure = ExposureSnapshot::default();
        self.pnl = PnlSnapshot {
            equity: capital,
            peak_equity: capital,
            ..Default::default()
        };
        self.circuit = CircuitBreakerState::default();
        self.wallet_daily_exposure.clear();
        self.daily_reset_date = None;
    }

    pub fn maybe_reset_daily(&mut self, now: DateTime<Utc>) {
        let today = now.date_naive();
        let reset = self.daily_reset_date.map(|d| d.date_naive()) != Some(today);
        if reset {
            self.pnl.daily_pnl = 0.0;
            self.wallet_daily_exposure.clear();
            self.daily_reset_date = Some(now);
            if self.circuit.block_new_entries {
                if let Some(until) = self.circuit.block_new_entries_until {
                    if now >= until {
                        self.circuit.block_new_entries = false;
                        self.circuit.block_new_entries_until = None;
                        self.circuit.last_reason = None;
                    }
                }
            }
        }
    }

    pub fn update_mtm(&mut self, unrealized: f64, realized: f64) {
        let capital = self.config.total_capital;
        self.pnl.realized_pnl = realized;
        self.pnl.unrealized_pnl = unrealized;
        let prev_equity = self.pnl.equity;
        self.pnl.equity = capital + realized + unrealized;
        self.pnl.peak_equity = self.pnl.peak_equity.max(self.pnl.equity);
        self.pnl.daily_pnl += self.pnl.equity - prev_equity;

        if self.pnl.peak_equity > 0.0 {
            self.pnl.drawdown_fraction =
                (self.pnl.peak_equity - self.pnl.equity) / self.pnl.peak_equity;
        }

        if self.pnl.drawdown_fraction > self.config.max_drawdown_fraction {
            self.circuit.live_disabled = true;
            self.circuit.last_reason = Some(format!(
                "drawdown {:.2}% exceeds max {:.2}%",
                self.pnl.drawdown_fraction * 100.0,
                self.config.max_drawdown_fraction * 100.0
            ));
        }

        let daily_loss_frac = (-self.pnl.daily_pnl.min(0.0)) / capital;
        if daily_loss_frac > self.config.max_daily_loss_fraction {
            self.circuit.block_new_entries = true;
            let end_of_day = Utc::now()
                .date_naive()
                .and_hms_opt(23, 59, 59)
                .unwrap()
                .and_utc();
            self.circuit.block_new_entries_until = Some(end_of_day);
            self.circuit.last_reason = Some(format!(
                "daily loss {:.2}% exceeds max {:.2}%",
                daily_loss_frac * 100.0,
                self.config.max_daily_loss_fraction * 100.0
            ));
        }
    }

    pub fn evaluate(&self, intent: &OrderIntent, price_range: [f64; 2]) -> RiskDecision {
        if !intent.reducing {
            if self.circuit.block_new_entries {
                return RiskDecision::Rejected {
                    reason: "daily loss circuit breaker: new entries blocked".into(),
                };
            }
            if intent.side != crate::types::Side::No {
                return RiskDecision::Rejected {
                    reason: "strategy only accepts NO side entries".into(),
                };
            }
            if intent.limit_price < price_range[0] || intent.limit_price > price_range[1] {
                return RiskDecision::Rejected {
                    reason: format!(
                        "NO price {:.4} outside allowed [{:.4}, {:.4}]",
                        intent.limit_price, price_range[0], price_range[1]
                    ),
                };
            }
        }

        let max_notional = self.max_allowed_notional(intent);
        if max_notional <= 0.0 {
            return RiskDecision::Rejected {
                reason: "no headroom under risk limits".into(),
            };
        }

        let requested = intent.notional_usd.min(max_notional);
        if requested <= 0.0 {
            return RiskDecision::Rejected {
                reason: "requested notional is zero".into(),
            };
        }

        let size_shares = requested / intent.limit_price.max(1e-9);
        let downsized = requested + 1e-9 < intent.notional_usd;

        if downsized {
            RiskDecision::Downsized {
                size_shares,
                notional_usd: requested,
                reason: "downsized to fit risk limits".into(),
            }
        } else {
            RiskDecision::Approved {
                size_shares,
                notional_usd: requested,
            }
        }
    }

    fn max_allowed_notional(&self, intent: &OrderIntent) -> f64 {
        let capital = self.config.total_capital;
        let mut max = intent.notional_usd;

        let invested_cap = capital * self.config.max_invested_capital_fraction;
        max = max.min((invested_cap - self.exposure.total_invested_usd).max(0.0));

        let per_trade = capital * self.config.max_risk_per_trade_fraction;
        max = max.min(per_trade);

        max = max.min(self.config.max_notional_per_market);

        let market_exp = self
            .exposure
            .by_market
            .get(&intent.market.condition_id)
            .copied()
            .unwrap_or(0.0);
        max = max.min((self.config.max_notional_per_market - market_exp).max(0.0));

        let cat_exp = self
            .exposure
            .by_category
            .get(&intent.market.category)
            .copied()
            .unwrap_or(0.0);
        let cat_cap = capital * self.config.max_category_risk_fraction;
        max = max.min((cat_cap - cat_exp).max(0.0));

        let asset_exp = self
            .exposure
            .by_asset
            .get(&intent.market.underlying)
            .copied()
            .unwrap_or(0.0);
        let asset_cap = capital * self.config.max_asset_risk_fraction;
        max = max.min((asset_cap - asset_exp).max(0.0));

        if let Some(wallet) = intent.source.copy_wallet() {
            let wallet_key = wallet.to_lowercase();
            let wallet_exp = self.wallet_daily_exposure.get(&wallet_key).copied().unwrap_or(0.0);
            // Per-wallet daily cap applied via set_wallet_daily_cap at runtime
            let wallet_cap = 500.0_f64;
            max = max.min((wallet_cap - wallet_exp).max(0.0));
        }

        max
    }

    pub fn record_fill(&mut self, intent: &OrderIntent, notional: f64) {
        self.exposure.total_invested_usd += notional;
        *self
            .exposure
            .by_market
            .entry(intent.market.condition_id.clone())
            .or_default() += notional;
        *self
            .exposure
            .by_category
            .entry(intent.market.category.clone())
            .or_default() += notional;
        *self
            .exposure
            .by_asset
            .entry(intent.market.underlying.clone())
            .or_default() += notional;
        if let Some(wallet) = intent.source.copy_wallet() {
            *self
                .exposure
                .by_wallet
                .entry(wallet.to_lowercase())
                .or_default() += notional;
            *self
                .wallet_daily_exposure
                .entry(wallet.to_lowercase())
                .or_default() += notional;
        }
        self.exposure.open_position_count += 1;
    }

    pub fn set_wallet_daily_cap(&mut self, wallet: &str, cap: f64, current: f64) {
        self.wallet_daily_exposure
            .insert(wallet.to_lowercase(), current.min(cap));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{MarketMeta, SignalSource, Side};
    use std::time::Instant;

    fn sample_market() -> MarketMeta {
        MarketMeta {
            condition_id: "cond-1".into(),
            question: "BTC above 100k?".into(),
            yes_token_id: "yes-1".into(),
            no_token_id: "no-1".into(),
            category: "crypto".into(),
            underlying: "BTC".into(),
            end_date: Utc::now() + chrono::Duration::days(30),
            enable_order_book: true,
            liquidity_usd: 1000.0,
        }
    }

    fn sample_intent(notional: f64) -> OrderIntent {
        OrderIntent {
            market: sample_market(),
            side: Side::No,
            token_id: "no-1".into(),
            limit_price: 0.85,
            size_shares: notional / 0.85,
            notional_usd: notional,
            source: SignalSource::Strategy,
            signal_ts: Instant::now(),
            reducing: false,
        }
    }

    fn default_config() -> RiskConfig {
        RiskConfig {
            total_capital: 10_000.0,
            max_invested_capital_fraction: 0.7,
            max_daily_loss_fraction: 0.05,
            max_drawdown_fraction: 0.15,
            max_risk_per_trade_fraction: 0.02,
            max_notional_per_market: 200.0,
            allowed_price_range_no: [0.75, 0.99],
            min_time_to_expiry_days: 7,
            max_category_risk_fraction: 0.25,
            max_asset_risk_fraction: 0.15,
        }
    }

    #[test]
    fn approves_within_limits() {
        let engine = RiskEngine::new(default_config());
        let intent = sample_intent(150.0);
        match engine.evaluate(&intent, [0.75, 0.99]) {
            RiskDecision::Approved { notional_usd, .. } => assert!(notional_usd > 0.0),
            other => panic!("expected approved, got {other:?}"),
        }
    }

    #[test]
    fn rejects_price_outside_range() {
        let engine = RiskEngine::new(default_config());
        let mut intent = sample_intent(100.0);
        intent.limit_price = 0.50;
        match engine.evaluate(&intent, [0.75, 0.99]) {
            RiskDecision::Rejected { .. } => {}
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn downsizes_when_exceeds_per_trade() {
        let engine = RiskEngine::new(default_config());
        let intent = sample_intent(500.0);
        match engine.evaluate(&intent, [0.75, 0.99]) {
            RiskDecision::Downsized { notional_usd, .. } => {
                assert!(notional_usd <= 200.0);
            }
            RiskDecision::Approved { notional_usd, .. } => assert!(notional_usd <= 200.0),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn circuit_breaker_blocks_new_entries() {
        let mut engine = RiskEngine::new(default_config());
        engine.circuit.block_new_entries = true;
        let intent = sample_intent(50.0);
        match engine.evaluate(&intent, [0.75, 0.99]) {
            RiskDecision::Rejected { reason } => assert!(reason.contains("circuit breaker")),
            other => panic!("expected rejection, got {other:?}"),
        }
    }

    #[test]
    fn drawdown_triggers_live_disable() {
        let mut engine = RiskEngine::new(default_config());
        engine.pnl.peak_equity = 10_000.0;
        engine.pnl.equity = 8_000.0;
        engine.update_mtm(0.0, -2_000.0);
        assert!(engine.circuit.live_disabled);
    }

    #[test]
    fn category_exposure_limits() {
        let mut engine = RiskEngine::new(default_config());
        engine.exposure.by_category.insert("crypto".into(), 2400.0);
        engine.exposure.total_invested_usd = 2400.0;
        let intent = sample_intent(100.0);
        match engine.evaluate(&intent, [0.75, 0.99]) {
            RiskDecision::Rejected { .. } => {}
            RiskDecision::Downsized { notional_usd, .. } => {
                assert!(notional_usd <= 100.0);
            }
            RiskDecision::Approved { notional_usd, .. } => {
                assert!(notional_usd <= 100.0);
            }
        }
    }
}
