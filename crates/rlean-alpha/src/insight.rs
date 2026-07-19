use rlean_core::{DateTime, NanosecondTimestamp, Symbol, TimeSpan};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

static NEXT_INSIGHT_ID: AtomicU64 = AtomicU64::new(1);

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
    /// Portfolio allocation weight used by InsightWeightingPortfolioConstructionModel.
    #[serde(default)]
    pub weight: Option<Decimal>,
    pub source_model: Arc<str>,
    pub generated_time_utc: DateTime,
    pub close_time_utc: DateTime,
    /// Filled in by a scoring layer after the fact.
    pub score: Option<Decimal>,
    /// Security price at the time the insight was generated (set by the engine).
    pub reference_value: Option<Decimal>,
    /// Most recent security price observed while scoring this insight.
    pub reference_value_final: Option<Decimal>,
    /// Direction-accuracy score in [0, 1] (mirrors C# `InsightScore.Direction`).
    pub score_direction: Option<f64>,
    /// Magnitude-accuracy score in [0, 1] (mirrors C# `InsightScore.Magnitude`).
    pub score_magnitude: Option<f64>,
    /// True once the insight has closed and its score is final.
    pub is_final_score: bool,
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
            weight: None,
            source_model: Arc::from(source_model),
            generated_time_utc: now,
            close_time_utc: close,
            score: None,
            reference_value: None,
            reference_value_final: None,
            score_direction: None,
            score_magnitude: None,
            is_final_score: false,
        }
    }

    /// Update direction/magnitude scores from the current security price.
    ///
    /// Direction score is 1.0 when the price has moved in the predicted direction
    /// (0.0 against, 0.5 for flat insights). Magnitude score compares the realised
    /// move to the predicted magnitude. Mirrors the spirit of C# insight scoring.
    pub fn update_score(&mut self, current_price: Decimal) {
        let reference = match self.reference_value {
            Some(r) if r != Decimal::ZERO => r,
            _ => return,
        };
        if self.is_final_score {
            return;
        }
        self.reference_value_final = Some(current_price);
        use rust_decimal::prelude::ToPrimitive;
        let realised = ((current_price - reference) / reference)
            .to_f64()
            .unwrap_or(0.0);
        let direction_score = match self.direction {
            InsightDirection::Up => {
                if realised > 0.0 {
                    1.0
                } else if realised < 0.0 {
                    0.0
                } else {
                    0.5
                }
            }
            InsightDirection::Down => {
                if realised < 0.0 {
                    1.0
                } else if realised > 0.0 {
                    0.0
                } else {
                    0.5
                }
            }
            InsightDirection::Flat => 0.5,
        };
        self.score_direction = Some(direction_score);
        let predicted = self
            .magnitude
            .and_then(|m| m.to_f64())
            .map(|m| m.abs())
            .filter(|m| *m > 0.0);
        if let Some(pred) = predicted {
            // 1.0 when realised magnitude matches prediction; decays with relative error.
            let err = (realised.abs() - pred).abs() / pred;
            self.score_magnitude = Some((1.0 - err).clamp(0.0, 1.0));
        } else {
            self.score_magnitude = Some(direction_score);
        }
    }

    /// Mark the score final (called when the insight closes).
    pub fn finalize_score(&mut self) {
        self.is_final_score = true;
    }

    pub fn with_generated_time_utc(mut self, generated_time_utc: DateTime) -> Self {
        self.generated_time_utc = generated_time_utc;
        self.close_time_utc = generated_time_utc + self.period;
        self
    }

    pub fn with_weight(mut self, weight: Option<Decimal>) -> Self {
        self.weight = weight;
        self
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
        // Match C# Lean Insight.IsExpired (CloseTimeUtc < utcTime): the insight is still ACTIVE
        // at exactly its close time, so it gets scored on the close bar and its realized return
        // is measured to the close price — not one bar short (which `>=` caused).
        utc_now > self.close_time_utc
    }

    pub fn is_active(&self, utc_now: DateTime) -> bool {
        !self.is_expired(utc_now)
    }
}

/// Generate a monotonically-increasing u64 id without external dependencies.
fn monotonic_id() -> u64 {
    NEXT_INSIGHT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Ensure newly-created insight IDs remain above IDs restored from durable
/// framework state. Live restart hydration preserves insight IDs, so the
/// process-local allocator must advance past the restored high-water mark.
pub(crate) fn reserve_insight_ids_through(restored_id: u64) {
    let next = restored_id.saturating_add(1);
    NEXT_INSIGHT_ID.fetch_max(next, Ordering::Relaxed);
}
