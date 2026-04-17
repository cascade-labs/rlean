use lean_core::Price;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Models transaction costs (commissions + fees).
pub trait SecurityTransactionModel: Send + Sync {
    fn get_order_fee(&self, order_fee_parameters: &OrderFeeParameters) -> OrderFee;
}

#[derive(Debug, Clone)]
pub struct OrderFeeParameters {
    pub security_price: Price,
    pub order_quantity: Decimal,
    pub order_direction: crate::order::OrderDirection,
}

#[derive(Debug, Clone)]
pub struct OrderFee {
    pub value: Price,
    pub currency: String,
}

impl OrderFee {
    pub fn zero() -> Self {
        OrderFee {
            value: dec!(0),
            currency: "USD".into(),
        }
    }
    pub fn flat(amount: Price, currency: &str) -> Self {
        OrderFee {
            value: amount,
            currency: currency.to_string(),
        }
    }
}

/// Zero-fee model for crypto or testing.
pub struct NullFeeModel;

impl SecurityTransactionModel for NullFeeModel {
    fn get_order_fee(&self, _params: &OrderFeeParameters) -> OrderFee {
        OrderFee::zero()
    }
}

/// Interactive Brokers tiered equity commission model.
/// $0.005/share, min $1.00, max 1% of trade value.
pub struct InteractiveBrokersFeeModel;

impl SecurityTransactionModel for InteractiveBrokersFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters) -> OrderFee {
        let qty = params.order_quantity.abs();
        let raw_fee = qty * dec!(0.005);
        let min_fee = dec!(1.0);
        let max_fee = params.security_price * qty * dec!(0.01);

        let fee = raw_fee.max(min_fee).min(max_fee);
        OrderFee::flat(fee, "USD")
    }
}

/// Binance fee model — 0.1% taker, 0.1% maker.
pub struct BinanceFeeModel {
    pub taker_rate: Decimal,
    pub maker_rate: Decimal,
}

impl Default for BinanceFeeModel {
    fn default() -> Self {
        BinanceFeeModel {
            taker_rate: dec!(0.001),
            maker_rate: dec!(0.001),
        }
    }
}

impl SecurityTransactionModel for BinanceFeeModel {
    fn get_order_fee(&self, params: &OrderFeeParameters) -> OrderFee {
        let trade_value = params.security_price * params.order_quantity.abs();
        let fee = trade_value * self.taker_rate;
        OrderFee::flat(fee, "USD")
    }
}

/// Flat-rate per-trade commission (e.g., Alpaca, Robinhood: $0).
pub struct FlatFeeModel {
    pub fee: Price,
}

impl FlatFeeModel {
    pub fn zero() -> Self {
        FlatFeeModel { fee: dec!(0) }
    }
    pub fn new(fee: Price) -> Self {
        FlatFeeModel { fee }
    }
}

impl SecurityTransactionModel for FlatFeeModel {
    fn get_order_fee(&self, _params: &OrderFeeParameters) -> OrderFee {
        OrderFee::flat(self.fee, "USD")
    }
}
