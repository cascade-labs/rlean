use rlean_core::{Symbol, TimeSpan};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeRule {
    At {
        hour: u32,
        minute: u32,
    },
    AfterMarketOpen {
        minutes_after_open: i64,
    },
    BeforeMarketClose {
        symbol: Symbol,
        minutes_before_close: i64,
        extended_market_close: bool,
    },
    Every(TimeSpan),
    EveryResolution,
}

pub struct TimeRules;

impl TimeRules {
    pub fn at(hour: u8, minute: u8) -> TimeRule {
        TimeRule::At {
            hour: u32::from(hour),
            minute: u32::from(minute),
        }
    }

    pub fn at_midnight() -> TimeRule {
        TimeRules::at(0, 0)
    }
    pub fn at_noon() -> TimeRule {
        TimeRules::at(12, 0)
    }

    pub fn after_market_open(offset_minutes: i64) -> TimeRule {
        TimeRule::AfterMarketOpen {
            minutes_after_open: offset_minutes,
        }
    }

    pub fn before_market_close(symbol: Symbol, offset_minutes: i64) -> TimeRule {
        TimeRule::BeforeMarketClose {
            symbol,
            minutes_before_close: offset_minutes,
            extended_market_close: false,
        }
    }

    pub fn every(minutes: i64) -> TimeRule {
        assert!(
            minutes > 0,
            "TimeRules::every(): interval can not be zero or less"
        );
        TimeRule::Every(TimeSpan::from_mins(minutes))
    }
}
