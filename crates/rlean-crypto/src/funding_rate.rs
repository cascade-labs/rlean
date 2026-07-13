use chrono::DateTime;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Perpetual futures funding rate payment
#[derive(Debug, Clone)]
pub struct FundingRate {
    pub rate: Decimal,       // e.g. 0.0001 = 0.01% per 8 hours
    pub interval_hours: u32, // typically 8 hours
    pub next_funding_time: DateTime<chrono::Utc>,
}

impl FundingRate {
    /// Default Binance-style 8-hour funding
    pub fn default_binance() -> Self {
        use chrono::Utc;
        Self {
            rate: dec!(0.0001),
            interval_hours: 8,
            next_funding_time: Utc::now(),
        }
    }

    /// Calculate funding payment for a position.
    /// Positive rate -> longs pay shorts; negative -> shorts pay longs.
    pub fn payment(&self, position_size: Decimal, mark_price: Decimal) -> Decimal {
        let notional = position_size.abs() * mark_price;
        let payment = notional * self.rate;
        if position_size > dec!(0) {
            -payment // longs pay
        } else {
            payment // shorts receive
        }
    }

    pub fn is_due(&self, now: DateTime<chrono::Utc>) -> bool {
        now >= self.next_funding_time
    }

    pub fn advance(&mut self) {
        self.next_funding_time += chrono::Duration::hours(self.interval_hours as i64);
    }
}
