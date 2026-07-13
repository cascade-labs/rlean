use rlean_core::{LeanError, Market, Resolution, Result, Symbol};
use rlean_data::{
    live_data_channel, DataQueueHandler, LiveDataItem, LiveDataSubscription,
    LiveDataSubscriptionConfig, LiveUniverseSubscriptionConfig, SubscriptionDataConfig,
};
use rlean_live::DataQueueHandlerManager;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct Calls {
    subscribed: Arc<Mutex<Vec<String>>>,
    unsubscribed: Arc<Mutex<Vec<String>>>,
}

impl Calls {
    fn new() -> Self {
        Self {
            subscribed: Arc::new(Mutex::new(Vec::new())),
            unsubscribed: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

struct TestHandler {
    name: &'static str,
    accepts_market: bool,
    connected: bool,
    calls: Calls,
}

struct UniverseTestHandler {
    name: &'static str,
    accepts_universe: bool,
    calls: Calls,
}

impl UniverseTestHandler {
    fn new(name: &'static str, accepts_universe: bool, calls: Calls) -> Self {
        Self {
            name,
            accepts_universe,
            calls,
        }
    }
}

impl TestHandler {
    fn new(name: &'static str, accepts_market: bool, calls: Calls) -> Self {
        Self {
            name,
            accepts_market,
            connected: true,
            calls,
        }
    }

    fn with_connected(mut self, connected: bool) -> Self {
        self.connected = connected;
        self
    }
}

impl DataQueueHandler for TestHandler {
    fn subscribe(&mut self, config: &SubscriptionDataConfig) -> Result<LiveDataSubscription> {
        self.calls
            .subscribed
            .lock()
            .unwrap()
            .push(config.symbol.value.to_string());

        if !self.accepts_market {
            return Err(LeanError::Unsupported(format!(
                "{} does not support {}",
                self.name, config.symbol
            )));
        }

        let (sender, receiver) = live_data_channel();
        sender
            .send(Ok(LiveDataItem::Heartbeat(rlean_core::DateTime::EPOCH)))
            .unwrap();
        Ok(LiveDataSubscription::new(
            LiveDataSubscriptionConfig::Market(Box::new(config.clone())),
            receiver,
        ))
    }

    fn unsubscribe(&mut self, config: &SubscriptionDataConfig) -> Result<()> {
        self.calls
            .unsubscribed
            .lock()
            .unwrap()
            .push(config.symbol.value.to_string());
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }

    fn name(&self) -> &str {
        self.name
    }
}

impl DataQueueHandler for UniverseTestHandler {
    fn subscribe(&mut self, config: &SubscriptionDataConfig) -> Result<LiveDataSubscription> {
        Err(LeanError::Unsupported(format!(
            "{} does not support market {}",
            self.name, config.symbol
        )))
    }

    fn subscribe_universe(
        &mut self,
        subscription: &LiveUniverseSubscriptionConfig,
    ) -> Result<LiveDataSubscription> {
        self.calls
            .subscribed
            .lock()
            .unwrap()
            .push(subscription.ticker.clone());

        if !self.accepts_universe {
            return Err(LeanError::Unsupported(format!(
                "{} does not support universe {}:{}",
                self.name, subscription.source_type, subscription.ticker
            )));
        }

        let (sender, receiver) = live_data_channel();
        sender
            .send(Ok(LiveDataItem::UniverseData {
                source_type: subscription.source_type.clone(),
                ticker: subscription.ticker.clone(),
                resolution: subscription.resolution,
                time: rlean_core::DateTime::EPOCH,
                data: Vec::new(),
            }))
            .unwrap();
        Ok(LiveDataSubscription::new(
            LiveDataSubscriptionConfig::Universe(subscription.clone()),
            receiver,
        ))
    }

    fn unsubscribe(&mut self, _config: &SubscriptionDataConfig) -> Result<()> {
        Ok(())
    }

    fn unsubscribe_universe(
        &mut self,
        subscription: &LiveUniverseSubscriptionConfig,
    ) -> Result<()> {
        self.calls
            .unsubscribed
            .lock()
            .unwrap()
            .push(subscription.ticker.clone());
        Ok(())
    }

    fn is_connected(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        self.name
    }
}

fn crypto_config(ticker: &str) -> SubscriptionDataConfig {
    SubscriptionDataConfig::new_crypto(
        Symbol::create_crypto(ticker, &Market::binance()),
        Resolution::Minute,
    )
}

#[test]
fn subscribe_falls_back_to_next_handler_when_unsupported() {
    let first_calls = Calls::new();
    let second_calls = Calls::new();
    let mut manager = DataQueueHandlerManager::new(vec![
        Box::new(TestHandler::new("first", false, first_calls.clone())),
        Box::new(TestHandler::new("second", true, second_calls.clone())),
    ]);
    let config = crypto_config("BTCUSDT");

    let subscription = manager.subscribe(&config).unwrap();

    assert!(matches!(
        subscription.receiver.recv().unwrap().unwrap(),
        LiveDataItem::Heartbeat(_)
    ));
    assert_eq!(
        first_calls.subscribed.lock().unwrap().as_slice(),
        &["BTCUSDT"]
    );
    assert_eq!(
        second_calls.subscribed.lock().unwrap().as_slice(),
        &["BTCUSDT"]
    );
}

#[test]
fn unsubscribe_is_routed_to_handler_that_accepted_subscription() {
    let first_calls = Calls::new();
    let second_calls = Calls::new();
    let mut manager = DataQueueHandlerManager::new(vec![
        Box::new(TestHandler::new("first", false, first_calls.clone())),
        Box::new(TestHandler::new("second", true, second_calls.clone())),
    ]);
    let config = crypto_config("ETHUSDT");

    manager.subscribe(&config).unwrap();
    manager.unsubscribe(&config).unwrap();

    assert!(first_calls.unsubscribed.lock().unwrap().is_empty());
    assert_eq!(
        second_calls.unsubscribed.lock().unwrap().as_slice(),
        &["ETHUSDT"]
    );
}

#[test]
fn duplicate_subscription_is_rejected() {
    let calls = Calls::new();
    let mut manager =
        DataQueueHandlerManager::new(vec![Box::new(TestHandler::new("only", true, calls))]);
    let config = crypto_config("SOLUSDT");

    manager.subscribe(&config).unwrap();
    let err = match manager.subscribe(&config) {
        Ok(_) => panic!("duplicate subscription should fail"),
        Err(err) => err,
    };

    assert!(matches!(err, LeanError::DataError(_)));
}

#[test]
fn universe_subscription_is_stacked_and_routed_to_owner() {
    let first_calls = Calls::new();
    let second_calls = Calls::new();
    let mut manager = DataQueueHandlerManager::new(vec![
        Box::new(UniverseTestHandler::new(
            "first",
            false,
            first_calls.clone(),
        )),
        Box::new(UniverseTestHandler::new(
            "second",
            true,
            second_calls.clone(),
        )),
    ]);
    let config = LiveUniverseSubscriptionConfig {
        source_type: "hyperliquid".to_string(),
        ticker: "HIP3_XYZ".to_string(),
        resolution: Resolution::Hour,
        properties: Default::default(),
    };

    let subscription = manager.subscribe_universe(&config).unwrap();
    assert!(matches!(
        subscription.receiver.recv().unwrap().unwrap(),
        LiveDataItem::UniverseData { .. }
    ));
    manager.unsubscribe_universe(&config).unwrap();

    assert_eq!(
        first_calls.subscribed.lock().unwrap().as_slice(),
        &["HIP3_XYZ"]
    );
    assert_eq!(
        second_calls.subscribed.lock().unwrap().as_slice(),
        &["HIP3_XYZ"]
    );
    assert!(first_calls.unsubscribed.lock().unwrap().is_empty());
    assert_eq!(
        second_calls.unsubscribed.lock().unwrap().as_slice(),
        &["HIP3_XYZ"]
    );
}

#[test]
fn is_connected_ignores_idle_fallback_handlers() {
    let first_calls = Calls::new();
    let second_calls = Calls::new();
    let mut manager = DataQueueHandlerManager::new(vec![
        Box::new(TestHandler::new("first", true, first_calls)),
        Box::new(TestHandler::new("second", false, second_calls).with_connected(false)),
    ]);
    let config = crypto_config("BTCUSDT");

    manager.subscribe(&config).unwrap();

    assert!(manager.is_connected());
}
