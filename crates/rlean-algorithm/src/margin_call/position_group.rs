use crate::buying_power::BuyingPowerModel;
use crate::portfolio::SecurityHolding;
use crate::securities::Security;
use rlean_core::Symbol;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;

/// Phase-1 default position group: one invested holding per group.
#[derive(Debug, Clone)]
pub struct DefaultPositionGroup {
    pub symbol: Symbol,
    pub quantity: Decimal,
    pub unit_quantity: Decimal,
    pub holding: SecurityHolding,
    pub security: Arc<Security>,
}

impl DefaultPositionGroup {
    pub fn reserved_buying_power(&self) -> Decimal {
        self.security
            .buying_power_model()
            .reserved_buying_power_for_holding(&self.holding, self.security.leverage())
    }

    pub fn unrealized_profit(&self) -> Decimal {
        self.holding.unrealized_pnl
    }
}

/// Collection of default single-security position groups.
#[derive(Debug, Clone, Default)]
pub struct PositionGroupCollection {
    pub groups: Vec<DefaultPositionGroup>,
}

impl PositionGroupCollection {
    pub fn from_holdings(
        holdings: &[SecurityHolding],
        securities: &crate::securities::SecurityManager,
    ) -> Self {
        let mut groups = Vec::new();
        for holding in holdings {
            if !holding.is_invested() {
                continue;
            }
            let security = securities.get(&holding.symbol).unwrap_or_else(|| {
                panic!(
                    "portfolio invariant violated: invested holding {} has no Security",
                    holding.symbol.value
                )
            });
            groups.push(DefaultPositionGroup {
                symbol: holding.symbol.clone(),
                quantity: holding.quantity,
                unit_quantity: dec!(1),
                holding: holding.clone(),
                security,
            });
        }
        PositionGroupCollection { groups }
    }

    pub fn total_reserved_buying_power(&self) -> Decimal {
        self.groups.iter().map(|g| g.reserved_buying_power()).sum()
    }
}

/// Compute margin-call liquidation quantity for one default position group.
pub fn generate_margin_call_quantity_for_group(
    group: &DefaultPositionGroup,
    total_portfolio_value: Decimal,
    total_used_margin: Decimal,
    margin_remaining: Decimal,
) -> Decimal {
    if group.quantity.is_zero() {
        return Decimal::ZERO;
    }

    let delta_account_currency = total_used_margin - total_portfolio_value;
    let currently_used = group.reserved_buying_power();
    let buying_power_to_keep = (currently_used - delta_account_currency).max(Decimal::ZERO);
    let sign = if group.quantity > Decimal::ZERO {
        Decimal::ONE
    } else if group.quantity < Decimal::ZERO {
        dec!(-1)
    } else {
        Decimal::ZERO
    };
    let delta_buying_power = (currently_used - buying_power_to_keep) * -sign;

    let result = BuyingPowerModel::maximum_order_quantity_for_delta_buying_power(
        group.security.buying_power_model(),
        &group.holding,
        group.security.leverage(),
        Decimal::from_f64_retain(group.security.symbol_properties.lot_size).unwrap_or(Decimal::ONE),
        total_portfolio_value,
        delta_buying_power,
        Decimal::ZERO,
        margin_remaining,
        |_| Decimal::ZERO,
    );
    result.quantity
}
