use lean_core::TimeSpan;
use lean_data::TradeBar;

use crate::consolidator::IConsolidator;

/// Tick-count consolidator.
///
/// Groups individual ticks (or tick-like TradeBar snapshots) into bars of `ticks_per_bar` ticks.
/// Mirrors C# TickConsolidator (count-based mode): only trade ticks are processed;
/// each incoming bar is treated as one tick event and merged into the working bar.
pub struct TickConsolidator {
    ticks_per_bar: usize,
    working: Option<TradeBar>,
    tick_count: usize,
}

impl TickConsolidator {
    pub fn new(ticks_per_bar: usize) -> Self {
        assert!(ticks_per_bar > 0, "ticks_per_bar must be > 0");
        Self {
            ticks_per_bar,
            working: None,
            tick_count: 0,
        }
    }
}

impl IConsolidator for TickConsolidator {
    fn update(&mut self, bar: &TradeBar) -> Option<TradeBar> {
        match &mut self.working {
            None => {
                self.working = Some(bar.clone());
                self.tick_count = 1;
            }
            Some(w) => {
                if bar.high > w.high { w.high = bar.high; }
                if bar.low  < w.low  { w.low  = bar.low;  }
                w.close = bar.close;
                w.volume += bar.volume;
                w.end_time = bar.end_time;
                w.period = TimeSpan::from_nanos(w.end_time.0 - w.time.0);
                self.tick_count += 1;
            }
        }

        if self.tick_count >= self.ticks_per_bar {
            self.tick_count = 0;
            self.working.take()
        } else {
            None
        }
    }

    fn reset(&mut self) {
        self.working = None;
        self.tick_count = 0;
    }

    fn name(&self) -> &str {
        "TickConsolidator"
    }
}
