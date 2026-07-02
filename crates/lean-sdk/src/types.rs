use lean_algorithm::qc_algorithm::{
    AccountType as CoreAccountType, BrokerageName as CoreBrokerageName,
};
use lean_core::{
    DataNormalizationMode as CoreDataNormalizationMode, OptionRight as CoreOptionRight,
    OptionStyle as CoreOptionStyle, Resolution as CoreResolution, SecurityType as CoreSecurityType,
};
use lean_orders::order::TimeInForce as CoreTimeInForce;

#[cfg_attr(feature = "python", pyo3::pyclass(name = "Market"))]
pub struct MarketConstants;

#[cfg_attr(feature = "python", pyo3::pyclass(name = "HyperliquidUniverse"))]
pub struct HyperliquidUniverseConstants;

impl HyperliquidUniverseConstants {
    pub fn hip3(dex: &str) -> String {
        format!(
            "HIP3_{}",
            dex.trim()
                .replace(['-', '.', ':', ' '], "_")
                .to_ascii_uppercase()
        )
    }
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "Resolution", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Tick = 0,
    Second = 1,
    Minute = 2,
    Hour = 3,
    Daily = 4,
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "SecurityType", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "DataNormalizationMode", eq, eq_int)
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataNormalizationMode {
    Raw = 0,
    Adjusted = 1,
    SplitAdjusted = 2,
    TotalReturn = 3,
    ForwardPanamaCanal = 4,
    BackwardPanamaCanal = 5,
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "TimeInForce", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeInForce {
    GoodTilCanceled = 0,
    Day = 1,
    ImmediateOrCancel = 2,
    FillOrKill = 3,
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "OptionRight", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionRight {
    Call = 0,
    Put = 1,
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "OptionStyle", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionStyle {
    American = 0,
    European = 1,
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "AccountType", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountType {
    Margin = 0,
    Cash = 1,
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "BrokerageName", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerageName {
    Default = 0,
    QuantConnectBrokerage = 1,
    InteractiveBrokersBrokerage = 2,
    TradierBrokerage = 3,
    HyperliquidBrokerage = 4,
}

#[cfg_attr(feature = "python", pyo3::pyclass(name = "OrderType", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[cfg_attr(feature = "python", pyo3::pyclass(name = "OrderStatus", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[cfg_attr(feature = "python", pyo3::pyclass(name = "OrderDirection", eq, eq_int))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    Buy = 0,
    Sell = 1,
    Hold = 2,
}

#[cfg_attr(
    feature = "python",
    pyo3::pyclass(name = "MovingAverageType", eq, eq_int)
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[cfg(feature = "python")]
macro_rules! enum_aliases {
    ($ty:ty, $($variant:ident => $alias:ident),+ $(,)?) => {
        #[pyo3::pymethods]
        impl $ty {
            $(
                #[classattr]
                const $alias: Self = Self::$variant;
            )+
        }
    };
}

#[cfg(feature = "python")]
enum_aliases!(Resolution, Tick => TICK, Second => SECOND, Minute => MINUTE, Hour => HOUR, Daily => DAILY);
#[cfg(feature = "python")]
enum_aliases!(
    SecurityType,
    Base => BASE,
    Equity => EQUITY,
    Option => OPTION,
    Commodity => COMMODITY,
    Forex => FOREX,
    Future => FUTURE,
    Cfd => CFD,
    Crypto => CRYPTO,
    FutureOption => FUTURE_OPTION,
    IndexOption => INDEX_OPTION,
    Index => INDEX,
    CryptoFuture => CRYPTO_FUTURE,
);
#[cfg(feature = "python")]
enum_aliases!(
    DataNormalizationMode,
    Raw => RAW,
    Adjusted => ADJUSTED,
    SplitAdjusted => SPLIT_ADJUSTED,
    TotalReturn => TOTAL_RETURN,
    ForwardPanamaCanal => FORWARD_PANAMA_CANAL,
    BackwardPanamaCanal => BACKWARD_PANAMA_CANAL,
);
#[cfg(feature = "python")]
enum_aliases!(
    TimeInForce,
    GoodTilCanceled => GOOD_TIL_CANCELED,
    Day => DAY,
    ImmediateOrCancel => IMMEDIATE_OR_CANCEL,
    FillOrKill => FILL_OR_KILL,
);
#[cfg(feature = "python")]
enum_aliases!(OptionRight, Call => CALL, Put => PUT);
#[cfg(feature = "python")]
enum_aliases!(OptionStyle, American => AMERICAN, European => EUROPEAN);
#[cfg(feature = "python")]
enum_aliases!(AccountType, Margin => MARGIN, Cash => CASH);
#[cfg(feature = "python")]
enum_aliases!(
    BrokerageName,
    Default => DEFAULT,
    QuantConnectBrokerage => QUANT_CONNECT_BROKERAGE,
    InteractiveBrokersBrokerage => INTERACTIVE_BROKERS_BROKERAGE,
    TradierBrokerage => TRADIER_BROKERAGE,
    HyperliquidBrokerage => HYPERLIQUID_BROKERAGE,
);
#[cfg(feature = "python")]
enum_aliases!(
    OrderType,
    Market => MARKET,
    Limit => LIMIT,
    StopMarket => STOP_MARKET,
    StopLimit => STOP_LIMIT,
    MarketOnOpen => MARKET_ON_OPEN,
    MarketOnClose => MARKET_ON_CLOSE,
    OptionExercise => OPTION_EXERCISE,
    LimitIfTouched => LIMIT_IF_TOUCHED,
    ComboMarket => COMBO_MARKET,
    ComboLimit => COMBO_LIMIT,
    ComboLegLimit => COMBO_LEG_LIMIT,
    TrailingStop => TRAILING_STOP,
);
#[cfg(feature = "python")]
enum_aliases!(
    OrderStatus,
    New => NEW,
    Submitted => SUBMITTED,
    PartiallyFilled => PARTIALLY_FILLED,
    Filled => FILLED,
    Canceled => CANCELED,
    Invalid => INVALID,
    CancelPending => CANCEL_PENDING,
    UpdateSubmitted => UPDATE_SUBMITTED,
);
#[cfg(feature = "python")]
enum_aliases!(OrderDirection, Buy => BUY, Sell => SELL, Hold => HOLD);
#[cfg(feature = "python")]
enum_aliases!(
    MovingAverageType,
    Simple => SIMPLE,
    Exponential => EXPONENTIAL,
    Weighted => WEIGHTED,
    DoubleExponential => DOUBLE_EXPONENTIAL,
    TripleExponential => TRIPLE_EXPONENTIAL,
    Triangular => TRIANGULAR,
    Kama => KAMA,
    Adaptive => ADAPTIVE,
    LinearWeightedMovingAverage => LINEAR_WEIGHTED_MOVING_AVERAGE,
    Alma => ALMA,
    Vwap => VWAP,
    Hull => HULL,
    MidPoint => MID_POINT,
    MidPrice => MID_PRICE,
    Dema => DEMA,
    Tema => TEMA,
    Hma => HMA,
    Wilders => WILDERS,
);

impl From<Resolution> for CoreResolution {
    fn from(value: Resolution) -> Self {
        match value {
            Resolution::Tick => CoreResolution::Tick,
            Resolution::Second => CoreResolution::Second,
            Resolution::Minute => CoreResolution::Minute,
            Resolution::Hour => CoreResolution::Hour,
            Resolution::Daily => CoreResolution::Daily,
        }
    }
}

impl From<CoreResolution> for Resolution {
    fn from(value: CoreResolution) -> Self {
        match value {
            CoreResolution::Tick => Resolution::Tick,
            CoreResolution::Second => Resolution::Second,
            CoreResolution::Minute => Resolution::Minute,
            CoreResolution::Hour => Resolution::Hour,
            CoreResolution::Daily => Resolution::Daily,
        }
    }
}

impl From<SecurityType> for CoreSecurityType {
    fn from(value: SecurityType) -> Self {
        match value {
            SecurityType::Base => CoreSecurityType::Base,
            SecurityType::Equity => CoreSecurityType::Equity,
            SecurityType::Option => CoreSecurityType::Option,
            SecurityType::Commodity => CoreSecurityType::Commodity,
            SecurityType::Forex => CoreSecurityType::Forex,
            SecurityType::Future => CoreSecurityType::Future,
            SecurityType::Cfd => CoreSecurityType::Cfd,
            SecurityType::Crypto => CoreSecurityType::Crypto,
            SecurityType::FutureOption => CoreSecurityType::FutureOption,
            SecurityType::IndexOption => CoreSecurityType::IndexOption,
            SecurityType::Index => CoreSecurityType::Index,
            SecurityType::CryptoFuture => CoreSecurityType::CryptoFuture,
        }
    }
}

impl From<DataNormalizationMode> for CoreDataNormalizationMode {
    fn from(value: DataNormalizationMode) -> Self {
        match value {
            DataNormalizationMode::Raw => CoreDataNormalizationMode::Raw,
            DataNormalizationMode::Adjusted => CoreDataNormalizationMode::Adjusted,
            DataNormalizationMode::SplitAdjusted => CoreDataNormalizationMode::SplitAdjusted,
            DataNormalizationMode::TotalReturn => CoreDataNormalizationMode::TotalReturn,
            DataNormalizationMode::ForwardPanamaCanal => {
                CoreDataNormalizationMode::ForwardPanamaCanal
            }
            DataNormalizationMode::BackwardPanamaCanal => {
                CoreDataNormalizationMode::BackwardPanamaCanal
            }
        }
    }
}

impl From<TimeInForce> for CoreTimeInForce {
    fn from(value: TimeInForce) -> Self {
        match value {
            TimeInForce::GoodTilCanceled => CoreTimeInForce::GoodTilCanceled,
            TimeInForce::Day => CoreTimeInForce::Day,
            TimeInForce::ImmediateOrCancel => CoreTimeInForce::ImmediateOrCancel,
            TimeInForce::FillOrKill => CoreTimeInForce::FillOrKill,
        }
    }
}

impl From<OptionRight> for CoreOptionRight {
    fn from(value: OptionRight) -> Self {
        match value {
            OptionRight::Call => CoreOptionRight::Call,
            OptionRight::Put => CoreOptionRight::Put,
        }
    }
}

impl From<CoreOptionRight> for OptionRight {
    fn from(value: CoreOptionRight) -> Self {
        match value {
            CoreOptionRight::Call => OptionRight::Call,
            CoreOptionRight::Put => OptionRight::Put,
        }
    }
}

impl From<OptionStyle> for CoreOptionStyle {
    fn from(value: OptionStyle) -> Self {
        match value {
            OptionStyle::American => CoreOptionStyle::American,
            OptionStyle::European => CoreOptionStyle::European,
        }
    }
}

impl From<AccountType> for CoreAccountType {
    fn from(value: AccountType) -> Self {
        match value {
            AccountType::Margin => CoreAccountType::Margin,
            AccountType::Cash => CoreAccountType::Cash,
        }
    }
}

impl From<BrokerageName> for CoreBrokerageName {
    fn from(value: BrokerageName) -> Self {
        match value {
            BrokerageName::Default => CoreBrokerageName::Default,
            BrokerageName::QuantConnectBrokerage => CoreBrokerageName::QuantConnectBrokerage,
            BrokerageName::InteractiveBrokersBrokerage => {
                CoreBrokerageName::InteractiveBrokersBrokerage
            }
            BrokerageName::TradierBrokerage => CoreBrokerageName::TradierBrokerage,
            BrokerageName::HyperliquidBrokerage => CoreBrokerageName::HyperliquidBrokerage,
        }
    }
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
