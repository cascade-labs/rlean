use chrono::NaiveDate;
use rust_decimal::Decimal;
use crate::contract_series::FuturesContract;

/// How to stitch contracts together
#[derive(Debug, Clone, Copy)]
pub enum ContinuousContractType {
    /// Raw price (no adjustment)
    Raw,
    /// Panama canal method: adjust all historical prices by the gap at roll
    PanamaCanal,
    /// Proportional adjustment (ratio method)
    ProportionalAdjustment,
}

/// A continuous (perpetual) futures contract that automatically rolls.
pub struct ContinuousContract {
    pub underlying: String,
    pub contract_type: ContinuousContractType,
    pub roll_days_before_expiry: i32,  // how many days before expiry to roll (default 3)
    pub current_contract: Option<FuturesContract>,
    pub next_contract: Option<FuturesContract>,
    pub cumulative_adjustment: Decimal, // for Panama Canal method
}

impl ContinuousContract {
    pub fn new(underlying: &str, contract_type: ContinuousContractType) -> Self {
        Self {
            underlying: underlying.to_string(),
            contract_type,
            roll_days_before_expiry: 3,
            current_contract: None,
            next_contract: None,
            cumulative_adjustment: Decimal::ZERO,
        }
    }

    /// Check if roll should occur on given date.
    pub fn should_roll(&self, date: NaiveDate) -> bool {
        if let Some(c) = &self.current_contract {
            let days_to_expiry = (c.expiry - date).num_days();
            days_to_expiry <= self.roll_days_before_expiry as i64
        } else {
            true // no contract yet — need to initialize
        }
    }

    /// Execute roll: current becomes expired, next becomes current.
    /// Returns the price adjustment factor/amount for the gap.
    pub fn roll(&mut self, current_price: Decimal, next_price: Decimal) -> Decimal {
        let gap = match self.contract_type {
            ContinuousContractType::Raw => Decimal::ZERO,
            ContinuousContractType::PanamaCanal => next_price - current_price,
            ContinuousContractType::ProportionalAdjustment => {
                if current_price.is_zero() {
                    Decimal::ZERO
                } else {
                    next_price / current_price - Decimal::ONE
                }
            }
        };
        self.cumulative_adjustment += gap;
        if let Some(next) = self.next_contract.take() {
            self.current_contract = Some(next);
        }
        gap
    }

    /// Adjust a historical price for the continuous contract
    pub fn adjust_price(&self, raw_price: Decimal) -> Decimal {
        match self.contract_type {
            ContinuousContractType::Raw => raw_price,
            ContinuousContractType::PanamaCanal => raw_price + self.cumulative_adjustment,
            ContinuousContractType::ProportionalAdjustment => {
                raw_price * (Decimal::ONE + self.cumulative_adjustment)
            }
        }
    }
}
