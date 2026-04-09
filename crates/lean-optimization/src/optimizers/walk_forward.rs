use chrono::{Months, NaiveDate};

pub struct WalkForwardWindow {
    pub in_sample_start: NaiveDate,
    pub in_sample_end: NaiveDate,
    pub out_of_sample_start: NaiveDate,
    pub out_of_sample_end: NaiveDate,
}

pub struct WalkForwardOptimizer {
    pub backtest_start: NaiveDate,
    pub backtest_end: NaiveDate,
    pub in_sample_months: u32,
    pub out_of_sample_months: u32,
}

impl WalkForwardOptimizer {
    pub fn new(
        start: NaiveDate,
        end: NaiveDate,
        in_sample_months: u32,
        out_of_sample_months: u32,
    ) -> Self {
        Self {
            backtest_start: start,
            backtest_end: end,
            in_sample_months,
            out_of_sample_months,
        }
    }

    /// Generate all walk-forward windows
    pub fn windows(&self) -> Vec<WalkForwardWindow> {
        let mut windows = Vec::new();
        let mut is_start = self.backtest_start;
        loop {
            let is_end = is_start + Months::new(self.in_sample_months);
            let oos_start = is_end;
            let oos_end = oos_start + Months::new(self.out_of_sample_months);
            if oos_end > self.backtest_end {
                break;
            }
            windows.push(WalkForwardWindow {
                in_sample_start: is_start,
                in_sample_end: is_end,
                out_of_sample_start: oos_start,
                out_of_sample_end: oos_end,
            });
            is_start = is_start + Months::new(self.out_of_sample_months);
        }
        windows
    }
}
