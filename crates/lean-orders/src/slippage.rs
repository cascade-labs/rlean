use crate::order::Order;
use lean_core::Price;
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Models price slippage on order fills.
pub trait SlippageModel: Send + Sync {
    fn get_slippage_amount(&self, order: &Order, bar: &TradeBar) -> Price;
}

/// Zero slippage — ideal execution at exact price.
pub struct NullSlippageModel;

impl SlippageModel for NullSlippageModel {
    fn get_slippage_amount(&self, _order: &Order, _bar: &TradeBar) -> Price {
        dec!(0)
    }
}

/// Fixed absolute slippage per trade (e.g., $0.01 per share).
pub struct ConstantSlippageModel {
    pub slippage: Price,
}

impl ConstantSlippageModel {
    pub fn new(slippage: Price) -> Self { ConstantSlippageModel { slippage } }
}

impl SlippageModel for ConstantSlippageModel {
    fn get_slippage_amount(&self, _order: &Order, _bar: &TradeBar) -> Price {
        self.slippage
    }
}

/// Half-spread slippage model — assume execution at mid ± half the spread.
/// For daily bars, approximates using (high - low) / 2 as a proxy for spread.
pub struct SpreadSlippageModel {
    pub spread_fraction: Decimal,
}

impl SpreadSlippageModel {
    /// `spread_fraction` = fraction of true range to use as slippage.
    /// Default is 0.02 (2% of daily range), which is conservative.
    pub fn new(spread_fraction: Decimal) -> Self {
        SpreadSlippageModel { spread_fraction }
    }
}

impl Default for SpreadSlippageModel {
    fn default() -> Self { SpreadSlippageModel::new(dec!(0.02)) }
}

impl SlippageModel for SpreadSlippageModel {
    fn get_slippage_amount(&self, _order: &Order, bar: &TradeBar) -> Price {
        bar.true_range() * self.spread_fraction
    }
}

/// Volume-weighted slippage — larger orders get worse fills.
pub struct VolumeShareSlippageModel {
    /// Price impact = price_impact * (quantity / volume)^volume_exponent
    pub price_impact: Decimal,
    pub volume_exponent: Decimal,
}

impl VolumeShareSlippageModel {
    pub fn new(price_impact: Decimal, volume_exponent: Decimal) -> Self {
        VolumeShareSlippageModel { price_impact, volume_exponent }
    }
}

impl Default for VolumeShareSlippageModel {
    fn default() -> Self {
        VolumeShareSlippageModel {
            price_impact: dec!(0.1),
            volume_exponent: dec!(2),
        }
    }
}

impl SlippageModel for VolumeShareSlippageModel {
    fn get_slippage_amount(&self, order: &Order, bar: &TradeBar) -> Price {
        if bar.volume.is_zero() { return dec!(0); }

        use rust_decimal::prelude::ToPrimitive;
        let qty_f = order.abs_quantity().to_f64().unwrap_or(0.0);
        let vol_f = bar.volume.to_f64().unwrap_or(1.0);
        let vol_share = qty_f / vol_f;
        let exp = self.volume_exponent.to_f64().unwrap_or(2.0);
        let impact = vol_share.powf(exp);

        let impact_dec = Decimal::from_f64_retain(impact).unwrap_or(dec!(0));
        bar.close * self.price_impact * impact_dec
    }
}
