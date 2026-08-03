use crate::order::Order;
use rlean_core::Price;
use rlean_data_tables::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Models price slippage on order fills.
pub trait SlippageModel: Send + Sync + std::fmt::Debug {
    fn get_slippage_amount(&self, order: &Order, bar: &TradeBar) -> Price;
}

/// Zero slippage — ideal execution at exact price.
#[derive(Debug)]
pub struct NullSlippageModel;

impl SlippageModel for NullSlippageModel {
    fn get_slippage_amount(&self, _order: &Order, _bar: &TradeBar) -> Price {
        dec!(0)
    }
}

/// Fixed absolute slippage per trade (e.g., $0.01 per share).
#[derive(Debug)]
pub struct ConstantSlippageModel {
    pub slippage: Price,
}

impl ConstantSlippageModel {
    pub fn new(slippage: Price) -> Self {
        ConstantSlippageModel { slippage }
    }
}

impl SlippageModel for ConstantSlippageModel {
    fn get_slippage_amount(&self, _order: &Order, _bar: &TradeBar) -> Price {
        self.slippage
    }
}

/// Half-spread slippage model — assume execution at mid ± half the spread.
/// For daily bars, approximates using (high - low) / 2 as a proxy for spread.
#[derive(Debug)]
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
    fn default() -> Self {
        SpreadSlippageModel::new(dec!(0.02))
    }
}

impl SlippageModel for SpreadSlippageModel {
    fn get_slippage_amount(&self, _order: &Order, bar: &TradeBar) -> Price {
        bar.true_range() * self.spread_fraction
    }
}

/// LEAN-compatible volume-share slippage.
///
/// Price impact is `price_impact * min(quantity / volume, volume_limit)^2`.
/// Quote-driven fills promote LEAN's directional QuoteBar size into the
/// synthetic TradeBar volume before invoking this model.
#[derive(Debug)]
pub struct VolumeShareSlippageModel {
    pub volume_limit: Decimal,
    pub price_impact: Decimal,
}

impl VolumeShareSlippageModel {
    pub fn new(volume_limit: Decimal, price_impact: Decimal) -> Self {
        VolumeShareSlippageModel {
            volume_limit,
            price_impact,
        }
    }
}

impl Default for VolumeShareSlippageModel {
    fn default() -> Self {
        VolumeShareSlippageModel {
            volume_limit: dec!(0.025),
            price_impact: dec!(0.1),
        }
    }
}

impl SlippageModel for VolumeShareSlippageModel {
    fn get_slippage_amount(&self, order: &Order, bar: &TradeBar) -> Price {
        let volume_share = if bar.volume > Decimal::ZERO {
            (order.abs_quantity() / bar.volume).min(self.volume_limit)
        } else {
            self.volume_limit
        };
        bar.close * self.price_impact * volume_share * volume_share
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
    use rlean_data_tables::TradeBarData;

    fn fixture(quantity: Decimal, volume: Decimal) -> (Order, TradeBar) {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let time = NanosecondTimestamp::from_secs(1);
        (
            Order::market(1, symbol.clone(), quantity, time, ""),
            TradeBar::new(
                symbol,
                time,
                TimeSpan::ONE_MINUTE,
                TradeBarData::new(dec!(10), dec!(10), dec!(10), dec!(10), volume),
            ),
        )
    }

    #[test]
    fn volume_share_matches_lean_below_limit() {
        let (order, bar) = fixture(dec!(1), dec!(100));
        let model = VolumeShareSlippageModel::default();
        assert_eq!(model.get_slippage_amount(&order, &bar), dec!(0.0001));
    }

    #[test]
    fn volume_share_caps_participation_at_lean_default() {
        let (order, bar) = fixture(dec!(100), dec!(100));
        let model = VolumeShareSlippageModel::default();
        assert_eq!(model.get_slippage_amount(&order, &bar), dec!(0.0006250));
    }

    #[test]
    fn zero_volume_uses_maximum_slippage() {
        let (order, bar) = fixture(dec!(1), Decimal::ZERO);
        let model = VolumeShareSlippageModel::default();
        assert_eq!(model.get_slippage_amount(&order, &bar), dec!(0.0006250));
    }
}
