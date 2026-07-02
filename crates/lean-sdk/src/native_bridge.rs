use lean_algorithm::algorithm::{DataDeliveryPayload, QcAlgorithmStrategy, SecurityChanges};
use lean_algorithm::charting::ChartCollection;
use lean_algorithm::lifecycle::{
    AlgorithmServices, AlgorithmStateAccess, LifecycleBridge, OptionSubscription, UniverseSelection,
};
use lean_alpha::AlphaAnalytics;
use lean_core::{DateTime, Price, Resolution, Symbol};
use lean_data::{
    CustomDataPoint, Delisting, Dividend, Split, SubscriptionDataConfig, SymbolChangedEvent,
};
use lean_options::OptionContract;
use lean_orders::{Order, OrderEvent, TransactionManager};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;

pub struct QcAlgorithmNativeBridge {
    strategy: Box<dyn QcAlgorithmStrategy>,
}

impl QcAlgorithmNativeBridge {
    pub fn new(strategy: Box<dyn QcAlgorithmStrategy>) -> Self {
        Self { strategy }
    }

    fn state(&self) -> Arc<std::sync::Mutex<lean_algorithm::qc_algorithm::QcAlgorithm>> {
        self.strategy.algorithm_state()
    }
}

impl AlgorithmStateAccess for QcAlgorithmNativeBridge {
    fn algorithm_state(
        &self,
    ) -> Option<Arc<std::sync::Mutex<lean_algorithm::qc_algorithm::QcAlgorithm>>> {
        Some(self.state())
    }
}

impl LifecycleBridge for QcAlgorithmNativeBridge {
    fn initialize(&mut self, _services: &mut dyn AlgorithmServices) -> anyhow::Result<()> {
        self.strategy.initialize().map_err(Into::into)
    }

    fn on_data(&mut self, payload: DataDeliveryPayload, _services: &mut dyn AlgorithmServices) {
        self.strategy.on_data(payload);
    }

    fn on_order_event(&mut self, event: &OrderEvent, _services: &mut dyn AlgorithmServices) {
        self.strategy.on_order_event(event);
    }

    fn on_otm_expiry(
        &mut self,
        _contract: OptionContract,
        _quantity: Decimal,
        _underlying_price: Decimal,
        _entry_premium: Decimal,
        _services: &mut dyn AlgorithmServices,
    ) {
    }

    fn on_assignment_order_event(
        &mut self,
        _contract: OptionContract,
        _quantity: Decimal,
        _is_assignment: bool,
        _services: &mut dyn AlgorithmServices,
    ) {
    }

    fn on_end_of_day(&mut self, symbol: Option<Symbol>, _services: &mut dyn AlgorithmServices) {
        self.strategy.on_end_of_day(symbol);
    }

    fn on_warmup_finished(&mut self, _services: &mut dyn AlgorithmServices) {
        self.strategy.on_warmup_finished();
    }

    fn on_end_of_algorithm(&mut self, _services: &mut dyn AlgorithmServices) {
        self.strategy.on_end_of_algorithm();
    }

    fn on_margin_call(&mut self, requests: &[Order], _services: &mut dyn AlgorithmServices) {
        self.strategy.on_margin_call(requests);
    }

    fn on_margin_call_warning(&mut self, _services: &mut dyn AlgorithmServices) {
        self.strategy.on_margin_call_warning();
    }

    fn on_securities_changed(
        &mut self,
        changes: &SecurityChanges,
        _services: &mut dyn AlgorithmServices,
    ) {
        self.strategy.on_securities_changed(changes);
    }

    fn on_splits(&mut self, _splits: &HashMap<u64, Split>, _services: &mut dyn AlgorithmServices) {}

    fn on_dividends(
        &mut self,
        _dividends: &HashMap<u64, Dividend>,
        _services: &mut dyn AlgorithmServices,
    ) {
    }

    fn on_delistings(
        &mut self,
        _delistings: &HashMap<u64, Delisting>,
        _services: &mut dyn AlgorithmServices,
    ) {
    }

    fn on_symbol_changed_events(
        &mut self,
        _events: &HashMap<u64, SymbolChangedEvent>,
        _services: &mut dyn AlgorithmServices,
    ) {
    }

    fn select_universe_changes(
        &mut self,
        _utc_ns: i64,
        _resolution: Resolution,
        _services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        Vec::new()
    }

    fn select_custom_universe_changes(
        &mut self,
        _utc_ns: i64,
        _resolution: Resolution,
        _custom_data: &HashMap<String, Vec<CustomDataPoint>>,
        _services: &mut dyn AlgorithmServices,
    ) -> Vec<UniverseSelection> {
        Vec::new()
    }

    fn on_end_of_time_step(&mut self, _services: &mut dyn AlgorithmServices) {}

    fn on_brokerage_message(&mut self, _message: &str, _services: &mut dyn AlgorithmServices) {}

    fn on_brokerage_disconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

    fn on_brokerage_reconnect(&mut self, _services: &mut dyn AlgorithmServices) {}

    fn name(&self) -> &str {
        "QcAlgorithmNative"
    }

    fn start_date(&self) -> DateTime {
        self.state().lock().unwrap().start_date
    }

    fn end_date(&self) -> DateTime {
        self.state().lock().unwrap().end_date
    }

    fn portfolio_value(&self) -> Price {
        self.state().lock().unwrap().portfolio_value()
    }

    fn starting_cash(&self) -> Price {
        self.state().lock().unwrap().portfolio_value()
    }

    fn subscriptions(&self) -> Vec<SubscriptionDataConfig> {
        self.state()
            .lock()
            .unwrap()
            .subscription_manager
            .get_all()
            .into_iter()
            .map(|config| (*config).clone())
            .collect()
    }

    fn prepare_data_delivery(
        &mut self,
        _subscriptions: &[SubscriptionDataConfig],
    ) -> anyhow::Result<()> {
        Ok(())
    }

    fn option_subscriptions(&self) -> Vec<OptionSubscription> {
        let algorithm = self.state();
        let algorithm = algorithm.lock().unwrap();
        algorithm
            .option_subscriptions
            .iter()
            .cloned()
            .map(|canonical| {
                let resolution = algorithm
                    .option_subscription_resolutions
                    .get(canonical.permtick.as_ref())
                    .copied()
                    .unwrap_or(Resolution::Daily);
                let filter = algorithm
                    .option_filters
                    .get(canonical.permtick.as_ref())
                    .copied()
                    .unwrap_or_default();
                OptionSubscription {
                    canonical,
                    resolution,
                    filter,
                }
            })
            .collect()
    }

    fn portfolio(&self) -> Option<Arc<lean_algorithm::portfolio::SecurityPortfolioManager>> {
        Some(self.state().lock().unwrap().portfolio.clone())
    }

    fn transactions(&self) -> Option<Arc<TransactionManager>> {
        Some(self.state().lock().unwrap().transactions.clone())
    }

    fn order_fee(&self, order: &Order, fill_price: Price) -> Price {
        self.state()
            .lock()
            .unwrap()
            .order_fee(order, fill_price)
            .amount
    }

    fn contract_multiplier_for_symbol(&self, symbol: &Symbol) -> Price {
        self.state()
            .lock()
            .unwrap()
            .contract_multiplier_for_symbol(symbol)
    }

    fn validate_order_buying_power(
        &self,
        order: &Order,
        fill_price: Price,
        fee: Price,
    ) -> std::result::Result<(), String> {
        self.state()
            .lock()
            .unwrap()
            .validate_order_buying_power(order, fill_price, fee)
    }

    fn has_universes(&self) -> bool {
        false
    }

    fn universe_resolution(&self) -> Option<Resolution> {
        None
    }

    fn alpha_analytics(&self) -> AlphaAnalytics {
        AlphaAnalytics::default()
    }

    fn charts(&self) -> ChartCollection {
        ChartCollection::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_algorithm::lifecycle::NoopAlgorithmServices;
    use lean_algorithm::qc_algorithm::OptionFilter;
    use rust_decimal_macros::dec;

    struct QcBackedTestStrategy {
        state: Arc<std::sync::Mutex<lean_algorithm::qc_algorithm::QcAlgorithm>>,
    }

    impl QcBackedTestStrategy {
        fn new() -> Self {
            Self {
                state: Arc::new(std::sync::Mutex::new(
                    lean_algorithm::qc_algorithm::QcAlgorithm::new("native", dec!(100000)),
                )),
            }
        }
    }

    impl QcAlgorithmStrategy for QcBackedTestStrategy {
        fn algorithm_state(
            &self,
        ) -> Arc<std::sync::Mutex<lean_algorithm::qc_algorithm::QcAlgorithm>> {
            self.state.clone()
        }

        fn initialize(&mut self) -> lean_core::Result<()> {
            let mut algorithm = self.state.lock().unwrap();
            algorithm.add_equity("SPY", Resolution::Daily);
            let option = algorithm.add_option("SPY", Resolution::Daily);
            algorithm.set_option_filter(
                &option,
                OptionFilter {
                    min_strike_rank: -1,
                    max_strike_rank: 1,
                    min_expiry_days: 0,
                    max_expiry_days: 30,
                },
            );
            Ok(())
        }
    }

    #[test]
    fn qc_algorithm_native_bridge_surfaces_shared_state() {
        let mut bridge = QcAlgorithmNativeBridge::new(Box::new(QcBackedTestStrategy::new()));
        let mut services = NoopAlgorithmServices::default();
        bridge.initialize(&mut services).unwrap();

        assert!(bridge.algorithm_state().is_some());
        assert_eq!(bridge.subscriptions().len(), 1);
        let option_subscriptions = bridge.option_subscriptions();
        assert_eq!(option_subscriptions.len(), 1);
        assert_eq!(option_subscriptions[0].canonical.permtick.as_ref(), "?SPY");
        assert_eq!(option_subscriptions[0].filter.min_strike_rank, -1);
        assert_eq!(option_subscriptions[0].filter.max_expiry_days, 30);
    }
}
