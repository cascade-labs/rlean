use lean_core::Price;
use rust_decimal_macros::dec;

pub trait BuyingPowerModel: Send + Sync {
    fn get_buying_power(&self, portfolio_value: Price, leverage: Price) -> Price;
    fn get_maximum_order_quantity(
        &self,
        portfolio_value: Price,
        target_weight: Price,
        security_price: Price,
        leverage: Price,
    ) -> Price;
}

pub struct CashBuyingPowerModel;

impl BuyingPowerModel for CashBuyingPowerModel {
    fn get_buying_power(&self, portfolio_value: Price, leverage: Price) -> Price {
        portfolio_value * leverage
    }

    fn get_maximum_order_quantity(
        &self,
        portfolio_value: Price,
        target_weight: Price,
        security_price: Price,
        leverage: Price,
    ) -> Price {
        if security_price.is_zero() {
            return dec!(0);
        }
        let target_value = portfolio_value * target_weight * leverage;
        (target_value / security_price).floor()
    }
}

pub struct SecurityMarginModel {
    pub initial_margin_requirement: Price,
    pub maintenance_margin_requirement: Price,
}

impl SecurityMarginModel {
    pub fn new(initial: Price, maintenance: Price) -> Self {
        SecurityMarginModel {
            initial_margin_requirement: initial,
            maintenance_margin_requirement: maintenance,
        }
    }
}

impl BuyingPowerModel for SecurityMarginModel {
    fn get_buying_power(&self, portfolio_value: Price, _leverage: Price) -> Price {
        if self.initial_margin_requirement.is_zero() {
            return dec!(0);
        }
        portfolio_value / self.initial_margin_requirement
    }

    fn get_maximum_order_quantity(
        &self,
        portfolio_value: Price,
        target_weight: Price,
        security_price: Price,
        _leverage: Price,
    ) -> Price {
        if security_price.is_zero() || self.initial_margin_requirement.is_zero() {
            return dec!(0);
        }
        let buying_power = portfolio_value / self.initial_margin_requirement;
        let target_value = buying_power * target_weight;
        (target_value / security_price).floor()
    }
}
