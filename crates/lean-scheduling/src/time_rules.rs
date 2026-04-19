use lean_core::TimeSpan;

pub enum TimeRule {
    At(TimeSpan),
    AfterMarketOpen { offset: TimeSpan },
    BeforeMarketClose { offset: TimeSpan },
    Every(TimeSpan),
}

pub struct TimeRules;

impl TimeRules {
    pub fn at(hour: u8, minute: u8) -> TimeRule {
        TimeRule::At(TimeSpan::from_secs(hour as i64 * 3600 + minute as i64 * 60))
    }

    pub fn at_midnight() -> TimeRule {
        TimeRules::at(0, 0)
    }
    pub fn at_noon() -> TimeRule {
        TimeRules::at(12, 0)
    }

    pub fn after_market_open(offset_minutes: i64) -> TimeRule {
        TimeRule::AfterMarketOpen {
            offset: TimeSpan::from_mins(offset_minutes),
        }
    }

    pub fn before_market_close(offset_minutes: i64) -> TimeRule {
        TimeRule::BeforeMarketClose {
            offset: TimeSpan::from_mins(offset_minutes),
        }
    }

    pub fn every(minutes: i64) -> TimeRule {
        TimeRule::Every(TimeSpan::from_mins(minutes))
    }
}
