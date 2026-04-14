use crate::order::{Order, OrderDirection, OrderType};
use lean_core::{Price, SecurityType};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// ─── OrderFee ────────────────────────────────────────────────────────────────

/// Represents the monetary cost of an order.
#[derive(Debug, Clone)]
pub struct OrderFee {
    /// The fee amount.
    pub amount: Price,
    /// Three-letter ISO currency code (e.g. "USD", "BTC").
    pub currency: String,
}

impl OrderFee {
    pub fn new(amount: Price, currency: impl Into<String>) -> Self {
        OrderFee { amount, currency: currency.into() }
    }

    /// Zero fee in USD.
    pub fn zero() -> Self {
        OrderFee { amount: dec!(0), currency: "USD".into() }
    }
}

// ─── FeeModel trait ──────────────────────────────────────────────────────────

/// Parameters passed to every fee model.
pub struct OrderFeeParameters<'a> {
    pub order: &'a Order,
    /// Current mid-price of the security (used to compute notional value).
    pub security_price: Price,
    /// SecurityType of the instrument being traded.
    pub security_type: SecurityType,
    /// Quote currency of the instrument (e.g. "USD", "USDT").
    pub quote_currency: String,
    /// Base currency for crypto pairs (e.g. "BTC" for BTCUSD).
    pub base_currency: Option<String>,
    /// Contract multiplier (1.0 for equities, 100 for standard options, etc.)
    pub contract_multiplier: Price,
}

impl<'a> OrderFeeParameters<'a> {
    /// Convenience constructor for plain equity / simple security.
    pub fn equity(order: &'a Order, price: Price) -> Self {
        OrderFeeParameters {
            order,
            security_price: price,
            security_type: SecurityType::Equity,
            quote_currency: "USD".into(),
            base_currency: None,
            contract_multiplier: dec!(1),
        }
    }
}

/// Calculates the commission / fee for a given order.
pub trait FeeModel: Send + Sync {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee;
}

// ─── Helper ──────────────────────────────────────────────────────────────────

/// Returns `true` when a limit order acts as a *maker* (posted to book, not
/// immediately marketable).  We conservatively treat all non-limit orders and
/// marketable limits as *taker*.
#[inline]
fn is_maker(order: &Order) -> bool {
    order.order_type == OrderType::Limit
}

// ─── NullFeeModel ─────────────────────────────────────────────────────────────

/// Zero commission for every order.
pub struct NullFeeModel;

impl FeeModel for NullFeeModel {
    fn get_order_fee(&self, _params: &OrderFeeParameters<'_>) -> OrderFee {
        OrderFee::zero()
    }
}

// ─── ZeroFeeModel (alias for NullFeeModel) ───────────────────────────────────

/// Alias for `NullFeeModel` — always returns zero.
pub struct ZeroFeeModel;

impl FeeModel for ZeroFeeModel {
    fn get_order_fee(&self, _params: &OrderFeeParameters<'_>) -> OrderFee {
        OrderFee::zero()
    }
}

// ─── FlatFeeModel ─────────────────────────────────────────────────────────────

/// Fixed dollar amount per trade regardless of size.
pub struct FlatFeeModel {
    pub fee: Price,
    pub currency: String,
}

impl FlatFeeModel {
    pub fn new(fee: Price) -> Self {
        FlatFeeModel { fee, currency: "USD".into() }
    }

    pub fn with_currency(fee: Price, currency: impl Into<String>) -> Self {
        FlatFeeModel { fee, currency: currency.into() }
    }
}

impl FeeModel for FlatFeeModel {
    fn get_order_fee(&self, _params: &OrderFeeParameters<'_>) -> OrderFee {
        OrderFee::new(self.fee, &self.currency)
    }
}

// ─── ConstantFeeModel ─────────────────────────────────────────────────────────

/// Fixed dollar amount per trade (configurable; default $1.00).
/// Mirrors `QuantConnect.Orders.Fees.ConstantFeeModel`.
pub struct ConstantFeeModel {
    pub fee: Price,
    pub currency: String,
}

impl Default for ConstantFeeModel {
    fn default() -> Self {
        ConstantFeeModel { fee: dec!(1.00), currency: "USD".into() }
    }
}

impl ConstantFeeModel {
    pub fn new(fee: Price) -> Self {
        ConstantFeeModel { fee: fee.abs(), currency: "USD".into() }
    }

    pub fn with_currency(fee: Price, currency: impl Into<String>) -> Self {
        ConstantFeeModel { fee: fee.abs(), currency: currency.into() }
    }
}

impl FeeModel for ConstantFeeModel {
    fn get_order_fee(&self, _params: &OrderFeeParameters<'_>) -> OrderFee {
        OrderFee::new(self.fee, &self.currency)
    }
}

// ─── PercentFeeModel ──────────────────────────────────────────────────────────

/// Fixed percentage of notional trade value (configurable; default 0.1%).
pub struct PercentFeeModel {
    pub rate: Decimal,
    pub currency: String,
}

impl Default for PercentFeeModel {
    fn default() -> Self {
        PercentFeeModel { rate: dec!(0.001), currency: "USD".into() }
    }
}

impl PercentFeeModel {
    pub fn new(rate: Decimal) -> Self {
        PercentFeeModel { rate, currency: "USD".into() }
    }
}

impl FeeModel for PercentFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        let notional = params.order.abs_quantity()
            * params.security_price
            * params.contract_multiplier;
        OrderFee::new(notional * self.rate, &self.currency)
    }
}

// ─── InteractiveBrokersFeeModel ───────────────────────────────────────────────

/// Models Interactive Brokers commissions.
///
/// Equities:  $0.005/share, min $1.00, max 1% of trade value.
/// Options:   $0.70/contract (tier-1).
/// Forex:     0.20 bps of notional, min $2.00.
/// Futures:   $0.85/contract + exchange fees (approximated).
/// Crypto:    0 (IB routes to exchange; we model zero here).
pub struct InteractiveBrokersFeeModel {
    pub forex_commission_rate: Decimal,
    pub forex_minimum_fee: Price,
    pub options_fee_per_contract: Price,
    pub futures_fee_per_contract: Price,
}

impl Default for InteractiveBrokersFeeModel {
    fn default() -> Self {
        InteractiveBrokersFeeModel {
            forex_commission_rate: dec!(0.000002),  // 0.20 bps
            forex_minimum_fee: dec!(2.00),
            options_fee_per_contract: dec!(0.70),
            futures_fee_per_contract: dec!(0.85),
        }
    }
}

impl FeeModel for InteractiveBrokersFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        let order = params.order;

        // Free exercise for equity options
        if order.order_type == OrderType::OptionExercise
            && params.security_type == SecurityType::Option
        {
            return OrderFee::zero();
        }

        match params.security_type {
            SecurityType::Equity => {
                // $0.005/share, min $1, max 0.5% of trade value
                // (mirrors C# InteractiveBrokersFeeModel: feePerShare=0.005, minFee=1, maxFeeRate=0.005)
                let shares = order.abs_quantity();
                let raw = shares * dec!(0.005);
                let notional = shares * params.security_price;
                let max_fee = notional * dec!(0.005);
                let fee = if raw < dec!(1.00) {
                    dec!(1.00)
                } else if raw > max_fee {
                    max_fee
                } else {
                    raw
                };
                OrderFee::new(fee, "USD")
            }
            SecurityType::Option | SecurityType::FutureOption | SecurityType::IndexOption => {
                let fee = order.abs_quantity() * self.options_fee_per_contract;
                OrderFee::new(fee, "USD")
            }
            SecurityType::Future | SecurityType::CryptoFuture => {
                let fee = order.abs_quantity() * self.futures_fee_per_contract;
                OrderFee::new(fee, "USD")
            }
            SecurityType::Forex => {
                let notional = order.abs_quantity()
                    * params.security_price
                    * params.contract_multiplier;
                let fee = (self.forex_commission_rate * notional)
                    .abs()
                    .max(self.forex_minimum_fee);
                OrderFee::new(fee, "USD")
            }
            _ => OrderFee::zero(),
        }
    }
}

// ─── BinanceFeeModel ──────────────────────────────────────────────────────────

/// Models Binance spot trading fees.
///
/// Default tier: maker 0.10%, taker 0.10% (BNB discount not modelled).
/// See https://www.binance.com/en/fee/schedule
pub struct BinanceFeeModel {
    pub maker_fee: Decimal,
    pub taker_fee: Decimal,
}

impl Default for BinanceFeeModel {
    fn default() -> Self {
        BinanceFeeModel { maker_fee: dec!(0.001), taker_fee: dec!(0.001) }
    }
}

impl BinanceFeeModel {
    pub fn new(maker_fee: Decimal, taker_fee: Decimal) -> Self {
        BinanceFeeModel { maker_fee, taker_fee }
    }
}

impl FeeModel for BinanceFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        let order = params.order;
        let fee_rate = if is_maker(order) { self.maker_fee } else { self.taker_fee };

        match order.direction() {
            OrderDirection::Buy => {
                // Fee in base currency (what was received)
                let base = params.base_currency.clone().unwrap_or_else(|| "BTC".into());
                let fee = order.abs_quantity() * fee_rate;
                OrderFee::new(fee, base)
            }
            _ => {
                // Fee in quote currency
                let notional = order.abs_quantity()
                    * params.security_price
                    * params.contract_multiplier;
                OrderFee::new(notional * fee_rate, &params.quote_currency)
            }
        }
    }
}

// ─── AlpacaFeeModel ───────────────────────────────────────────────────────────

/// Alpaca fee model.
///
/// Equities: $0 commission.
/// Crypto:   maker 0.15%, taker 0.25%.
/// See https://docs.alpaca.markets/docs/crypto-fees
pub struct AlpacaFeeModel;

impl AlpacaFeeModel {
    pub const MAKER_CRYPTO_FEE: Decimal = dec!(0.0015);
    pub const TAKER_CRYPTO_FEE: Decimal = dec!(0.0025);
}

impl FeeModel for AlpacaFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        if params.security_type == SecurityType::Crypto {
            let fee_rate = if is_maker(params.order) {
                Self::MAKER_CRYPTO_FEE
            } else {
                Self::TAKER_CRYPTO_FEE
            };

            match params.order.direction() {
                OrderDirection::Buy => {
                    let base = params.base_currency.clone().unwrap_or_else(|| "BTC".into());
                    OrderFee::new(params.order.abs_quantity() * fee_rate, base)
                }
                _ => {
                    let notional = params.order.abs_quantity() * params.security_price;
                    OrderFee::new(notional * fee_rate, &params.quote_currency)
                }
            }
        } else {
            OrderFee::zero()
        }
    }
}

// ─── TradierFeeModel ──────────────────────────────────────────────────────────

/// Tradier fee model — $0 equities, $0.35/contract options.
pub struct TradierFeeModel;

impl FeeModel for TradierFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        match params.security_type {
            SecurityType::Option => {
                let fee = params.order.abs_quantity() * dec!(0.35);
                OrderFee::new(fee, "USD")
            }
            _ => OrderFee::zero(),
        }
    }
}

// ─── GDAXFeeModel / CoinbaseFeeModel ─────────────────────────────────────────

/// Coinbase Advanced Trade fee model (formerly GDAX).
///
/// Default (Advanced 1): maker 0.60%, taker 0.80%.
/// See https://www.coinbase.com/advanced-fees
pub struct GDAXFeeModel {
    pub maker_fee: Decimal,
    pub taker_fee: Decimal,
}

impl Default for GDAXFeeModel {
    fn default() -> Self {
        // Advanced 1 defaults
        GDAXFeeModel { maker_fee: dec!(0.006), taker_fee: dec!(0.008) }
    }
}

impl GDAXFeeModel {
    pub fn new(maker_fee: Decimal, taker_fee: Decimal) -> Self {
        GDAXFeeModel { maker_fee, taker_fee }
    }
}

impl FeeModel for GDAXFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        let fee_rate = if is_maker(params.order) { self.maker_fee } else { self.taker_fee };
        let unit_price = params.security_price * params.contract_multiplier;
        let fee = unit_price * params.order.abs_quantity() * fee_rate;
        OrderFee::new(fee, &params.quote_currency)
    }
}

/// Type alias — GDAXFeeModel IS the Coinbase fee model.
pub type CoinbaseFeeModel = GDAXFeeModel;

// ─── KrakenFeeModel ───────────────────────────────────────────────────────────

/// Kraken fee model.
///
/// Tier-1 (no 30-day volume tracking): maker 0.16%, taker 0.26%.
/// FX / stablecoin pairs: 0.20%.
/// See https://www.kraken.com/features/fee-schedule
pub struct KrakenFeeModel {
    pub maker_fee: Decimal,
    pub taker_fee: Decimal,
    pub fx_fee: Decimal,
}

impl Default for KrakenFeeModel {
    fn default() -> Self {
        KrakenFeeModel {
            maker_fee: dec!(0.0016),
            taker_fee: dec!(0.0026),
            fx_fee: dec!(0.002),
        }
    }
}

impl FeeModel for KrakenFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        let order = params.order;
        let fee_rate = if is_maker(order) { self.maker_fee } else { self.taker_fee };

        let unit_price = params.security_price * params.contract_multiplier;

        match order.direction() {
            OrderDirection::Buy => {
                // fee in quote currency
                let fee = unit_price * order.abs_quantity() * fee_rate;
                OrderFee::new(fee, &params.quote_currency)
            }
            _ => {
                // fee in base currency
                let base = params.base_currency.clone().unwrap_or_else(|| "BTC".into());
                let fee = order.abs_quantity() * fee_rate;
                OrderFee::new(fee, base)
            }
        }
    }
}

// ─── BybitFeeModel ────────────────────────────────────────────────────────────

/// Bybit spot fee model.
///
/// Non-VIP tier: maker 0.10%, taker 0.10%.
/// Perpetuals / CryptoFuture: maker 0.02%, taker 0.055%.
/// See https://learn.bybit.com/bybit-guide/bybit-trading-fees/
pub struct BybitFeeModel {
    pub maker_fee: Decimal,
    pub taker_fee: Decimal,
}

impl Default for BybitFeeModel {
    fn default() -> Self {
        BybitFeeModel { maker_fee: dec!(0.001), taker_fee: dec!(0.001) }
    }
}

impl BybitFeeModel {
    pub fn new(maker_fee: Decimal, taker_fee: Decimal) -> Self {
        BybitFeeModel { maker_fee, taker_fee }
    }

    /// Perpetuals preset: maker 0.02%, taker 0.055%.
    pub fn perpetuals() -> Self {
        BybitFeeModel { maker_fee: dec!(0.0002), taker_fee: dec!(0.00055) }
    }
}

impl FeeModel for BybitFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        let order = params.order;
        let fee_rate = if is_maker(order) { self.maker_fee } else { self.taker_fee };

        if params.security_type == SecurityType::CryptoFuture {
            // Position value in quote currency
            let notional = order.abs_quantity()
                * params.security_price
                * params.contract_multiplier;
            return OrderFee::new(notional * fee_rate, &params.quote_currency);
        }

        match order.direction() {
            OrderDirection::Buy => {
                let base = params.base_currency.clone().unwrap_or_else(|| "BTC".into());
                OrderFee::new(order.abs_quantity() * fee_rate, base)
            }
            _ => {
                let unit_price = params.security_price * params.contract_multiplier;
                let fee = unit_price * order.abs_quantity() * fee_rate;
                OrderFee::new(fee, &params.quote_currency)
            }
        }
    }
}

// ─── CharlesSchwabFeeModel ────────────────────────────────────────────────────

/// Charles Schwab fee model.
///
/// Equities: $0.
/// Options (equity): $0.65/contract.
/// Options (index): $1.00/contract.
/// See https://www.schwab.com/pricing
pub struct CharlesSchwabFeeModel;

impl FeeModel for CharlesSchwabFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        match params.security_type {
            SecurityType::Option => {
                let fee = params.order.abs_quantity() * dec!(0.65);
                OrderFee::new(fee, "USD")
            }
            SecurityType::IndexOption => {
                let fee = params.order.abs_quantity() * dec!(1.00);
                OrderFee::new(fee, "USD")
            }
            _ => OrderFee::zero(),
        }
    }
}

// ─── TDAmeritradeFeeModel ─────────────────────────────────────────────────────

/// TD Ameritrade fee model (now merged with Schwab).
///
/// Equities: $0.
/// Options: $0.65/contract.
pub struct TDAmeritradeFeeModel;

impl FeeModel for TDAmeritradeFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        match params.security_type {
            SecurityType::Option | SecurityType::FutureOption | SecurityType::IndexOption => {
                let fee = params.order.abs_quantity() * dec!(0.65);
                OrderFee::new(fee, "USD")
            }
            _ => OrderFee::zero(),
        }
    }
}

// ─── RobinhoodFeeModel ────────────────────────────────────────────────────────

/// Robinhood fee model — zero commission on everything.
pub struct RobinhoodFeeModel;

impl FeeModel for RobinhoodFeeModel {
    fn get_order_fee(&self, _params: &OrderFeeParameters<'_>) -> OrderFee {
        OrderFee::zero()
    }
}

// ─── EtradeFeeModel ───────────────────────────────────────────────────────────

/// E*TRADE fee model.
///
/// Equities: $0.
/// Options: $0.65/contract.
pub struct EtradeFeeModel;

impl FeeModel for EtradeFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        match params.security_type {
            SecurityType::Option | SecurityType::FutureOption | SecurityType::IndexOption => {
                let fee = params.order.abs_quantity() * dec!(0.65);
                OrderFee::new(fee, "USD")
            }
            _ => OrderFee::zero(),
        }
    }
}

// ─── FidelityFeeModel ─────────────────────────────────────────────────────────

/// Fidelity fee model.
///
/// Equities: $0.
/// Options: $0.65/contract.
pub struct FidelityFeeModel;

impl FeeModel for FidelityFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        match params.security_type {
            SecurityType::Option | SecurityType::FutureOption | SecurityType::IndexOption => {
                let fee = params.order.abs_quantity() * dec!(0.65);
                OrderFee::new(fee, "USD")
            }
            _ => OrderFee::zero(),
        }
    }
}

// ─── OandaFeeModel ────────────────────────────────────────────────────────────

/// OANDA fee model — spread-based forex, zero explicit commission.
pub struct OandaFeeModel;

impl FeeModel for OandaFeeModel {
    fn get_order_fee(&self, _params: &OrderFeeParameters<'_>) -> OrderFee {
        OrderFee::zero()
    }
}

// ─── FxcmFeeModel ─────────────────────────────────────────────────────────────

/// FXCM fee model.
///
/// Forex: $0.04/1k lot for major pairs, $0.06/1k lot for others.
/// CFD: no commission.
/// See https://www.fxcm.com/
pub struct FxcmFeeModel {
    pub currency: String,
}

impl Default for FxcmFeeModel {
    fn default() -> Self {
        FxcmFeeModel { currency: "USD".into() }
    }
}

impl FxcmFeeModel {
    /// Major FX pairs that attract the lower $0.04/1k lot rate.
    const MAJOR_PAIRS: &'static [&'static str] = &[
        "EURUSD", "GBPUSD", "USDJPY", "USDCHF", "AUDUSD", "EURJPY", "GBPJPY",
    ];

    pub fn new(currency: impl Into<String>) -> Self {
        FxcmFeeModel { currency: currency.into() }
    }

    fn is_major(ticker: &str) -> bool {
        Self::MAJOR_PAIRS.contains(&ticker.to_uppercase().as_str())
    }
}

impl FeeModel for FxcmFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        if params.security_type != SecurityType::Forex {
            return OrderFee::zero();
        }

        let ticker = &params.order.symbol.value;
        let rate = if Self::is_major(ticker) { dec!(0.04) } else { dec!(0.06) };
        // $rate per side per 1k lot
        let fee = (rate * params.order.abs_quantity() / dec!(1000)).abs();
        OrderFee::new(fee, &self.currency)
    }
}

// ─── ExchangeFeeModel ─────────────────────────────────────────────────────────

/// U.S. exchange regulatory fee model.
///
/// Applies two standard regulatory fees on equity sell orders:
/// - SEC fee: 0.0000278 × sell value (rounded up to nearest cent)
/// - FINRA TAF: $0.000145/share, capped at $7.27/trade
///
/// These are charged by the exchange/FINRA, not the broker, and apply
/// in addition to any brokerage commission.
pub struct ExchangeFeeModel;

impl ExchangeFeeModel {
    /// SEC Section 31 fee rate (as of 2024).
    pub const SEC_FEE_RATE: Decimal = dec!(0.0000278);
    /// FINRA Trading Activity Fee per share.
    pub const FINRA_TAF_RATE: Decimal = dec!(0.000145);
    /// FINRA TAF maximum per trade.
    pub const FINRA_TAF_MAX: Decimal = dec!(7.27);
}

impl FeeModel for ExchangeFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters<'_>) -> OrderFee {
        // Regulatory fees only apply to equity sell orders
        if params.security_type != SecurityType::Equity {
            return OrderFee::zero();
        }
        if params.order.direction() != OrderDirection::Sell {
            return OrderFee::zero();
        }

        let qty = params.order.abs_quantity();
        let sell_value = qty * params.security_price;

        // SEC fee
        let sec_fee = sell_value * Self::SEC_FEE_RATE;

        // FINRA TAF
        let finra_taf = (qty * Self::FINRA_TAF_RATE).min(Self::FINRA_TAF_MAX);

        OrderFee::new(sec_fee + finra_taf, "USD")
    }
}
