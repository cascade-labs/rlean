use crate::crypto_exchange::CryptoExchange;
use crate::crypto_margin::LeverageConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Bybit spot + futures fee model.
///
/// Spot non-VIP: maker 0.1%, taker 0.1%
/// Futures non-VIP: maker 0.02%, taker 0.055%
///
/// Source: <https://learn.bybit.com/bybit-guide/bybit-trading-fees/>
pub struct BybitExchangeModel {
    pub spot_taker_fee: Decimal,
    pub spot_maker_fee: Decimal,
    pub perp_taker_fee: Decimal,
    pub perp_maker_fee: Decimal,
}

impl Default for BybitExchangeModel {
    fn default() -> Self {
        Self {
            spot_taker_fee: dec!(0.001),
            spot_maker_fee: dec!(0.001),
            perp_taker_fee: dec!(0.00055),
            perp_maker_fee: dec!(0.0002),
        }
    }
}

impl BybitExchangeModel {
    pub fn exchange() -> CryptoExchange {
        CryptoExchange::bybit()
    }

    pub fn spot_leverage() -> LeverageConfig {
        LeverageConfig::spot()
    }

    /// Bybit brokerage model uses 10x leverage for margin accounts.
    pub fn perp_leverage() -> LeverageConfig {
        LeverageConfig::perp_10x()
    }
}
