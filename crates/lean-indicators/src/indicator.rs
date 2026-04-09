use lean_core::{DateTime, Price};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndicatorStatus {
    /// Not enough data yet.
    NotReady,
    /// Ready — output is valid.
    Ready,
}

#[derive(Debug, Clone)]
pub struct IndicatorResult {
    pub value: Price,
    pub time: DateTime,
    pub status: IndicatorStatus,
}

impl IndicatorResult {
    pub fn ready(value: Price, time: DateTime) -> Self {
        IndicatorResult { value, time, status: IndicatorStatus::Ready }
    }

    pub fn not_ready() -> Self {
        use rust_decimal_macros::dec;
        IndicatorResult {
            value: dec!(0),
            time: lean_core::DateTime::EPOCH,
            status: IndicatorStatus::NotReady,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.status == IndicatorStatus::Ready
    }
}

/// Core indicator trait. All indicators implement this.
pub trait Indicator: Send + Sync {
    fn name(&self) -> &str;
    fn is_ready(&self) -> bool;
    fn current(&self) -> IndicatorResult;
    fn samples(&self) -> usize;
    fn warm_up_period(&self) -> usize;
    fn reset(&mut self);

    /// Update with a single price value and time.
    fn update_price(&mut self, time: DateTime, value: Price) -> IndicatorResult;

    /// Update with a full trade bar.
    fn update_bar(&mut self, bar: &lean_data::TradeBar) -> IndicatorResult {
        self.update_price(bar.time, bar.close)
    }
}
