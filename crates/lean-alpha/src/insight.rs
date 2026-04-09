use lean_core::{DateTime, NanosecondTimestamp, Symbol, TimeSpan};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InsightType {
    /// Directional price prediction.
    Price,
    /// Volatility prediction.
    Volatility,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InsightDirection {
    Up = 1,
    Flat = 0,
    Down = -1,
}

/// A single alpha signal/prediction. Mirrors C# `Insight`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Insight {
    pub id: u64,
    pub symbol: Symbol,
    pub insight_type: InsightType,
    pub direction: InsightDirection,
    /// How long this insight is valid (nanoseconds via TimeSpan).
    pub period: TimeSpan,
    /// Predicted % move (optional).
    pub magnitude: Option<Decimal>,
    /// Confidence in [0, 1] (optional).
    pub confidence: Option<Decimal>,
    pub source_model: String,
    pub generated_time_utc: DateTime,
    pub close_time_utc: DateTime,
    /// Filled in by a scoring layer after the fact.
    pub score: Option<Decimal>,
}

impl Insight {
    pub fn new(
        symbol: Symbol,
        direction: InsightDirection,
        period: TimeSpan,
        magnitude: Option<Decimal>,
        confidence: Option<Decimal>,
        source_model: &str,
    ) -> Self {
        let now = NanosecondTimestamp::now();
        let close = now + period;
        Self {
            id: monotonic_id(),
            symbol,
            insight_type: InsightType::Price,
            direction,
            period,
            magnitude,
            confidence,
            source_model: source_model.to_string(),
            generated_time_utc: now,
            close_time_utc: close,
            score: None,
        }
    }

    pub fn up(symbol: Symbol, period: TimeSpan) -> Self {
        Self::new(symbol, InsightDirection::Up, period, None, None, "")
    }

    pub fn down(symbol: Symbol, period: TimeSpan) -> Self {
        Self::new(symbol, InsightDirection::Down, period, None, None, "")
    }

    pub fn flat(symbol: Symbol, period: TimeSpan) -> Self {
        Self::new(symbol, InsightDirection::Flat, period, None, None, "")
    }

    pub fn is_expired(&self, utc_now: DateTime) -> bool {
        utc_now >= self.close_time_utc
    }

    pub fn is_active(&self, utc_now: DateTime) -> bool {
        !self.is_expired(utc_now)
    }
}

/// Generate a monotonically-increasing u64 id without external dependencies.
fn monotonic_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}
