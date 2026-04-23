use chrono::NaiveDate;
use rust_decimal::Decimal;

/// A lightweight view of an option contract sufficient for universe filtering.
#[derive(Debug, Clone)]
pub struct OptionContractView {
    /// Underlying ticker (e.g. "SPY").
    pub underlying: String,
    /// Option ticker / symbol value.
    pub symbol: String,
    /// Expiration date.
    pub expiry: NaiveDate,
    /// Strike price (dollars).
    pub strike: Decimal,
    /// "call" or "put".
    pub right: OptionRight,
    /// Option delta (positive for calls, negative for puts).
    /// None if greeks have not been computed.
    pub delta: Option<Decimal>,
    /// Days to expiration relative to evaluation date.
    pub dte: i64,
}

/// Option right (call or put).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionRight {
    Call,
    Put,
}

/// Filters option contracts based on expiry and greek criteria.
///
/// Mirrors the intent of C# `OptionUniverseSelectionModel.Filter(OptionFilterUniverse)`.
///
/// In rlean the full option chain is loaded from Parquet each day, so this
/// model acts as a pure filter rather than a universe subscription creator.
#[derive(Debug, Clone)]
pub struct OptionUniverseSelectionModel {
    /// Minimum days-to-expiration (inclusive).
    pub min_expiry_days: i64,
    /// Maximum days-to-expiration (inclusive).
    pub max_expiry_days: i64,
    /// Minimum delta (inclusive). For puts this is typically negative.
    pub min_delta: Option<Decimal>,
    /// Maximum delta (inclusive).
    pub max_delta: Option<Decimal>,
    /// Minimum strike (inclusive), in dollars.
    pub min_strike: Option<Decimal>,
    /// Maximum strike (inclusive), in dollars.
    pub max_strike: Option<Decimal>,
    /// If set, only include the specified right.
    pub right_filter: Option<OptionRight>,
}

impl OptionUniverseSelectionModel {
    /// Create a model with only expiry filtering. Delta and strike are unconstrained.
    pub fn new(min_expiry_days: i64, max_expiry_days: i64) -> Self {
        Self {
            min_expiry_days,
            max_expiry_days,
            min_delta: None,
            max_delta: None,
            min_strike: None,
            max_strike: None,
            right_filter: None,
        }
    }

    /// Restrict delta range (e.g. 0.20 to 0.50 for OTM calls).
    pub fn with_delta(mut self, min: Decimal, max: Decimal) -> Self {
        self.min_delta = Some(min);
        self.max_delta = Some(max);
        self
    }

    /// Restrict strike range (in dollars).
    pub fn with_strike(mut self, min: Decimal, max: Decimal) -> Self {
        self.min_strike = Some(min);
        self.max_strike = Some(max);
        self
    }

    /// Only include calls or puts.
    pub fn with_right(mut self, right: OptionRight) -> Self {
        self.right_filter = Some(right);
        self
    }

    /// Filter a slice of option contracts and return those that pass all criteria.
    ///
    /// * `contracts` – full option chain for the current evaluation date.
    pub fn filter<'a>(&self, contracts: &'a [OptionContractView]) -> Vec<&'a OptionContractView> {
        contracts
            .iter()
            .filter(|c| {
                // DTE range
                if c.dte < self.min_expiry_days || c.dte > self.max_expiry_days {
                    return false;
                }

                // Right filter
                if let Some(right) = self.right_filter {
                    if c.right != right {
                        return false;
                    }
                }

                // Delta filter
                if let Some(delta) = c.delta {
                    if let Some(min_d) = self.min_delta {
                        if delta < min_d {
                            return false;
                        }
                    }
                    if let Some(max_d) = self.max_delta {
                        if delta > max_d {
                            return false;
                        }
                    }
                }

                // Strike filter
                if let Some(min_k) = self.min_strike {
                    if c.strike < min_k {
                        return false;
                    }
                }
                if let Some(max_k) = self.max_strike {
                    if c.strike > max_k {
                        return false;
                    }
                }

                true
            })
            .collect()
    }

    /// Convenience: same as `filter` but returns owned copies.
    pub fn filter_owned(&self, contracts: &[OptionContractView]) -> Vec<OptionContractView> {
        self.filter(contracts).into_iter().cloned().collect()
    }
}

impl Default for OptionUniverseSelectionModel {
    /// Default: 0–45 DTE, no delta/strike constraint, both rights.
    fn default() -> Self {
        Self::new(0, 45)
    }
}
