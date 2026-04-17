use lean_orders::security_transaction_model::{FlatFeeModel, SecurityTransactionModel};

pub trait BrokerageModel: Send + Sync {
    fn name(&self) -> &str;
    fn transaction_model(&self) -> Box<dyn SecurityTransactionModel>;
    fn default_leverage(&self) -> f64;
    fn can_submit_order(&self) -> bool {
        true
    }
    fn can_update_order(&self) -> bool {
        true
    }
    fn can_execute_order(&self) -> bool {
        true
    }
}

pub struct DefaultBrokerageModel;

impl BrokerageModel for DefaultBrokerageModel {
    fn name(&self) -> &str {
        "Default"
    }
    fn transaction_model(&self) -> Box<dyn SecurityTransactionModel> {
        Box::new(FlatFeeModel::zero())
    }
    fn default_leverage(&self) -> f64 {
        1.0
    }
}
