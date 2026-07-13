use rlean_core::{Price, Quantity, SecurityType, Symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::portfolio::SecurityHolding;

/// Result of a maximum-order-quantity calculation — mirrors C# `GetMaximumOrderQuantityResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaximumOrderQuantityResult {
    pub quantity: Quantity,
    pub reason: Option<String>,
    pub is_error: bool,
}

impl MaximumOrderQuantityResult {
    pub fn success(quantity: Quantity) -> Self {
        Self {
            quantity,
            reason: None,
            is_error: false,
        }
    }

    pub fn zero(reason: Option<String>, is_error: bool) -> Self {
        Self {
            quantity: Decimal::ZERO,
            reason,
            is_error,
        }
    }
}

fn signum_decimal(d: Decimal) -> Decimal {
    if d > Decimal::ZERO {
        Decimal::ONE
    } else if d < Decimal::ZERO {
        dec!(-1)
    } else {
        Decimal::ZERO
    }
}

fn discretely_round_by(quantity: Decimal, lot_size: Decimal, toward_positive: bool) -> Decimal {
    if lot_size <= Decimal::ZERO {
        return quantity;
    }
    let units = quantity / lot_size;
    let rounded_units = if toward_positive {
        units.ceil()
    } else {
        units.floor()
    };
    rounded_units * lot_size
}

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

    /// Mirrors C# `BuyingPowerModelExtensions.AboveMinimumOrderMarginPortfolioPercentage`.
    pub fn above_minimum_order_margin_portfolio_percentage(
        total_portfolio_value: Decimal,
        minimum_order_margin_portfolio_percentage: Decimal,
        abs_final_order_margin: Decimal,
        margin_remaining: Decimal,
    ) -> bool {
        if minimum_order_margin_portfolio_percentage.is_zero() {
            return true;
        }
        let minimum_value = total_portfolio_value * minimum_order_margin_portfolio_percentage;
        if minimum_value > abs_final_order_margin && margin_remaining > Decimal::ZERO {
            return false;
        }
        true
    }

    fn amount_to_order(
        model: Self,
        holding: &SecurityHolding,
        leverage: f64,
        lot_size: Decimal,
        target_margin: Decimal,
        margin_for_one_unit: Decimal,
    ) -> (Decimal, Decimal) {
        let mut order_size = -holding.quantity;
        if !margin_for_one_unit.is_zero() {
            order_size += target_margin / margin_for_one_unit;
        }

        let toward_positive = target_margin < Decimal::ZERO;
        order_size = discretely_round_by(order_size, lot_size, toward_positive);

        let mut final_margin = model.initial_margin_requirement(
            order_size + holding.quantity,
            holding.last_price,
            holding.contract_multiplier,
            leverage,
        );

        let mut margin_difference = final_margin - target_margin;
        while (target_margin < Decimal::ZERO && margin_difference < Decimal::ZERO)
            || (target_margin > Decimal::ZERO && margin_difference > Decimal::ZERO)
        {
            order_size += if target_margin < Decimal::ZERO {
                lot_size
            } else {
                -lot_size
            };
            let new_final_margin = model.initial_margin_requirement(
                order_size + holding.quantity,
                holding.last_price,
                holding.contract_multiplier,
                leverage,
            );
            let new_difference = new_final_margin - target_margin;
            if new_difference.abs() > margin_difference.abs()
                && signum_decimal(new_difference) == signum_decimal(margin_difference)
            {
                break;
            }
            final_margin = new_final_margin;
            margin_difference = new_difference;
        }

        (order_size, final_margin)
    }

    /// Mirrors C# `GetMaximumOrderQuantityForTargetBuyingPower`.
    #[allow(clippy::too_many_arguments)]
    pub fn maximum_order_quantity_for_target_buying_power(
        model: Self,
        holding: &SecurityHolding,
        leverage: f64,
        lot_size: Decimal,
        total_portfolio_value: Decimal,
        target_buying_power: Decimal,
        minimum_order_margin_portfolio_percentage: Decimal,
        margin_remaining: Decimal,
        order_fee: impl Fn(Decimal) -> Decimal,
    ) -> MaximumOrderQuantityResult {
        let required_free_buying_power_percent = Decimal::ZERO;
        let mut signed_target_final_margin_value = target_buying_power
            * (total_portfolio_value - total_portfolio_value * required_free_buying_power_percent);

        if signed_target_final_margin_value.is_zero() {
            return MaximumOrderQuantityResult::success(-holding.quantity);
        }

        let signed_current_used_margin = model.initial_margin_requirement(
            holding.quantity,
            holding.last_price,
            holding.contract_multiplier,
            leverage,
        );

        let abs_unit_margin = model.initial_margin_requirement(
            Decimal::ONE,
            holding.last_price,
            holding.contract_multiplier,
            leverage,
        );
        if abs_unit_margin.is_zero() {
            return MaximumOrderQuantityResult::zero(
                Some(format!("{} has no price data yet", holding.symbol.value)),
                true,
            );
        }

        let abs_difference_of_margin =
            (signed_target_final_margin_value - signed_current_used_margin).abs();
        if !Self::above_minimum_order_margin_portfolio_percentage(
            total_portfolio_value,
            minimum_order_margin_portfolio_percentage,
            abs_difference_of_margin,
            margin_remaining,
        ) {
            return MaximumOrderQuantityResult::zero(None, false);
        }

        let mut last_order_quantity = Decimal::ZERO;
        let mut order_quantity;
        let mut signed_target_holdings_margin;

        loop {
            let (qty, holdings_margin) = Self::amount_to_order(
                model,
                holding,
                leverage,
                lot_size,
                signed_target_final_margin_value,
                abs_unit_margin,
            );
            order_quantity = qty;
            signed_target_holdings_margin = holdings_margin;

            if order_quantity.is_zero() {
                return MaximumOrderQuantityResult::zero(None, false);
            }

            let fees = order_fee(order_quantity);
            signed_target_final_margin_value = (total_portfolio_value
                - fees
                - total_portfolio_value * required_free_buying_power_percent)
                * target_buying_power;

            if last_order_quantity == order_quantity {
                break;
            }
            last_order_quantity = order_quantity;

            if signed_target_holdings_margin.abs() <= signed_target_final_margin_value.abs() {
                break;
            }
        }

        MaximumOrderQuantityResult::success(order_quantity)
    }

    /// Mirrors C# `GetMaximumOrderQuantityForDeltaBuyingPower`.
    #[allow(clippy::too_many_arguments)]
    pub fn maximum_order_quantity_for_delta_buying_power(
        model: Self,
        holding: &SecurityHolding,
        leverage: f64,
        lot_size: Decimal,
        total_portfolio_value: Decimal,
        delta_buying_power: Decimal,
        minimum_order_margin_portfolio_percentage: Decimal,
        margin_remaining: Decimal,
        order_fee: impl Fn(Decimal) -> Decimal,
    ) -> MaximumOrderQuantityResult {
        let used_buying_power = model.reserved_buying_power_for_holding(holding, leverage);
        let signed_used = used_buying_power
            * if holding.is_long() {
                Decimal::ONE
            } else {
                dec!(-1)
            };
        let target_buying_power = signed_used + delta_buying_power;
        let target = if total_portfolio_value.is_zero() {
            Decimal::ZERO
        } else {
            target_buying_power / total_portfolio_value
        };

        Self::maximum_order_quantity_for_target_buying_power(
            model,
            holding,
            leverage,
            lot_size,
            total_portfolio_value,
            target,
            minimum_order_margin_portfolio_percentage,
            margin_remaining,
            order_fee,
        )
    }
}
