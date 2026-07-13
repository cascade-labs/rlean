use crate::crypto_exchange::CryptoExchange;
use crate::crypto_margin::LeverageConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Binance spot + futures fee model.
///
/// Spot tier-1: maker 0.1%, taker 0.1%
/// USDT futures tier-1: maker 0.02%, taker 0.04%
/// BUSD futures tier-1: maker 0.012%, taker 0.036%
///
/// Source: <https://www.binance.com/en/fee/schedule>
pub struct BinanceExchangeModel {
    pub spot_taker_fee: Decimal,
    pub spot_maker_fee: Decimal,
    /// USDT-margined futures taker fee
    pub perp_usdt_taker_fee: Decimal,
    /// USDT-margined futures maker fee
    pub perp_usdt_maker_fee: Decimal,
    /// BUSD-margined futures taker fee
    pub perp_busd_taker_fee: Decimal,
    /// BUSD-margined futures maker fee
    pub perp_busd_maker_fee: Decimal,
}

impl Default for BinanceExchangeModel {
    fn default() -> Self {
        Self {
            spot_taker_fee: dec!(0.001),
            spot_maker_fee: dec!(0.001),
            perp_usdt_taker_fee: dec!(0.0004),
            perp_usdt_maker_fee: dec!(0.0002),
            perp_busd_taker_fee: dec!(0.00036),
            perp_busd_maker_fee: dec!(0.00012),
        }
    }
}

impl BinanceExchangeModel {
    pub fn exchange() -> CryptoExchange {
        CryptoExchange::binance()
    }

    pub fn spot_leverage() -> LeverageConfig {
        LeverageConfig::spot()
    }

    /// Binance perpetual futures support up to 125x leverage.
    /// `CryptoFutureMarginModel` defaults to 25x.
    pub fn perp_leverage() -> LeverageConfig {
        LeverageConfig::perp_125x()
    }

    /// Default leverage used by `CryptoFutureMarginModel` (25x).
    pub fn default_perp_leverage() -> LeverageConfig {
        LeverageConfig::perp_25x()
    }
}
