use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Margin mode for crypto futures/perps
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarginMode {
    Isolated, // each position has separate margin
    Cross,    // all positions share account margin
}

/// Leverage configuration
#[derive(Debug, Clone)]
pub struct LeverageConfig {
    pub max_leverage: Decimal,
    pub default_leverage: Decimal,
    pub maintenance_margin_rate: Decimal, // 0.005 = 0.5%
    pub initial_margin_rate: Decimal,     // 1/max_leverage
}

impl LeverageConfig {
    pub fn spot() -> Self {
        Self {
            max_leverage: dec!(1),
            default_leverage: dec!(1),
            maintenance_margin_rate: dec!(0),
            initial_margin_rate: dec!(1),
        }
    }

    /// 10x leverage — Bybit default
    pub fn perp_10x() -> Self {
        Self {
            max_leverage: dec!(10),
            default_leverage: dec!(3),
            maintenance_margin_rate: dec!(0.005),
            initial_margin_rate: dec!(0.1),
        }
    }

    /// 125x leverage — Binance max
    pub fn perp_125x() -> Self {
        Self {
            max_leverage: dec!(125),
            default_leverage: dec!(20),
            maintenance_margin_rate: dec!(0.004),
            initial_margin_rate: dec!(0.008),
        }
    }

    /// 25x leverage — Binance/CryptoFutureMarginModel default
    pub fn perp_25x() -> Self {
        Self {
            max_leverage: dec!(25),
            default_leverage: dec!(10),
            maintenance_margin_rate: dec!(0.005),
            initial_margin_rate: dec!(0.04),
        }
    }

    pub fn initial_margin(&self, notional: Decimal) -> Decimal {
        notional * self.initial_margin_rate
    }

    pub fn maintenance_margin(&self, notional: Decimal) -> Decimal {
        notional * self.maintenance_margin_rate
    }
}

/// Liquidation price calculation for a leveraged position.
/// For long:  liquidation_price = entry * (1 - 1/leverage + maintenance_margin_rate)
/// For short: liquidation_price = entry * (1 + 1/leverage - maintenance_margin_rate)
pub fn liquidation_price(
    entry: Decimal,
    leverage: Decimal,
    is_long: bool,
    mmr: Decimal,
) -> Decimal {
    if leverage.is_zero() {
        return dec!(0);
    }
    if is_long {
        entry * (dec!(1) - dec!(1) / leverage + mmr)
    } else {
        entry * (dec!(1) + dec!(1) / leverage - mmr)
    }
}

/// Returns true if a position should be liquidated at current mark price.
pub fn should_liquidate(mark_price: Decimal, liq_price: Decimal, is_long: bool) -> bool {
    if is_long {
        mark_price <= liq_price
    } else {
        mark_price >= liq_price
    }
}
