use std::collections::HashMap;

use arrow_flight::FlightData;
use prost::{Enumeration, Message, Oneof};
use rlean_core::{Resolution, TickType};
use rlean_data::{CustomDataQuery, SubscriptionDataConfig, SubscriptionDataKind};
use rlean_data_tables::BaseDataType;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum DeliveryMode {
    Backtest = 0,
    Live = 1,
    /// Daemon-to-daemon stream of every newly ingested canonical batch.
    Relay = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Enumeration)]
#[repr(i32)]
pub enum WireDataType {
    TradeBar = 0,
    QuoteBar = 1,
    Tick = 2,
    OpenInterest = 3,
    MarginInterestRate = 4,
    Custom = 5,
    OptionUniverse = 6,
    FutureUniverse = 7,
    FundamentalUniverse = 8,
    EtfConstituent = 9,
    FactorFile = 10,
    MapFile = 11,
    /// Provider-generated universe rows. This is distinct from ordinary
    /// custom data even when both use the same canonical point schema.
    Universe = 12,
}

#[derive(Clone, PartialEq, Message)]
pub struct StringList {
    #[prost(string, repeated, tag = "1")]
    pub values: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct StringListEntry {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<StringList>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CustomQuery {
    #[prost(string, repeated, tag = "1")]
    pub symbols: Vec<String>,
    #[prost(string, repeated, tag = "2")]
    pub columns: Vec<String>,
    #[prost(sint32, optional, tag = "3")]
    pub start_day: Option<i32>,
    #[prost(sint32, optional, tag = "4")]
    pub end_day: Option<i32>,
    #[prost(sint64, optional, tag = "5")]
    pub start_time_ns: Option<i64>,
    #[prost(sint64, optional, tag = "6")]
    pub end_time_ns: Option<i64>,
    #[prost(map = "string, string", tag = "7")]
    pub string_equals: HashMap<String, String>,
    #[prost(message, repeated, tag = "8")]
    pub string_in: Vec<StringListEntry>,
    #[prost(map = "string, double", tag = "9")]
    pub numeric_min: HashMap<String, f64>,
    #[prost(map = "string, double", tag = "10")]
    pub numeric_max: HashMap<String, f64>,
    #[prost(map = "string, string", tag = "11")]
    pub properties: HashMap<String, String>,
}

/// Provider-neutral subscription definition. Time ranges deliberately do not
/// belong here: a subscription is registered once, then queried repeatedly in
/// backtests or pushed continuously in live mode.
#[derive(Clone, PartialEq, Message)]
pub struct SubscriptionSpec {
    #[prost(uint64, tag = "1")]
    pub config_id: u64,
    #[prost(uint64, tag = "2")]
    pub symbol_sid: u64,
    #[prost(string, tag = "3")]
    pub symbol_value: String,
    #[prost(string, tag = "4")]
    pub permanent_ticker: String,
    #[prost(int32, tag = "5")]
    pub security_type: i32,
    #[prost(string, tag = "6")]
    pub market: String,
    #[prost(int32, tag = "7")]
    pub resolution: i32,
    #[prost(int32, tag = "8")]
    pub tick_type: i32,
    #[prost(enumeration = "WireDataType", tag = "9")]
    pub data_type: i32,
    #[prost(bool, tag = "10")]
    pub extended_market_hours: bool,
    #[prost(string, tag = "11")]
    pub source_type: String,
    #[prost(string, tag = "12")]
    pub ticker: String,
    #[prost(message, optional, tag = "13")]
    pub custom_query: Option<CustomQuery>,
    #[prost(map = "string, string", tag = "14")]
    pub properties: HashMap<String, String>,
    /// Physical execution/data venue. This is intentionally separate from the
    /// LEAN market used to construct the symbol SID.
    #[prost(string, tag = "15")]
    pub venue: String,
}

impl From<&SubscriptionDataConfig> for SubscriptionSpec {
    fn from(config: &SubscriptionDataConfig) -> Self {
        let custom = config.custom.as_ref();
        let fundamental = config.fundamental_universe.as_ref();
        Self {
            config_id: config.unique_id(),
            symbol_sid: config.symbol.id.sid,
            symbol_value: config.symbol.value.to_string(),
            permanent_ticker: config.symbol.permtick.to_string(),
            security_type: config.symbol.security_type() as i32,
            market: config.symbol.market().as_str().to_string(),
            resolution: resolution_code(config.resolution),
            tick_type: tick_type_code(config.tick_type),
            data_type: subscription_data_type(config) as i32,
            extended_market_hours: config.extended_market_hours,
            source_type: custom
                .map(|value| value.source_type.clone())
                .or_else(|| fundamental.map(|value| value.source_type.clone()))
                .unwrap_or_default(),
            // The sidecar treats this as an all-equity snapshot, not a ticker
            // request. Keeping it explicit makes the protocol easy to audit.
            ticker: custom
                .map(|value| value.ticker.clone())
                .or_else(|| fundamental.map(|_| "*".to_string()))
                .unwrap_or_default(),
            custom_query: custom.map(|value| CustomQuery::from(&value.dynamic_query)),
            properties: custom
                .map(|value| value.config.properties.clone())
                .unwrap_or_default(),
            venue: config.venue.clone(),
        }
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Initialize {
    #[prost(string, tag = "1")]
    pub origin_id: String,
    #[prost(string, tag = "2")]
    pub session_id: String,
}

/// Requests the sidecar-owned data manifest. The manifest body is opaque to
/// rlean and is passed through unchanged by the CLI.
#[derive(Clone, PartialEq, Message)]
pub struct GetManifest {}

#[derive(Clone, PartialEq, Message)]
pub struct AddSubscription {
    #[prost(uint64, tag = "1")]
    pub subscription_id: u64,
    #[prost(enumeration = "DeliveryMode", tag = "2")]
    pub mode: i32,
    #[prost(message, optional, tag = "3")]
    pub subscription: Option<SubscriptionSpec>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RemoveSubscription {
    #[prost(uint64, tag = "1")]
    pub subscription_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct BacktestQuery {
    #[prost(uint64, tag = "1")]
    pub subscription_id: u64,
    #[prost(sint64, tag = "2")]
    pub start_time_ns: i64,
    #[prost(sint64, tag = "3")]
    pub end_time_ns: i64,
}

/// Opens the live market-data feed used by strategy-created subscriptions.
/// The opaque configuration is provider-specific and is treated as secret by
/// both peers. Symbols and resolutions never belong in this message.
#[derive(Clone, PartialEq, Message)]
pub struct OpenLiveDataFeed {
    #[prost(uint64, tag = "1")]
    pub feed_connection_id: u64,
    #[prost(string, tag = "2")]
    pub provider: String,
    #[prost(bytes = "vec", tag = "3")]
    pub opaque_config_json: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CloseLiveDataFeed {
    #[prost(uint64, tag = "1")]
    pub feed_connection_id: u64,
}

/// Opens an execution/account connection. This is deliberately independent
/// from the live-data feed: a run may consume Tradier data and execute through
/// Robinhood, or use a live feed while keeping paper execution inside rlean.
#[derive(Clone, PartialEq, Message)]
pub struct OpenBrokerage {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
    #[prost(string, tag = "2")]
    pub brokerage: String,
    #[prost(bytes = "vec", tag = "3")]
    pub opaque_config_json: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CloseBrokerage {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetBrokerageSnapshot {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireSymbol {
    #[prost(uint64, tag = "1")]
    pub sid: u64,
    #[prost(string, tag = "2")]
    pub value: String,
    #[prost(string, tag = "3")]
    pub permanent_ticker: String,
    #[prost(string, tag = "4")]
    pub identifier_ticker: String,
    #[prost(int32, tag = "5")]
    pub security_type: i32,
    #[prost(string, tag = "6")]
    pub market: String,
    #[prost(sint32, optional, tag = "7")]
    pub expiry_day: Option<i32>,
    #[prost(string, optional, tag = "8")]
    pub strike: Option<String>,
    #[prost(int32, optional, tag = "9")]
    pub option_right: Option<i32>,
    #[prost(int32, optional, tag = "10")]
    pub option_style: Option<i32>,
    #[prost(message, optional, boxed, tag = "11")]
    pub underlying: Option<Box<WireSymbol>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireOrder {
    #[prost(sint64, tag = "1")]
    pub engine_order_id: i64,
    #[prost(sint64, tag = "2")]
    pub contingent_id: i64,
    #[prost(string, repeated, tag = "3")]
    pub brokerage_order_ids: Vec<String>,
    #[prost(message, optional, tag = "4")]
    pub symbol: Option<WireSymbol>,
    #[prost(int32, tag = "5")]
    pub order_type: i32,
    #[prost(int32, tag = "6")]
    pub status: i32,
    #[prost(string, tag = "7")]
    pub quantity: String,
    #[prost(string, tag = "8")]
    pub filled_quantity: String,
    #[prost(string, tag = "9")]
    pub average_fill_price: String,
    #[prost(string, optional, tag = "10")]
    pub limit_price: Option<String>,
    #[prost(string, optional, tag = "11")]
    pub stop_price: Option<String>,
    #[prost(string, optional, tag = "12")]
    pub trailing_amount: Option<String>,
    #[prost(bool, tag = "13")]
    pub trailing_as_percentage: bool,
    #[prost(sint64, tag = "14")]
    pub time_ns: i64,
    #[prost(sint64, tag = "15")]
    pub created_time_ns: i64,
    #[prost(int32, tag = "16")]
    pub time_in_force: i32,
    #[prost(sint64, optional, tag = "17")]
    pub good_til_time_ns: Option<i64>,
    #[prost(string, tag = "18")]
    pub price_currency: String,
    #[prost(string, tag = "19")]
    pub tag: String,
    #[prost(string, optional, tag = "20")]
    pub exchange: Option<String>,
    #[prost(bool, tag = "21")]
    pub outside_regular_trading_hours: bool,
    #[prost(bool, tag = "22")]
    pub post_only: bool,
    #[prost(string, tag = "23")]
    pub price: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubmitOrder {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
    #[prost(string, tag = "2")]
    pub command_id: String,
    #[prost(message, optional, tag = "3")]
    pub order: Option<WireOrder>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpdateOrder {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
    #[prost(string, tag = "2")]
    pub command_id: String,
    #[prost(message, optional, tag = "3")]
    pub order: Option<WireOrder>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CancelOrder {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
    #[prost(string, tag = "2")]
    pub command_id: String,
    #[prost(sint64, tag = "3")]
    pub engine_order_id: i64,
    #[prost(string, repeated, tag = "4")]
    pub brokerage_order_ids: Vec<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientMessage {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(
        oneof = "client_message::Payload",
        tags = "10, 11, 12, 13, 14, 20, 21, 22, 23, 24, 25, 26, 27"
    )]
    pub payload: Option<client_message::Payload>,
}

pub mod client_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Payload {
        #[prost(message, tag = "10")]
        Initialize(Initialize),
        #[prost(message, tag = "11")]
        AddSubscription(AddSubscription),
        #[prost(message, tag = "12")]
        RemoveSubscription(RemoveSubscription),
        #[prost(message, tag = "13")]
        BacktestQuery(BacktestQuery),
        #[prost(message, tag = "14")]
        GetManifest(GetManifest),
        #[prost(message, tag = "20")]
        OpenLiveDataFeed(OpenLiveDataFeed),
        #[prost(message, tag = "21")]
        CloseLiveDataFeed(CloseLiveDataFeed),
        #[prost(message, tag = "22")]
        OpenBrokerage(OpenBrokerage),
        #[prost(message, tag = "23")]
        CloseBrokerage(CloseBrokerage),
        #[prost(message, tag = "24")]
        GetBrokerageSnapshot(GetBrokerageSnapshot),
        #[prost(message, tag = "25")]
        SubmitOrder(SubmitOrder),
        #[prost(message, tag = "26")]
        UpdateOrder(UpdateOrder),
        #[prost(message, tag = "27")]
        CancelOrder(CancelOrder),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct Initialized {
    #[prost(string, tag = "1")]
    pub origin_id: String,
    #[prost(string, tag = "2")]
    pub session_id: String,
}

/// Sidecar-owned manifest document. rlean deliberately does not model its
/// contents; the content type and body are exposed verbatim to callers.
#[derive(Clone, PartialEq, Message)]
pub struct Manifest {
    #[prost(string, tag = "1")]
    pub content_type: String,
    #[prost(bytes = "vec", tag = "2")]
    pub body: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscriptionAdded {
    #[prost(uint64, tag = "1")]
    pub subscription_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct SubscriptionRemoved {
    #[prost(uint64, tag = "1")]
    pub subscription_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct DataBatch {
    #[prost(uint64, tag = "1")]
    pub subscription_id: u64,
    /// Zero denotes unsolicited live data. Backtest data carries the query's
    /// request id so concurrent range queries can be routed independently.
    #[prost(uint64, tag = "2")]
    pub query_id: u64,
    #[prost(enumeration = "WireDataType", tag = "3")]
    pub data_type: i32,
    /// Canonical identity of an ingested batch. Present on daemon relay
    /// traffic so followers can route it through their local subscriptions.
    #[prost(message, optional, tag = "4")]
    pub subscription: Option<SubscriptionSpec>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BacktestQueryComplete {
    #[prost(uint64, tag = "1")]
    pub subscription_id: u64,
    #[prost(uint64, tag = "2")]
    pub query_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProtocolError {
    #[prost(string, tag = "1")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct LiveDataFeedOpened {
    #[prost(uint64, tag = "1")]
    pub feed_connection_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct LiveDataFeedClosed {
    #[prost(uint64, tag = "1")]
    pub feed_connection_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct BrokerageOpened {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
    #[prost(string, tag = "2")]
    pub brokerage: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct BrokerageClosed {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct BrokerageCommandResult {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
    #[prost(string, tag = "2")]
    pub command_id: String,
    #[prost(bool, tag = "3")]
    pub accepted: bool,
    #[prost(bool, tag = "4")]
    pub retryable: bool,
    #[prost(string, repeated, tag = "5")]
    pub brokerage_order_ids: Vec<String>,
    #[prost(string, tag = "6")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireCashBalance {
    #[prost(string, tag = "1")]
    pub currency: String,
    #[prost(string, tag = "2")]
    pub amount: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct WireBrokerageHolding {
    #[prost(message, optional, tag = "1")]
    pub symbol: Option<WireSymbol>,
    #[prost(string, tag = "2")]
    pub quantity: String,
    #[prost(string, tag = "3")]
    pub average_price: String,
    #[prost(string, tag = "4")]
    pub market_price: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct BrokerageSnapshot {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
    #[prost(uint64, tag = "2")]
    pub sequence: u64,
    #[prost(message, repeated, tag = "3")]
    pub cash: Vec<WireCashBalance>,
    #[prost(message, repeated, tag = "4")]
    pub holdings: Vec<WireBrokerageHolding>,
    #[prost(message, repeated, tag = "5")]
    pub open_orders: Vec<WireOrder>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BrokerageOrderUpdate {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
    #[prost(uint64, tag = "2")]
    pub sequence: u64,
    #[prost(sint64, tag = "3")]
    pub engine_order_id: i64,
    #[prost(string, repeated, tag = "4")]
    pub brokerage_order_ids: Vec<String>,
    #[prost(message, optional, tag = "5")]
    pub symbol: Option<WireSymbol>,
    #[prost(int32, tag = "6")]
    pub status: i32,
    #[prost(string, tag = "7")]
    pub cumulative_filled_quantity: String,
    #[prost(string, tag = "8")]
    pub average_fill_price: String,
    #[prost(string, optional, tag = "9")]
    pub cumulative_fee: Option<String>,
    #[prost(string, tag = "10")]
    pub fee_currency: String,
    #[prost(sint64, tag = "11")]
    pub event_time_ns: i64,
    #[prost(string, tag = "12")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct BrokerageConnectionState {
    #[prost(uint64, tag = "1")]
    pub brokerage_connection_id: u64,
    #[prost(uint64, tag = "2")]
    pub sequence: u64,
    #[prost(bool, tag = "3")]
    pub connected: bool,
    #[prost(string, tag = "4")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServerMessage {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(uint64, tag = "2")]
    pub request_id: u64,
    #[prost(
        oneof = "server_message::Payload",
        tags = "10, 11, 12, 13, 14, 15, 16, 20, 21, 22, 23, 24, 25, 26, 27"
    )]
    pub payload: Option<server_message::Payload>,
}

pub mod server_message {
    use super::*;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Payload {
        #[prost(message, tag = "10")]
        Initialized(Initialized),
        #[prost(message, tag = "11")]
        SubscriptionAdded(SubscriptionAdded),
        #[prost(message, tag = "12")]
        SubscriptionRemoved(SubscriptionRemoved),
        #[prost(message, boxed, tag = "13")]
        DataBatch(Box<DataBatch>),
        #[prost(message, tag = "14")]
        BacktestQueryComplete(BacktestQueryComplete),
        #[prost(message, tag = "15")]
        Error(ProtocolError),
        #[prost(message, tag = "16")]
        Manifest(Manifest),
        #[prost(message, tag = "20")]
        LiveDataFeedOpened(LiveDataFeedOpened),
        #[prost(message, tag = "21")]
        LiveDataFeedClosed(LiveDataFeedClosed),
        #[prost(message, tag = "22")]
        BrokerageOpened(BrokerageOpened),
        #[prost(message, tag = "23")]
        BrokerageClosed(BrokerageClosed),
        #[prost(message, tag = "24")]
        BrokerageCommandResult(BrokerageCommandResult),
        #[prost(message, tag = "25")]
        BrokerageSnapshot(BrokerageSnapshot),
        #[prost(message, boxed, tag = "26")]
        BrokerageOrderUpdate(Box<BrokerageOrderUpdate>),
        #[prost(message, tag = "27")]
        BrokerageConnectionState(BrokerageConnectionState),
    }
}

impl ClientMessage {
    pub fn into_flight_data(self) -> FlightData {
        FlightData::new().with_app_metadata(self.encode_to_vec())
    }

    pub fn decode_flight_data(data: &FlightData) -> Result<Self, prost::DecodeError> {
        Self::decode(data.app_metadata.as_ref())
    }
}

impl ServerMessage {
    pub fn into_flight_data(self) -> FlightData {
        FlightData::new().with_app_metadata(self.encode_to_vec())
    }

    pub fn decode_flight_data(data: &FlightData) -> Result<Self, prost::DecodeError> {
        Self::decode(data.app_metadata.as_ref())
    }
}

impl From<&CustomDataQuery> for CustomQuery {
    fn from(query: &CustomDataQuery) -> Self {
        Self {
            symbols: query.symbols.clone().unwrap_or_default(),
            columns: query.columns.clone().unwrap_or_default(),
            start_day: query.start_date.map(days_since_epoch),
            end_day: query.end_date.map(days_since_epoch),
            start_time_ns: query.start_time.map(|time| time.0),
            end_time_ns: query.end_time.map(|time| time.0),
            string_equals: query.string_equals.clone(),
            string_in: query
                .string_in
                .iter()
                .map(|(key, values)| StringListEntry {
                    key: key.clone(),
                    value: Some(StringList {
                        values: values.clone(),
                    }),
                })
                .collect(),
            numeric_min: query.numeric_min.clone(),
            numeric_max: query.numeric_max.clone(),
            properties: query.properties.clone(),
        }
    }
}

fn days_since_epoch(date: chrono::NaiveDate) -> i32 {
    date.signed_duration_since(chrono::NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
        .num_days() as i32
}

fn resolution_code(value: Resolution) -> i32 {
    match value {
        Resolution::Tick => 0,
        Resolution::Second => 1,
        Resolution::Minute => 2,
        Resolution::Hour => 3,
        Resolution::Daily => 4,
    }
}

fn tick_type_code(value: TickType) -> i32 {
    match value {
        TickType::Trade => 0,
        TickType::Quote => 1,
        TickType::OpenInterest => 2,
    }
}

fn subscription_data_type(config: &SubscriptionDataConfig) -> WireDataType {
    if config.data_kind == SubscriptionDataKind::Custom {
        return WireDataType::Custom;
    }
    if config.data_kind == SubscriptionDataKind::Universe {
        return WireDataType::Universe;
    }
    if config.data_kind == SubscriptionDataKind::FundamentalUniverse {
        return WireDataType::FundamentalUniverse;
    }
    if config.data_kind == SubscriptionDataKind::Option {
        return WireDataType::OptionUniverse;
    }
    match if config.resolution.is_tick() {
        if config.tick_type == TickType::OpenInterest {
            BaseDataType::OpenInterest
        } else {
            BaseDataType::Tick
        }
    } else if config.tick_type == TickType::Quote {
        BaseDataType::QuoteBar
    } else {
        BaseDataType::TradeBar
    } {
        BaseDataType::TradeBar => WireDataType::TradeBar,
        BaseDataType::QuoteBar => WireDataType::QuoteBar,
        BaseDataType::Tick => WireDataType::Tick,
        BaseDataType::OpenInterest => WireDataType::OpenInterest,
        BaseDataType::MarginInterestRate => WireDataType::MarginInterestRate,
        BaseDataType::Custom => WireDataType::Custom,
        _ => WireDataType::Custom,
    }
}
