use chrono::Datelike;
use lean_core::TimeSpan;
use lean_data::TradeBar;

use crate::consolidator::IConsolidator;

/// Calendar period granularity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarPeriod {
    Daily,
    Weekly,
    Monthly,
    Quarterly,
    Annual,
}

/// Calendar-based consolidator.
/// Emits a bar when the calendar period boundary is crossed (e.g., day/week/month).
pub struct CalendarConsolidator {
    period: CalendarPeriod,
    working: Option<TradeBar>,
    /// The period key for the working bar (day-of-year, week, month, quarter, or year integer).
    period_key: Option<i64>,
}

impl CalendarConsolidator {
    pub fn new(period: CalendarPeriod) -> Self {
        Self {
            period,
            working: None,
            period_key: None,
        }
    }

    /// Compute an integer "key" that identifies which calendar bucket a timestamp belongs to.
    fn period_key_for(&self, bar: &TradeBar) -> i64 {
        let date = bar.time.date_utc();
        match self.period {
            CalendarPeriod::Daily => {
                // Unique per calendar day: YYYYMMDD
                date.year() as i64 * 10_000 + date.month() as i64 * 100 + date.day() as i64
            }
            CalendarPeriod::Weekly => {
                // ISO week: year * 100 + week_number
                let iso = date.iso_week();
                iso.year() as i64 * 100 + iso.week() as i64
            }
            CalendarPeriod::Monthly => date.year() as i64 * 100 + date.month() as i64,
            CalendarPeriod::Quarterly => {
                let q = ((date.month() - 1) / 3 + 1) as i64;
                date.year() as i64 * 10 + q
            }
            CalendarPeriod::Annual => date.year() as i64,
        }
    }
}

impl IConsolidator for CalendarConsolidator {
    fn update(&mut self, bar: &TradeBar) -> Option<TradeBar> {
        let key = self.period_key_for(bar);

        match &mut self.working {
            None => {
                self.working = Some(bar.clone());
                self.period_key = Some(key);
                None
            }
            Some(w) => {
                let current_key = self.period_key.unwrap_or(key);
                if key != current_key {
                    // Period boundary — emit current, start fresh
                    let emitted = self.working.take();
                    self.working = Some(bar.clone());
                    self.period_key = Some(key);
                    emitted
                } else {
                    // Same period — merge
                    if bar.high > w.high {
                        w.high = bar.high;
                    }
                    if bar.low < w.low {
                        w.low = bar.low;
                    }
                    w.close = bar.close;
                    w.volume += bar.volume;
                    w.end_time = bar.end_time;
                    w.period = TimeSpan::from_nanos(w.end_time.0 - w.time.0);
                    None
                }
            }
        }
    }

    fn reset(&mut self) {
        self.working = None;
        self.period_key = None;
    }

    fn name(&self) -> &str {
        match self.period {
            CalendarPeriod::Daily => "CalendarConsolidator(Daily)",
            CalendarPeriod::Weekly => "CalendarConsolidator(Weekly)",
            CalendarPeriod::Monthly => "CalendarConsolidator(Monthly)",
            CalendarPeriod::Quarterly => "CalendarConsolidator(Quarterly)",
            CalendarPeriod::Annual => "CalendarConsolidator(Annual)",
        }
    }
}
