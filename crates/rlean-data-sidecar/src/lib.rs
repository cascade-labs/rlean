//! Apache Arrow Flight data plane for rlean.
//!
//! The engine sends provider-neutral subscription requests and receives
//! canonical Arrow record batches. Acquisition, caching, persistence, and
//! vendor integration belong entirely to the sidecar.

mod brokerage_wire;
mod client;
mod decode;
mod ipc;
mod protocol;

pub use brokerage_wire::{order_status_from_wire, symbol_from_wire};
pub use client::{
    BrokerageEvent, BrokerageEventStream, DataBatchStream, DataSidecarClient, DataSidecarConfig,
    RelayBatch, RelayBatchStream, SidecarEndpoint,
};
pub use decode::{
    decode_batch, decode_factor_file_batch, decode_map_file_batch, CanonicalDataBatch,
};
pub use ipc::{
    decode_record_batch, encode_record_batch, encode_record_batch_chunks,
    MAX_RECORD_BATCH_BODY_BYTES,
};
pub use protocol::{
    client_message, server_message, AddSubscription, BacktestQuery, BacktestQueryComplete,
    BrokerageClosed, BrokerageCommandResult, BrokerageConnectionState, BrokerageOpened,
    BrokerageOrderUpdate, BrokerageSnapshot, CancelOrder, ClientMessage, CloseBrokerage,
    CloseLiveDataFeed, CustomQuery, DataBatch, DeliveryMode, GetBrokerageSnapshot, GetManifest,
    Initialize, Initialized, LiveDataFeedClosed, LiveDataFeedOpened, Manifest, OpenBrokerage,
    OpenLiveDataFeed, ProtocolError, RemoveSubscription, ServerMessage, StringList,
    StringListEntry, SubmitOrder, SubscriptionAdded, SubscriptionRemoved, SubscriptionSpec,
    UpdateOrder, WireBrokerageHolding, WireCashBalance, WireDataType, WireOrder, WireSymbol,
    PROTOCOL_VERSION,
};
