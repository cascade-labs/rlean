use lean_core::Symbol;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::portfolio_construction_model::{
    IPortfolioConstructionModel, InsightDirection, InsightForPcm,
};
use crate::portfolio_target::PortfolioTarget;

/// An insight stored with its generation sequence number and expiry epoch.
/// We use a monotonic sequence counter rather than wall-clock time so tests
/// can remain deterministic without touching a real clock.
#[derive(Debug, Clone)]
struct StoredInsight {
    /// Symbol this insight applies to.
    symbol: Symbol,
    /// Insight direction.
    direction: InsightDirection,
    /// Sequence number (lower = generated earlier).
    sequence: u64,
    /// Epoch seconds at which this insight expires (None = never expires).
    expires_at_secs: Option<u64>,
}

/// Accumulates insights over time rather than resetting on each `create_targets` call.
///
/// Mirrors C# `AccumulativeInsightPortfolioConstructionModel`:
///   - Rule 1: On active Up insight, increase position by `percent`.
///   - Rule 2: On active Down insight, decrease position by `percent`.
///   - Rule 3: On active Flat insight, move by `percent` toward 0.
///   - Rule 4: On expired insight (no other active insight for the symbol), emit 0 target.
///
/// # Implementation notes
/// Because the Rust engine does not (yet) maintain a global insight store, this model
/// keeps its own ordered list of insights with optional expiry timestamps.
/// Call `create_targets_at` (or pass expiry via `InsightForPcm::magnitude` as a
/// seconds-from-now value) when accurate expiry tracking is needed.
///
/// The default `create_targets` implementation treats insights as non-expiring unless
/// the caller sets `magnitude` to a positive number of seconds until expiry.
pub struct AccumulativeInsightPortfolioConstructionModel {
    /// Fraction of portfolio value allocated per insight.
    percent: Decimal,
    /// Stored insight history, in arrival order.
    stored: Vec<StoredInsight>,
    /// Monotonic counter used to order insights.
    sequence_counter: u64,
    /// Wall-clock function — returns current epoch seconds.
    /// Defaults to `SystemTime::now()`.  Replaced in tests via `now_fn`.
    now_secs: u64,
    /// Whether the `now_secs` field was set externally (for test determinism).
    use_fixed_now: bool,
}

impl AccumulativeInsightPortfolioConstructionModel {
    /// Create with default 3 % per insight.
    pub fn new() -> Self {
        Self::with_percent(Decimal::new(3, 2))
    }

    /// Create with a custom per-insight allocation fraction.
    pub fn with_percent(percent: Decimal) -> Self {
        Self {
            percent: percent.abs(),
            stored: Vec::new(),
            sequence_counter: 0,
            now_secs: 0,
            use_fixed_now: false,
        }
    }

    /// Override the current time used for expiry checks (useful in tests).
    pub fn set_now_secs(&mut self, secs: u64) {
        self.now_secs = secs;
        self.use_fixed_now = true;
    }

    fn current_secs(&self) -> u64 {
        if self.use_fixed_now {
            self.now_secs
        } else {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs()
        }
    }

    /// Add a single insight.  `expiry_secs` is the absolute epoch second at which the
    /// insight expires; pass `None` for a non-expiring insight.
    pub fn push_insight(
        &mut self,
        symbol: Symbol,
        direction: InsightDirection,
        expiry_secs: Option<u64>,
    ) {
        let seq = self.sequence_counter;
        self.sequence_counter += 1;
        self.stored.push(StoredInsight {
            symbol,
            direction,
            sequence: seq,
            expires_at_secs: expiry_secs,
        });
    }

    /// Compute targets from all currently active stored insights.
    ///
    /// Returns one target per symbol that has at least one recorded insight.
    /// Expired insights are excluded from the accumulation but their symbols still
    /// receive a 0-target if they were previously tracked (mirrors C# Rule 4).
    pub fn compute_targets(
        &self,
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        let now = self.current_secs();

        // Collect all symbols that have ever had an insight stored.
        let mut all_symbols: Vec<Symbol> = Vec::new();
        for s in &self.stored {
            if !all_symbols.iter().any(|sym| sym.value == s.symbol.value) {
                all_symbols.push(s.symbol.clone());
            }
        }

        let mut targets = Vec::new();

        for sym in &all_symbols {
            // Gather all stored insights for this symbol in sequence order.
            let mut sym_insights: Vec<&StoredInsight> = self
                .stored
                .iter()
                .filter(|s| s.symbol.value == sym.value)
                .collect();
            sym_insights.sort_by_key(|s| s.sequence);

            // Process in generation order to accumulate the target percent.
            // Mirrors C# DetermineTargetPercent logic.
            let mut pct_per_symbol: Option<Decimal> = None;

            for si in &sym_insights {
                // Skip expired insights.
                if let Some(exp) = si.expires_at_secs {
                    if now >= exp {
                        continue;
                    }
                }

                let current = pct_per_symbol.get_or_insert(Decimal::ZERO);

                match si.direction {
                    InsightDirection::Flat => {
                        // Move toward 0 by percent.
                        if current.abs() < self.percent {
                            *current = Decimal::ZERO;
                        } else if *current > Decimal::ZERO {
                            *current -= self.percent;
                        } else {
                            *current += self.percent;
                        }
                    }
                    InsightDirection::Up => {
                        *current += self.percent;
                    }
                    InsightDirection::Down => {
                        *current -= self.percent;
                    }
                }
            }

            let pct = pct_per_symbol.unwrap_or(Decimal::ZERO);
            let price = prices.get(&sym.value).copied().unwrap_or(Decimal::ZERO);
            targets.push(PortfolioTarget::percent(
                sym.clone(),
                pct,
                portfolio_value,
                price,
            ));
        }

        targets
    }
}

impl Default for AccumulativeInsightPortfolioConstructionModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IPortfolioConstructionModel for AccumulativeInsightPortfolioConstructionModel {
    /// Ingest new insights (appended to the stored list) then compute targets from the
    /// full accumulated set.
    ///
    /// To enable expiry, encode the insight lifetime in seconds as a positive
    /// `magnitude` value; the model interprets it as `now + magnitude` seconds.
    /// If `magnitude` is None or <= 0, the insight never expires.
    fn create_targets(
        &mut self,
        insights: &[InsightForPcm],
        portfolio_value: Decimal,
        prices: &HashMap<String, Decimal>,
    ) -> Vec<PortfolioTarget> {
        let now = self.current_secs();

        for insight in insights {
            let expiry = insight.magnitude.and_then(|m| {
                if m > Decimal::ZERO {
                    let secs = m.try_into().unwrap_or(0u64);
                    Some(now + secs)
                } else {
                    None
                }
            });
            self.push_insight(insight.symbol.clone(), insight.direction, expiry);
        }

        self.compute_targets(portfolio_value, prices)
    }

    fn name(&self) -> &str {
        "AccumulativeInsightPortfolioConstructionModel"
    }

    fn on_securities_changed(&mut self, _added: &[Symbol], _removed: &[Symbol]) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{Market, Symbol};
    use rust_decimal_macros::dec;

    fn make_symbol(ticker: &str) -> Symbol {
        Symbol::create_equity(ticker, &Market::usa())
    }

    fn make_prices(pairs: &[(&str, Decimal)]) -> HashMap<String, Decimal> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    /// 3 % of 100_000 at price 100 = 3000 → qty = 30
    const PORTFOLIO: Decimal = dec!(100_000);

    #[test]
    fn insights_accumulate_across_calls() {
        // Each call with an Up insight for the same symbol should add percent to the target.
        let mut pcm = AccumulativeInsightPortfolioConstructionModel::new();
        let prices = make_prices(&[("SPY", dec!(100))]);
        let spy = make_symbol("SPY");

        // First Up insight → pct = 0.03, qty = 30
        let targets = pcm.create_targets(
            &[InsightForPcm {
                symbol: spy.clone(),
                direction: InsightDirection::Up,
                magnitude: None,
                confidence: None,
                source_model: "test".to_string(),
            }],
            PORTFOLIO,
            &prices,
        );
        let qty1 = targets
            .iter()
            .find(|t| t.symbol.value == "SPY")
            .unwrap()
            .quantity;
        assert_eq!(qty1, dec!(30), "first Up: expected qty=30, got {}", qty1);

        // Second Up insight → pct = 0.06, qty = 60
        let targets = pcm.create_targets(
            &[InsightForPcm {
                symbol: spy.clone(),
                direction: InsightDirection::Up,
                magnitude: None,
                confidence: None,
                source_model: "test".to_string(),
            }],
            PORTFOLIO,
            &prices,
        );
        let qty2 = targets
            .iter()
            .find(|t| t.symbol.value == "SPY")
            .unwrap()
            .quantity;
        assert_eq!(qty2, dec!(60), "second Up: expected qty=60, got {}", qty2);
    }

    #[test]
    fn flat_insight_reduces_accumulation() {
        // Two Up then one Flat should reduce by one step.
        let mut pcm = AccumulativeInsightPortfolioConstructionModel::new();
        let prices = make_prices(&[("SPY", dec!(100))]);
        let spy = make_symbol("SPY");

        // Accumulate to pct = 0.06
        pcm.push_insight(spy.clone(), InsightDirection::Up, None);
        pcm.push_insight(spy.clone(), InsightDirection::Up, None);
        // Flat: reduce to 0.03
        pcm.push_insight(spy.clone(), InsightDirection::Flat, None);

        let targets = pcm.compute_targets(PORTFOLIO, &prices);
        let qty = targets
            .iter()
            .find(|t| t.symbol.value == "SPY")
            .unwrap()
            .quantity;
        assert_eq!(qty, dec!(30), "after Flat: expected qty=30, got {}", qty);

        // Second Flat: reduce to 0
        pcm.push_insight(spy.clone(), InsightDirection::Flat, None);
        let targets = pcm.compute_targets(PORTFOLIO, &prices);
        let qty = targets
            .iter()
            .find(|t| t.symbol.value == "SPY")
            .unwrap()
            .quantity;
        assert_eq!(
            qty,
            dec!(0),
            "after second Flat: expected qty=0, got {}",
            qty
        );
    }

    #[test]
    fn expired_insight_is_excluded_from_accumulation() {
        let mut pcm = AccumulativeInsightPortfolioConstructionModel::new();
        pcm.set_now_secs(1_000);
        let prices = make_prices(&[("SPY", dec!(100))]);
        let spy = make_symbol("SPY");

        // First insight: expires at t=500 (already expired at t=1000)
        pcm.push_insight(spy.clone(), InsightDirection::Up, Some(500));
        // Second insight: expires at t=2000 (still active)
        pcm.push_insight(spy.clone(), InsightDirection::Up, Some(2_000));

        let targets = pcm.compute_targets(PORTFOLIO, &prices);
        let qty = targets
            .iter()
            .find(|t| t.symbol.value == "SPY")
            .unwrap()
            .quantity;
        // Only the second insight is active → pct=0.03, qty=30
        assert_eq!(
            qty,
            dec!(30),
            "expired insight excluded: expected qty=30, got {}",
            qty
        );
    }

    #[test]
    fn insights_for_different_symbols_are_independent() {
        let mut pcm = AccumulativeInsightPortfolioConstructionModel::new();
        let prices = make_prices(&[("SPY", dec!(100)), ("IBM", dec!(100))]);
        let spy = make_symbol("SPY");
        let ibm = make_symbol("IBM");

        // SPY: two Up; IBM: one Down
        pcm.push_insight(spy.clone(), InsightDirection::Up, None);
        pcm.push_insight(spy.clone(), InsightDirection::Up, None);
        pcm.push_insight(ibm.clone(), InsightDirection::Down, None);

        let targets = pcm.compute_targets(PORTFOLIO, &prices);
        let spy_qty = targets
            .iter()
            .find(|t| t.symbol.value == "SPY")
            .unwrap()
            .quantity;
        let ibm_qty = targets
            .iter()
            .find(|t| t.symbol.value == "IBM")
            .unwrap()
            .quantity;

        assert_eq!(spy_qty, dec!(60), "SPY: expected 60, got {}", spy_qty);
        assert_eq!(ibm_qty, dec!(-30), "IBM: expected -30, got {}", ibm_qty);
    }
}
