use crate::crypto_exchange::CryptoExchange;
use crate::crypto_margin::LeverageConfig;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Coinbase Advanced Trade fee model.
///
/// Tier 1 (< $10k 30-day volume): maker 0.4%, taker 0.6%
///
/// Source: <https://www.coinbase.com/advanced-trade/fees>
pub struct CoinbaseExchangeModel {
    pub spot_taker_fee: Decimal,
    pub spot_maker_fee: Decimal,
}

impl Default for CoinbaseExchangeModel {
    fn default() -> Self {
        Self {
            spot_taker_fee: dec!(0.006),
            spot_maker_fee: dec!(0.004),
        }
    }
}

impl CoinbaseExchangeModel {
    pub fn exchange() -> CryptoExchange {
        CryptoExchange::coinbase()
    }

    /// Coinbase spot only — no leveraged perpetuals on the main exchange.
    pub fn spot_leverage() -> LeverageConfig {
        LeverageConfig::spot()
    }
}
