use crate::{
    CustomDataPoint, CustomDataSubscription, MarginInterestRate, OrderBook, PerpetualContext,
    QuoteBar, Slice, SubscriptionDataConfig, Tick, TradeBar,
};
use crossbeam_channel::{bounded, Receiver, Sender};
use lean_core::{DateTime, LeanError, Resolution, Result, Symbol};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Live packet metadata supplied to data queue handlers at deployment startup.
///
/// This mirrors LEAN's `LiveNodePacket` in the parts the queue layer needs:
/// provider stack, brokerage name, arbitrary brokerage/provider settings, and
/// the paper/live execution flag.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveNodePacket {
    pub brokerage: String,
    pub data_queue_handlers: Vec<String>,
    pub brokerage_data: HashMap<String, String>,
    pub parameters: HashMap<String, String>,
    pub paper_trading: bool,
}

#[derive(Debug, Clone)]
pub enum LiveDataSubscriptionConfig {
    Market(SubscriptionDataConfig),
    Custom(Box<CustomDataSubscription>),
    Universe(LiveUniverseSubscriptionConfig),
}

impl LiveDataSubscriptionConfig {
    pub fn key(&self) -> LiveSubscriptionKey {
        match self {
            Self::Market(config) => LiveSubscriptionKey::Market(config.unique_id()),
            Self::Custom(subscription) => LiveSubscriptionKey::Custom {
                source_type: subscription.source_type.clone(),
                ticker: subscription.ticker.clone(),
            },
            Self::Universe(subscription) => LiveSubscriptionKey::Universe {
                source_type: subscription.source_type.clone(),
                ticker: subscription.ticker.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum LiveSubscriptionKey {
    Market(u64),
    Custom { source_type: String, ticker: String },
    Universe { source_type: String, ticker: String },
}

/// Live universe data subscription.
///
/// This mirrors the C# LEAN distinction between a universe subscription and
/// the ordinary market subscriptions generated after selection. Providers emit
/// `LiveDataItem::UniverseData` for this config; the engine consumes it before
/// building user `Slice` objects.
#[derive(Debug, Clone)]
pub struct LiveUniverseSubscriptionConfig {
    pub source_type: String,
    pub ticker: String,
    pub resolution: Resolution,
    pub properties: HashMap<String, String>,
}

/// One provider-owned live subscription stream.
///
/// This is the Rust equivalent of the C# `IDataQueueHandler.Subscribe(...)`
/// enumerator: the handler owns the streaming work and returns a receiver for
/// engine consumption.
pub struct LiveDataSubscription {
    pub config: LiveDataSubscriptionConfig,
    pub receiver: Receiver<Result<LiveDataItem>>,
}

impl LiveDataSubscription {
    pub fn new(
        config: LiveDataSubscriptionConfig,
        receiver: Receiver<Result<LiveDataItem>>,
    ) -> Self {
        Self { config, receiver }
    }

    pub fn key(&self) -> LiveSubscriptionKey {
        self.config.key()
    }
}

#[derive(Debug, Clone)]
pub enum LiveDataItem {
    TradeBar(TradeBar),
    QuoteBar(QuoteBar),
    Tick(Tick),
    MarginInterestRate(MarginInterestRate),
    PerpetualContext(PerpetualContext),
    OrderBook(OrderBook),
    CustomData {
        source_type: String,
        ticker: String,
        point: CustomDataPoint,
    },
    UniverseData {
        source_type: String,
        ticker: String,
        resolution: Resolution,
        time: DateTime,
        data: Vec<CustomDataPoint>,
    },
    Heartbeat(DateTime),
}

impl LiveDataItem {
    pub fn time(&self) -> DateTime {
        match self {
            Self::TradeBar(bar) => bar.time,
            Self::QuoteBar(bar) => bar.time,
            Self::Tick(tick) => tick.time,
            Self::MarginInterestRate(rate) => rate.time,
            Self::PerpetualContext(context) => context.time,
            Self::OrderBook(book) => book.time,
            Self::CustomData { point, .. } => point.end_time.unwrap_or_else(|| {
                point
                    .time
                    .and_hms_opt(0, 0, 0)
                    .expect("midnight is valid")
                    .into()
            }),
            Self::UniverseData { time, .. } => *time,
            Self::Heartbeat(time) => *time,
        }
    }

    pub fn end_time(&self) -> DateTime {
        match self {
            Self::TradeBar(bar) => bar.end_time,
            Self::QuoteBar(bar) => bar.end_time,
            Self::Tick(tick) => tick.time,
            Self::MarginInterestRate(rate) => rate.time,
            Self::PerpetualContext(context) => context.end_time,
            Self::OrderBook(book) => book.time,
            Self::CustomData { point, .. } => point.end_time.unwrap_or_else(|| {
                point
                    .time
                    .and_hms_opt(0, 0, 0)
                    .expect("midnight is valid")
                    .into()
            }),
            Self::UniverseData { time, .. } => *time,
            Self::Heartbeat(time) => *time,
        }
    }

    pub fn symbol(&self) -> Option<&Symbol> {
        match self {
            Self::TradeBar(bar) => Some(&bar.symbol),
            Self::QuoteBar(bar) => Some(&bar.symbol),
            Self::Tick(tick) => Some(&tick.symbol),
            Self::MarginInterestRate(rate) => Some(&rate.symbol),
            Self::PerpetualContext(context) => Some(&context.symbol),
            Self::OrderBook(book) => Some(&book.symbol),
            Self::CustomData { .. } | Self::UniverseData { .. } | Self::Heartbeat(_) => None,
        }
    }

    pub fn add_to_slice(self, slice: &mut Slice) {
        match self {
            Self::TradeBar(bar) => slice.add_bar(bar),
            Self::QuoteBar(bar) => slice.add_quote_bar(bar),
            Self::Tick(tick) => slice.add_tick(tick),
            Self::MarginInterestRate(rate) => slice.add_margin_interest_rate(rate),
            Self::PerpetualContext(context) => slice.add_perpetual_context(context),
            Self::OrderBook(book) => slice.add_order_book(book),
            Self::CustomData {
                source_type,
                ticker,
                point,
            } => slice.add_custom_data(source_type, ticker, point),
            Self::UniverseData { .. } => {}
            Self::Heartbeat(_) => {}
        }
    }
}

pub fn live_data_channel() -> (Sender<Result<LiveDataItem>>, Receiver<Result<LiveDataItem>>) {
    bounded(100_000)
}

/// Trait for live data queue handlers, matching LEAN's `IDataQueueHandler`.
///
/// Handlers should return one receiver per accepted subscription. Returning
/// `LeanError::Unsupported` lets `DataQueueHandlerManager` try the next handler
/// in a stacked deployment.
pub trait DataQueueHandler: Send + Sync {
    fn set_job(&mut self, _job: &LiveNodePacket) -> Result<()> {
        Ok(())
    }

    fn subscribe(&mut self, config: &SubscriptionDataConfig) -> Result<LiveDataSubscription>;

    fn subscribe_custom(
        &mut self,
        subscription: &CustomDataSubscription,
    ) -> Result<LiveDataSubscription> {
        Err(LeanError::Unsupported(format!(
            "{} does not support custom live subscriptions for {}:{}",
            self.name(),
            subscription.source_type,
            subscription.ticker
        )))
    }

    fn subscribe_universe(
        &mut self,
        subscription: &LiveUniverseSubscriptionConfig,
    ) -> Result<LiveDataSubscription> {
        Err(LeanError::Unsupported(format!(
            "{} does not support live universe subscriptions for {}:{}",
            self.name(),
            subscription.source_type,
            subscription.ticker
        )))
    }

    fn unsubscribe(&mut self, config: &SubscriptionDataConfig) -> Result<()>;

    fn unsubscribe_custom(&mut self, _subscription: &CustomDataSubscription) -> Result<()> {
        Ok(())
    }

    fn unsubscribe_universe(
        &mut self,
        _subscription: &LiveUniverseSubscriptionConfig,
    ) -> Result<()> {
        Ok(())
    }

    fn is_connected(&self) -> bool;

    fn name(&self) -> &str {
        "DataQueueHandler"
    }
}
