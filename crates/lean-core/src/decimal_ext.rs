use rust_decimal::Decimal;
use rust_decimal_macros::dec;

pub trait DecimalExt {
    fn is_zero_approx(&self) -> bool;
    fn round_to(&self, places: u32) -> Decimal;
    fn clamp_min(&self, min: Decimal) -> Decimal;
    fn clamp_max(&self, max: Decimal) -> Decimal;
}

impl DecimalExt for Decimal {
    fn is_zero_approx(&self) -> bool {
        self.abs() < dec!(0.00000001)
    }

    fn round_to(&self, places: u32) -> Decimal {
        self.round_dp(places)
    }

    fn clamp_min(&self, min: Decimal) -> Decimal {
        if *self < min {
            min
        } else {
            *self
        }
    }

    fn clamp_max(&self, max: Decimal) -> Decimal {
        if *self > max {
            max
        } else {
            *self
        }
    }
}
