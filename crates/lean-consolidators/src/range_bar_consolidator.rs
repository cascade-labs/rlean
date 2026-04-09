use lean_core::TimeSpan;
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::consolidator::IConsolidator;

/// Range bar consolidator.
///
/// Emits a bar when the High - Low range of the working bar reaches or exceeds `range`.
/// Unlike Renko, range bars are directionally neutral — they measure the full H-L spread,
/// not the directional price move.
///
/// Algorithm:
/// 1. The first tick initialises the working bar (H = L = close of first tick).
/// 2. Each subsequent tick updates H/L/Close of the working bar.
/// 3. When (H - L) >= range, the working bar is emitted and a new one starts from the
///    current tick's close (so H = L = close, carrying no state from the old bar).
pub struct RangeBarConsolidator {
    range: Decimal,
    working: Option<TradeBar>,
}

impl RangeBarConsolidator {
    pub fn new(range: Decimal) -> Self {
        assert!(range > dec!(0), "range must be > 0");
        Self { range, working: None }
    }
}

impl IConsolidator for RangeBarConsolidator {
    fn update(&mut self, bar: &TradeBar) -> Option<TradeBar> {
        match &mut self.working {
            None => {
                // Seed a new working bar with the incoming bar's close as a single point.
                let mut seed = bar.clone();
                // Normalise to a single-price point so range is 0 at start
                seed.open  = bar.close;
                seed.high  = bar.close;
                seed.low   = bar.close;
                self.working = Some(seed);
                None
            }
            Some(w) => {
                // Update running extremes
                if bar.high > w.high { w.high = bar.high; }
                if bar.low  < w.low  { w.low  = bar.low;  }
                w.close    = bar.close;
                w.volume  += bar.volume;
                w.end_time = bar.end_time;
                w.period   = TimeSpan::from_nanos(w.end_time.0 - w.time.0);

                let current_range = w.high - w.low;
                if current_range >= self.range {
                    let emitted = self.working.take();
                    // Seed next bar from current close
                    let mut seed = bar.clone();
                    seed.open  = bar.close;
                    seed.high  = bar.close;
                    seed.low   = bar.close;
                    self.working = Some(seed);
                    emitted
                } else {
                    None
                }
            }
        }
    }

    fn reset(&mut self) {
        self.working = None;
    }

    fn name(&self) -> &str {
        "RangeBarConsolidator"
    }
}
