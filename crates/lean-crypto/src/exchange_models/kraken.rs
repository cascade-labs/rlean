use crate::crypto_exchange::CryptoExchange;
use crate::crypto_margin::LeverageConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Kraken spot + futures fee model.
///
/// Spot starter tier: maker 0.16%, taker 0.26%
/// Futures tier 1: maker 0.02%, taker 0.05%
///
/// Source: <https://www.kraken.com/features/fee-schedule>
pub struct KrakenExchangeModel {
    pub spot_taker_fee: Decimal,
    pub spot_maker_fee: Decimal,
    pub perp_taker_fee: Decimal,
    pub perp_maker_fee: Decimal,
}

impl Default for KrakenExchangeModel {
    fn default() -> Self {
        Self {
            spot_taker_fee: dec!(0.0026),
            spot_maker_fee: dec!(0.0016),
            perp_taker_fee: dec!(0.0005),
            perp_maker_fee: dec!(0.0002),
        }
    }
}

impl KrakenExchangeModel {
    pub fn exchange() -> CryptoExchange {
        CryptoExchange::kraken()
    }

    pub fn spot_leverage() -> LeverageConfig {
        LeverageConfig::spot()
    }

    /// Kraken futures leverage up to 50x for BTC.
    pub fn perp_leverage() -> LeverageConfig {
        LeverageConfig {
            max_leverage: dec!(50),
            default_leverage: dec!(5),
            maintenance_margin_rate: dec!(0.005),
            initial_margin_rate: dec!(0.02),
        }
    }
}
