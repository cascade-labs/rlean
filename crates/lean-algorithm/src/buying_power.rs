use lean_core::{Price, Quantity, SecurityType, Symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::portfolio::SecurityHolding;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuyingPowerModel {
    Cash,
    SecurityMargin,
    CryptoFutureMargin,
}

impl BuyingPowerModel {
    pub fn default_for(symbol: &Symbol, account_is_cash: bool) -> Self {
        if account_is_cash {
            return BuyingPowerModel::Cash;
        }
        match symbol.security_type() {
            SecurityType::CryptoFuture => BuyingPowerModel::CryptoFutureMargin,
            _ => BuyingPowerModel::SecurityMargin,
        }
    }

    pub fn validate_leverage(leverage: f64) {
        assert!(
            leverage.is_finite() && leverage >= 1.0,
            "Leverage must be greater than or equal to 1."
        );
    }

    pub fn leverage_decimal(leverage: f64) -> Decimal {
        Self::validate_leverage(leverage);
        Decimal::from_f64_retain(leverage).expect("finite leverage converts to Decimal")
    }

    pub fn notional_value(quantity: Quantity, price: Price, contract_multiplier: Decimal) -> Price {
        quantity.abs() * price.abs() * contract_multiplier
    }

    pub fn initial_margin_requirement(
        &self,
        quantity: Quantity,
        price: Price,
        contract_multiplier: Decimal,
        leverage: f64,
    ) -> Price {
        if quantity.is_zero() || price.is_zero() || contract_multiplier.is_zero() {
            return Decimal::ZERO;
        }
        let notional = Self::notional_value(quantity, price, contract_multiplier);
        match self {
            BuyingPowerModel::Cash => notional,
            BuyingPowerModel::SecurityMargin | BuyingPowerModel::CryptoFutureMargin => {
                notional / Self::leverage_decimal(leverage)
            }
        }
    }

    pub fn maintenance_margin_requirement(
        &self,
        quantity: Quantity,
        price: Price,
        contract_multiplier: Decimal,
        leverage: f64,
    ) -> Price {
        self.initial_margin_requirement(quantity, price, contract_multiplier, leverage)
    }

    pub fn unit_margin(&self, price: Price, contract_multiplier: Decimal, leverage: f64) -> Price {
        self.initial_margin_requirement(Decimal::ONE, price, contract_multiplier, leverage)
    }

    pub fn target_quantity_for_buying_power(
        &self,
        portfolio_value: Price,
        target_buying_power: Decimal,
        price: Price,
        contract_multiplier: Decimal,
        leverage: f64,
    ) -> Quantity {
        let unit_margin = self.unit_margin(price, contract_multiplier, leverage);
        if unit_margin.is_zero() {
            return Decimal::ZERO;
        }
        (portfolio_value * target_buying_power) / unit_margin
    }

    pub fn reserved_buying_power_for_holding(
        &self,
        holding: &SecurityHolding,
        leverage: f64,
    ) -> Price {
        if !holding.is_invested() || holding.last_price <= dec!(0) {
            return Decimal::ZERO;
        }
        self.maintenance_margin_requirement(
            holding.quantity,
            holding.last_price,
            holding.contract_multiplier,
            leverage,
        )
    }
}
