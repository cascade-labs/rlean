use lean_orders::security_transaction_model::{
    BinanceFeeModel, FlatFeeModel, InteractiveBrokersFeeModel, SecurityTransactionModel,
};

pub trait BrokerageModel: Send + Sync {
    fn name(&self) -> &str;
    fn transaction_model(&self) -> Box<dyn SecurityTransactionModel>;
    fn default_leverage(&self) -> f64;
    fn can_submit_order(&self) -> bool { true }
    fn can_update_order(&self) -> bool { true }
    fn can_execute_order(&self) -> bool { true }
}

pub struct DefaultBrokerageModel;

impl BrokerageModel for DefaultBrokerageModel {
    fn name(&self) -> &str { "Default" }
    fn transaction_model(&self) -> Box<dyn SecurityTransactionModel> {
        Box::new(FlatFeeModel::zero())
    }
    fn default_leverage(&self) -> f64 { 1.0 }
}

pub struct InteractiveBrokersModel;

impl BrokerageModel for InteractiveBrokersModel {
    fn name(&self) -> &str { "InteractiveBrokers" }
    fn transaction_model(&self) -> Box<dyn SecurityTransactionModel> {
        Box::new(InteractiveBrokersFeeModel)
    }
    fn default_leverage(&self) -> f64 { 2.0 }
}

pub struct BinanceModel;

impl BrokerageModel for BinanceModel {
    fn name(&self) -> &str { "Binance" }
    fn transaction_model(&self) -> Box<dyn SecurityTransactionModel> {
        Box::new(BinanceFeeModel::default())
    }
    fn default_leverage(&self) -> f64 { 1.0 }
}

pub struct AlpacaModel;

impl BrokerageModel for AlpacaModel {
    fn name(&self) -> &str { "Alpaca" }
    fn transaction_model(&self) -> Box<dyn SecurityTransactionModel> {
        Box::new(FlatFeeModel::zero())
    }
    fn default_leverage(&self) -> f64 { 4.0 }
}
