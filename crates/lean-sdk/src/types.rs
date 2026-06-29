use lean_sdk_annotations::{sdk_bind, sdk_static};

#[sdk_bind(py_name = "Market")]
pub struct MarketConstants;

#[sdk_bind(py_name = "HyperliquidUniverse")]
pub struct HyperliquidUniverseConstants;

impl HyperliquidUniverseConstants {
    #[sdk_static]
    pub fn hip3(dex: &str) -> String {
        format!(
            "HIP3_{}",
            dex.trim()
                .replace(['-', '.', ':', ' '], "_")
                .to_ascii_uppercase()
        )
    }
}

#[sdk_bind(py_name = "Resolution", rust_type = "lean_core::Resolution")]
pub enum Resolution {
    Tick = 0,
    Second = 1,
    Minute = 2,
    Hour = 3,
    Daily = 4,
}

#[sdk_bind(py_name = "SecurityType", rust_type = "lean_core::SecurityType")]
pub enum SecurityType {
    Base = 0,
    Equity = 1,
    Option = 2,
    Commodity = 3,
    Forex = 4,
    Future = 5,
    Cfd = 6,
    Crypto = 7,
    FutureOption = 8,
    IndexOption = 9,
    Index = 10,
    CryptoFuture = 11,
}

#[sdk_bind(
    py_name = "DataNormalizationMode",
    rust_type = "lean_core::DataNormalizationMode"
)]
pub enum DataNormalizationMode {
    Raw = 0,
    Adjusted = 1,
    SplitAdjusted = 2,
    TotalReturn = 3,
    ForwardPanamaCanal = 4,
    BackwardPanamaCanal = 5,
}

#[sdk_bind(
    py_name = "TimeInForce",
    rust_type = "lean_orders::order::TimeInForce",
    reverse = false
)]
pub enum TimeInForce {
    GoodTilCanceled = 0,
    Day = 1,
    ImmediateOrCancel = 2,
    FillOrKill = 3,
}

#[sdk_bind(py_name = "OptionRight", rust_type = "lean_core::OptionRight")]
pub enum OptionRight {
    Call = 0,
    Put = 1,
}

#[sdk_bind(py_name = "OptionStyle", rust_type = "lean_core::OptionStyle")]
pub enum OptionStyle {
    American = 0,
    European = 1,
}

#[sdk_bind(
    py_name = "AccountType",
    rust_type = "lean_algorithm::qc_algorithm::AccountType"
)]
pub enum AccountType {
    Margin = 0,
    Cash = 1,
}

#[sdk_bind(
    py_name = "BrokerageName",
    rust_type = "lean_algorithm::qc_algorithm::BrokerageName"
)]
pub enum BrokerageName {
    Default = 0,
    QuantConnectBrokerage = 1,
    InteractiveBrokersBrokerage = 2,
    TradierBrokerage = 3,
    HyperliquidBrokerage = 4,
}

#[sdk_bind(py_name = "OrderType")]
pub enum OrderType {
    Market = 0,
    Limit = 1,
    StopMarket = 2,
    StopLimit = 3,
    MarketOnOpen = 4,
    MarketOnClose = 5,
    OptionExercise = 6,
    LimitIfTouched = 7,
    ComboMarket = 8,
    ComboLimit = 9,
    ComboLegLimit = 10,
    TrailingStop = 11,
}

#[sdk_bind(py_name = "OrderStatus")]
pub enum OrderStatus {
    New = 0,
    Submitted = 1,
    PartiallyFilled = 2,
    Filled = 3,
    Canceled = 5,
    Invalid = 6,
    CancelPending = 7,
    UpdateSubmitted = 8,
}

#[sdk_bind(py_name = "OrderDirection")]
pub enum OrderDirection {
    Buy = 0,
    Sell = 1,
    Hold = 2,
}

#[sdk_bind(
    py_name = "MovingAverageType",
    rust_type = "lean_sdk::types::MovingAverageType"
)]
pub enum MovingAverageType {
    Simple = 0,
    Exponential = 1,
    Weighted = 2,
    DoubleExponential = 3,
    TripleExponential = 4,
    Triangular = 5,
    Kama = 6,
    Adaptive = 7,
    LinearWeightedMovingAverage = 8,
    Alma = 9,
    T3 = 10,
    Vwap = 11,
    Hull = 12,
    MidPoint = 13,
    MidPrice = 14,
    Dema = 15,
    Tema = 16,
    Hma = 17,
    Wilders = 18,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_discriminants_match_lean_public_constants() {
        assert_eq!(Resolution::Daily as i32, 4);
        assert_eq!(SecurityType::CryptoFuture as i32, 11);
        assert_eq!(DataNormalizationMode::Adjusted as i32, 1);
        assert_eq!(TimeInForce::Day as i32, 1);
        assert_eq!(OptionRight::Put as i32, 1);
        assert_eq!(OptionStyle::European as i32, 1);
        assert_eq!(AccountType::Cash as i32, 1);
        assert_eq!(BrokerageName::HyperliquidBrokerage as i32, 4);
        assert_eq!(OrderType::TrailingStop as i32, 11);
        assert_eq!(OrderStatus::Canceled as i32, 5);
        assert_eq!(OrderDirection::Hold as i32, 2);
        assert_eq!(MovingAverageType::Wilders as i32, 18);
    }

    #[test]
    fn hyperliquid_hip3_constant_normalizes_dex_name() {
        assert_eq!(
            HyperliquidUniverseConstants::hip3(" trading.xyz "),
            "HIP3_TRADING_XYZ"
        );
    }
}
