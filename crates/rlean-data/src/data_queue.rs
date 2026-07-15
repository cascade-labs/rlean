use crate::{FundamentalData, OrderBook, Slice, SubscriptionDataConfig};
use crossbeam_channel::{bounded, Receiver, Sender};
use rlean_core::{DateTime, Resolution, Result, Symbol};
use rlean_data_tables::{CustomDataPoint, MarginInterestRate, QuoteBar, Tick, TradeBar};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub enum LiveDataSubscriptionConfig {
    Market(Box<SubscriptionDataConfig>),
    Universe(LiveUniverseSubscriptionConfig),
}

impl LiveDataSubscriptionConfig {
    pub fn key(&self) -> LiveSubscriptionKey {
        match self {
            Self::Market(config) => LiveSubscriptionKey::Market(config.unique_id()),
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

/// One sidecar-backed live subscription stream consumed by the engine.
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
    OrderBook(OrderBook),
    CustomData {
        symbol: Symbol,
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
    FundamentalUniverseData {
        /// Availability frontier of the whole point-in-time snapshot.
        time: DateTime,
        data: Vec<FundamentalData>,
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
            Self::OrderBook(book) => book.time,
            Self::CustomData { point, .. } => point.time,
            Self::UniverseData { time, .. } => *time,
            Self::FundamentalUniverseData { time, .. } => *time,
            Self::Heartbeat(time) => *time,
        }
    }

    pub fn end_time(&self) -> DateTime {
        match self {
            Self::TradeBar(bar) => bar.end_time,
            Self::QuoteBar(bar) => bar.end_time,
            Self::Tick(tick) => tick.time,
            Self::MarginInterestRate(rate) => rate.time,
            Self::OrderBook(book) => book.time,
            Self::CustomData { point, .. } => point.end_time,
            Self::UniverseData { time, .. } => *time,
            Self::FundamentalUniverseData { time, .. } => *time,
            Self::Heartbeat(time) => *time,
        }
    }

    pub fn symbol(&self) -> Option<&Symbol> {
        match self {
            Self::TradeBar(bar) => Some(&bar.symbol),
            Self::QuoteBar(bar) => Some(&bar.symbol),
            Self::Tick(tick) => Some(&tick.symbol),
            Self::MarginInterestRate(rate) => Some(&rate.symbol),
            Self::OrderBook(book) => Some(&book.symbol),
            Self::CustomData { symbol, .. } => Some(symbol),
            Self::UniverseData { .. }
            | Self::FundamentalUniverseData { .. }
            | Self::Heartbeat(_) => None,
        }
    }

    pub fn add_to_slice(self, slice: &mut Slice) {
        match self {
            Self::TradeBar(bar) => slice.add_bar(bar),
            Self::QuoteBar(bar) => slice.add_quote_bar(bar),
            Self::Tick(tick) => slice.add_tick(tick),
            Self::MarginInterestRate(rate) => slice.add_margin_interest_rate(rate),
            Self::OrderBook(book) => slice.add_order_book(book),
            Self::CustomData {
                symbol,
                source_type,
                ticker,
                point,
            } => {
                let _ = source_type;
                slice.add_custom_data_for_symbol(symbol, ticker, point)
            }
            Self::UniverseData { .. } => {}
            Self::FundamentalUniverseData { data, .. } => slice.add_fundamentals(data),
            Self::Heartbeat(_) => {}
        }
    }
}

pub fn live_data_channel() -> (Sender<Result<LiveDataItem>>, Receiver<Result<LiveDataItem>>) {
    bounded(100_000)
}
