use chrono::Duration;
use lean_core::TimeSpan;
use lean_data::TradeBar;

use crate::consolidator::IConsolidator;

/// How bars are consolidated.
pub enum ConsolidationMode {
    /// Consolidate every N input bars.
    BarCount(usize),
    /// Consolidate on a fixed time-period boundary (e.g., every 5 minutes).
    TimePeriod(Duration),
}

/// Aggregates raw TradeBar data into larger bars by bar count or time period.
/// Mirrors C# TradeBarConsolidator / PeriodCountConsolidatorBase logic.
pub struct TradeBarConsolidator {
    mode: ConsolidationMode,
    /// The bar currently being built.
    working: Option<TradeBar>,
    /// Number of input bars accumulated so far (BarCount mode).
    bar_count: usize,
    /// The period bucket the working bar belongs to (TimePeriod mode).
    /// Stored as i64 nanoseconds divided by the period nanos — i.e. floor(bar.time / period).
    period_bucket: Option<i64>,
}

impl TradeBarConsolidator {
    /// Create a count-based consolidator that emits every `n` bars.
    pub fn new_count(n: usize) -> Self {
        assert!(n > 0, "bar count must be > 0");
        Self {
            mode: ConsolidationMode::BarCount(n),
            working: None,
            bar_count: 0,
            period_bucket: None,
        }
    }

    /// Create a time-period consolidator that emits on period boundaries.
    pub fn new_period(period: Duration) -> Self {
        assert!(
            period.num_nanoseconds().unwrap_or(0) > 0,
            "period must be > 0"
        );
        Self {
            mode: ConsolidationMode::TimePeriod(period),
            working: None,
            bar_count: 0,
            period_bucket: None,
        }
    }

    /// Merge `incoming` into `working`, updating H/L/C/V.
    fn merge_bar(working: &mut TradeBar, incoming: &TradeBar) {
        if incoming.high > working.high {
            working.high = incoming.high;
        }
        if incoming.low < working.low {
            working.low = incoming.low;
        }
        working.close = incoming.close;
        working.volume += incoming.volume;
        working.end_time = incoming.end_time;
        working.period = TimeSpan::from_nanos(working.end_time.0 - working.time.0);
    }

    /// Compute which period bucket a timestamp (nanos) falls into.
    fn bucket_for(nanos: i64, period_nanos: i64) -> i64 {
        // Integer floor division (handles negatives correctly)
        nanos.div_euclid(period_nanos)
    }
}

impl IConsolidator for TradeBarConsolidator {
    fn update(&mut self, bar: &TradeBar) -> Option<TradeBar> {
        match &self.mode {
            ConsolidationMode::BarCount(n) => {
                let n = *n;
                match &mut self.working {
                    None => {
                        self.working = Some(bar.clone());
                        self.bar_count = 1;
                    }
                    Some(w) => {
                        Self::merge_bar(w, bar);
                        self.bar_count += 1;
                    }
                }
                if self.bar_count >= n {
                    self.bar_count = 0;
                    self.working.take()
                } else {
                    None
                }
            }

            ConsolidationMode::TimePeriod(period) => {
                let period_nanos = period.num_nanoseconds().unwrap_or(0);
                let bar_bucket = Self::bucket_for(bar.time.0, period_nanos);

                match &mut self.working {
                    None => {
                        self.working = Some(bar.clone());
                        self.period_bucket = Some(bar_bucket);
                        None
                    }
                    Some(w) => {
                        let current_bucket = self.period_bucket.unwrap_or(bar_bucket);
                        if bar_bucket > current_bucket {
                            // Period boundary crossed — emit current working bar
                            let emitted = self.working.take();
                            // Start new working bar from incoming
                            self.working = Some(bar.clone());
                            self.period_bucket = Some(bar_bucket);
                            emitted
                        } else {
                            // Same period — merge
                            Self::merge_bar(w, bar);
                            None
                        }
                    }
                }
            }
        }
    }

    fn reset(&mut self) {
        self.working = None;
        self.bar_count = 0;
        self.period_bucket = None;
    }

    fn name(&self) -> &str {
        match &self.mode {
            ConsolidationMode::BarCount(_) => "TradeBarConsolidator(BarCount)",
            ConsolidationMode::TimePeriod(_) => "TradeBarConsolidator(TimePeriod)",
        }
    }
}
