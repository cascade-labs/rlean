use lean_core::{OptionRight, OptionStyle};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use crate::contract::OptionContract;

pub struct OptionAssignmentResult {
    pub quantity: Decimal,
    pub tag: String,
}

impl OptionAssignmentResult {
    pub fn new(quantity: Decimal, tag: impl Into<String>) -> Self {
        OptionAssignmentResult { quantity, tag: tag.into() }
    }

    pub fn is_null(&self) -> bool { self.quantity == Decimal::ZERO }
}

pub struct OptionAssignmentParameters<'a> {
    pub contract: &'a OptionContract,
    pub holding_quantity: Decimal,  // negative = short
    pub underlying_price: Decimal,
    pub current_time: chrono::NaiveDate,
}

pub trait IOptionAssignmentModel: Send + Sync {
    fn get_assignment(&self, params: &OptionAssignmentParameters) -> OptionAssignmentResult;
}

pub struct DefaultOptionAssignmentModel {
    /// Days before expiry to check for early assignment (default: 4)
    pub prior_expiration_days: i64,
    /// Required ITM percentage (default: 5%)
    pub required_itm_percent: Decimal,
}

impl Default for DefaultOptionAssignmentModel {
    fn default() -> Self {
        DefaultOptionAssignmentModel {
            prior_expiration_days: 4,
            required_itm_percent: dec!(0.05),
        }
    }
}

impl IOptionAssignmentModel for DefaultOptionAssignmentModel {
    fn get_assignment(&self, params: &OptionAssignmentParameters) -> OptionAssignmentResult {
        let contract = params.contract;
        let days_to_expiry = (contract.expiry - params.current_time).num_days();

        // Only check short positions (holding_quantity < 0)
        if params.holding_quantity >= Decimal::ZERO {
            return OptionAssignmentResult::new(Decimal::ZERO, "");
        }

        // Check expiry proximity
        let within_window = match contract.style {
            OptionStyle::American => days_to_expiry <= self.prior_expiration_days,
            OptionStyle::European => days_to_expiry == 0,
        };
        if !within_window { return OptionAssignmentResult::new(Decimal::ZERO, ""); }

        // Check if deep ITM
        if !self.is_deep_in_the_money(contract, params.underlying_price) {
            return OptionAssignmentResult::new(Decimal::ZERO, "");
        }

        // Check arbitrage P&L is positive
        let pnl = self.estimate_arbitrage_pnl(contract, params.holding_quantity, params.underlying_price);
        if pnl <= Decimal::ZERO { return OptionAssignmentResult::new(Decimal::ZERO, ""); }

        OptionAssignmentResult::new(
            params.holding_quantity.abs(),
            "Simulated option assignment before expiration",
        )
    }
}

impl DefaultOptionAssignmentModel {
    fn is_deep_in_the_money(&self, contract: &OptionContract, underlying_price: Decimal) -> bool {
        if underlying_price.is_zero() { return false; }
        let itm_pct = match contract.right {
            OptionRight::Call => (underlying_price - contract.strike) / underlying_price,
            OptionRight::Put  => (contract.strike - underlying_price) / underlying_price,
        };
        itm_pct > self.required_itm_percent
    }

    fn estimate_arbitrage_pnl(
        &self,
        contract: &OptionContract,
        holding_quantity: Decimal,
        underlying_price: Decimal,
    ) -> Decimal {
        // Simplified: compare option bid vs intrinsic value
        // If intrinsic > bid, exercise is more profitable
        let intrinsic = crate::payoff::intrinsic_value(underlying_price, contract.strike, contract.right);
        let bid = contract.data.bid_price;
        // For short positions, assignment profit = (strike - underlying) for puts, etc.
        (intrinsic - bid) * holding_quantity.abs()
    }
}
