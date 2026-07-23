use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use arrow_array::RecordBatch;
use arrow_flight::flight_service_client::FlightServiceClient;
use arrow_flight::FlightData;
use hyper_util::rt::TokioIo;
use rlean_core::DateTime;
use rlean_data::SubscriptionDataConfig;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::Request;
use tower::service_fn;
use uuid::Uuid;

use crate::client_message::Payload as ClientPayload;
use crate::server_message::Payload as ServerPayload;
use crate::{
    decode_record_batch, AddSubscription, BacktestQuery, BrokerageCommandResult,
    BrokerageConnectionState, BrokerageOrderUpdate, BrokerageSnapshot, CancelOrder, ClientMessage,
    CloseBrokerage, CloseLiveDataFeed, DeliveryMode, GetBrokerageSnapshot, GetManifest, Initialize,
    OpenBrokerage, OpenLiveDataFeed, RemoveSubscription, ServerMessage, SubmitOrder,
    SubscriptionSpec, UpdateOrder, WireOrder, PROTOCOL_VERSION,
};

pub type DataBatchStream = ReceiverStream<anyhow::Result<RecordBatch>>;
pub type BrokerageEventStream = ReceiverStream<anyhow::Result<BrokerageEvent>>;
pub type RelayBatchStream = ReceiverStream<anyhow::Result<RelayBatch>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriptionRegistration {
    pub subscription_id: u64,
    pub available_from: Option<DateTime>,
}

#[derive(Debug)]
pub struct RelayBatch {
    pub subscription: SubscriptionSpec,
    pub data_type: crate::WireDataType,
    pub batch: RecordBatch,
}

#[derive(Debug, Clone)]
pub enum BrokerageEvent {
    Order(Box<BrokerageOrderUpdate>),
    Connection(BrokerageConnectionState),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SidecarEndpoint {
    Tcp {
        authority: String,
        tls: bool,
    },
    #[cfg(unix)]
    Unix(PathBuf),
}

impl SidecarEndpoint {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        if let Some(authority) = value.strip_prefix("grpc+tls://") {
            validate_authority(authority)?;
            return Ok(Self::Tcp {
                authority: authority.to_string(),
                tls: true,
            });
        }
        if let Some(authority) = value.strip_prefix("grpc://") {
            validate_authority(authority)?;
            if !is_loopback_authority(authority) {
                bail!(
                    "plaintext Flight is limited to loopback; use grpc+tls:// for remote sidecars"
                );
            }
            return Ok(Self::Tcp {
                authority: authority.to_string(),
                tls: false,
            });
        }
        #[cfg(unix)]
        if let Some(path) = value.strip_prefix("grpc+unix://") {
            if path.is_empty() {
                bail!("Flight Unix endpoint is missing a socket path");
            }
            return Ok(Self::Unix(PathBuf::from(path)));
        }
        bail!(
            "unsupported Flight endpoint '{value}'; use grpc://host:port, \
             grpc+tls://host:port, or grpc+unix:///path"
        )
    }
}

fn is_loopback_authority(authority: &str) -> bool {
    let host = authority
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(authority)
        .trim_matches(['[', ']']);
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false)
}

fn validate_authority(authority: &str) -> anyhow::Result<()> {
    if authority.is_empty() || !authority.contains(':') {
        bail!("Flight endpoint must include host:port");
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataSidecarConfig {
    pub endpoint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
}

const fn default_connect_timeout_ms() -> u64 {
    10_000
}

struct SessionInner {
    outbound: mpsc::Sender<FlightData>,
    controls: Mutex<HashMap<u64, oneshot::Sender<anyhow::Result<ServerMessage>>>>,
    queries: Mutex<HashMap<u64, mpsc::Sender<anyhow::Result<RecordBatch>>>>,
    live: Mutex<HashMap<u64, mpsc::Sender<anyhow::Result<RecordBatch>>>>,
    relay: Mutex<HashMap<u64, mpsc::Sender<anyhow::Result<RelayBatch>>>>,
    live_data_feed: Mutex<Option<u64>>,
    brokerages: Mutex<HashMap<u64, mpsc::Sender<anyhow::Result<BrokerageEvent>>>>,
    next_request_id: AtomicU64,
    next_subscription_id: AtomicU64,
    next_connection_id: AtomicU64,
    /// Monotonic session generation. A fresh session (initial connect or a
    /// reconnect after a sidecar restart) carries a higher epoch so concurrent
    /// reconnect requests can coalesce onto a single re-establish.
    epoch: u64,
}

/// A live-data feed remembered so that a reconnect can restore it on the fresh
/// session under the same connection id the deployment already holds.
#[derive(Clone)]
struct RememberedLiveFeed {
    provider: String,
    opaque_config_json: Vec<u8>,
    feed_connection_id: u64,
}

struct ClientShared {
    config: DataSidecarConfig,
    origin_id: String,
    session_id: String,
    /// The current Flight session. Swapped atomically on reconnect so every
    /// `Arc<DataSidecarClient>` holder transparently follows the new session.
    session: std::sync::Mutex<Arc<SessionInner>>,
    live_feed: std::sync::Mutex<Option<RememberedLiveFeed>>,
    /// Serializes reconnect attempts so a sidecar restart triggers exactly one
    /// re-establish even when several stream owners notice the drop at once.
    reconnect_lock: tokio::sync::Mutex<()>,
}

#[derive(Clone)]
pub struct DataSidecarClient {
    shared: Arc<ClientShared>,
}

impl DataSidecarClient {
    pub async fn connect(config: DataSidecarConfig) -> anyhow::Result<Self> {
        let origin_id = Uuid::new_v4().to_string();
        let session_id = Uuid::new_v4().to_string();
        let session = connect_session(
            &config,
            &origin_id,
            &transport_session_id(&session_id, 0),
            0,
        )
        .await?;
        Ok(Self {
            shared: Arc::new(ClientShared {
                config,
                origin_id,
                session_id,
                session: std::sync::Mutex::new(session),
                live_feed: std::sync::Mutex::new(None),
                reconnect_lock: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Snapshot the current Flight session. Cheap `Arc` clone; callers that need
    /// consistency across several operations hold the returned handle so a
    /// concurrent reconnect cannot swap the session out from under them.
    fn session(&self) -> Arc<SessionInner> {
        self.shared
            .session
            .lock()
            .expect("session lock poisoned")
            .clone()
    }

    /// Generation of the current session. A caller records this before a stream
    /// dies and passes it back in through `reconnect` so an already-completed
    /// reconnect is not repeated.
    pub fn session_epoch(&self) -> u64 {
        self.session().epoch
    }

    /// Re-establish the Flight session after the sidecar dropped it (restart,
    /// crash-loop, transient network fault). Coalesced by epoch: if another
    /// stream owner already reconnected since this session's epoch, the call is
    /// a no-op. On success the remembered live-data feed is reopened on the
    /// fresh session under its original connection id; live subscriptions and
    /// brokerage connections are re-registered by their owners.
    pub async fn reconnect(&self) -> anyhow::Result<()> {
        self.reconnect_failed_epoch(self.session_epoch()).await
    }

    /// Re-establish the Flight session that owned a failed stream.
    ///
    /// Stream owners must pass the epoch captured when their stream was opened.
    /// This is intentionally different from [`Self::reconnect`]: a second owner
    /// can observe its old stream fail after another owner has already advanced
    /// the shared session. In that case this method adopts the newer session
    /// instead of tearing it down again and attempting to initialize the same
    /// logical session twice.
    pub async fn reconnect_failed_epoch(&self, failed_epoch: u64) -> anyhow::Result<()> {
        let _guard = self.shared.reconnect_lock.lock().await;
        // Another owner may have re-established the session while we waited for
        // the reconnect lock; if so, adopt it rather than churning a new one.
        if self.session().epoch != failed_epoch {
            return Ok(());
        }
        let next_epoch = failed_epoch.wrapping_add(1);
        let session = connect_session(
            &self.shared.config,
            &self.shared.origin_id,
            &transport_session_id(&self.shared.session_id, next_epoch),
            next_epoch,
        )
        .await?;
        // Reopen the remembered live-data feed before publishing the session so
        // no subscriber ever observes a live session without its feed.
        let feed = self
            .shared
            .live_feed
            .lock()
            .expect("live-feed lock poisoned")
            .clone();
        if let Some(feed) = feed {
            reopen_live_data_feed(&session, &feed).await?;
        }
        *self.shared.session.lock().expect("session lock poisoned") = session;
        Ok(())
    }

    /// Returns the sidecar-owned manifest without interpreting its contents.
    pub async fn manifest(&self) -> anyhow::Result<crate::Manifest> {
        let response = self
            .send_and_wait(ClientPayload::GetManifest(GetManifest {}))
            .await?;
        match response.payload {
            Some(ServerPayload::Manifest(manifest)) => Ok(manifest),
            _ => bail!("sidecar returned an invalid manifest response"),
        }
    }

    pub fn origin_id(&self) -> &str {
        &self.shared.origin_id
    }

    pub fn session_id(&self) -> &str {
        &self.shared.session_id
    }

    /// Open the session's default live-data feed. Strategy subscriptions remain
    /// independent and are added later by the algorithm subscription manager.
    pub async fn open_live_data_feed(
        &self,
        provider: impl Into<String>,
        opaque_config_json: Vec<u8>,
    ) -> anyhow::Result<u64> {
        let inner = self.session();
        let provider = provider.into();
        let mut active_feed = inner.live_data_feed.lock().await;
        if active_feed.is_some() {
            bail!("this sidecar session already has an open live-data feed");
        }
        let feed_connection_id = inner.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let response = send_and_wait_on(
            &inner,
            ClientPayload::OpenLiveDataFeed(OpenLiveDataFeed {
                feed_connection_id,
                provider: provider.clone(),
                opaque_config_json: opaque_config_json.clone(),
            }),
        )
        .await?;
        match response.payload {
            Some(ServerPayload::LiveDataFeedOpened(opened))
                if opened.feed_connection_id == feed_connection_id =>
            {
                *active_feed = Some(feed_connection_id);
                // Remember the feed so a reconnect can restore it on the fresh
                // session under this same connection id.
                *self
                    .shared
                    .live_feed
                    .lock()
                    .expect("live-feed lock poisoned") = Some(RememberedLiveFeed {
                    provider,
                    opaque_config_json,
                    feed_connection_id,
                });
                Ok(feed_connection_id)
            }
            _ => bail!("sidecar returned an invalid OpenLiveDataFeed acknowledgement"),
        }
    }

    pub async fn close_live_data_feed(&self, feed_connection_id: u64) -> anyhow::Result<()> {
        let inner = self.session();
        let response = send_and_wait_on(
            &inner,
            ClientPayload::CloseLiveDataFeed(CloseLiveDataFeed { feed_connection_id }),
        )
        .await?;
        match response.payload {
            Some(ServerPayload::LiveDataFeedClosed(closed))
                if closed.feed_connection_id == feed_connection_id =>
            {
                let mut active_feed = inner.live_data_feed.lock().await;
                if *active_feed == Some(feed_connection_id) {
                    *active_feed = None;
                }
                // A deliberately closed feed must not be resurrected by a later
                // reconnect.
                let mut remembered = self
                    .shared
                    .live_feed
                    .lock()
                    .expect("live-feed lock poisoned");
                if remembered
                    .as_ref()
                    .is_some_and(|feed| feed.feed_connection_id == feed_connection_id)
                {
                    *remembered = None;
                }
                Ok(())
            }
            _ => bail!("sidecar returned an invalid CloseLiveDataFeed acknowledgement"),
        }
    }

    pub async fn open_brokerage(
        &self,
        brokerage: impl Into<String>,
        opaque_config_json: Vec<u8>,
    ) -> anyhow::Result<(u64, BrokerageEventStream)> {
        let inner = self.session();
        let brokerage_connection_id = inner.next_connection_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel(256);
        inner
            .brokerages
            .lock()
            .await
            .insert(brokerage_connection_id, sender);
        let result = send_and_wait_on(
            &inner,
            ClientPayload::OpenBrokerage(OpenBrokerage {
                brokerage_connection_id,
                brokerage: brokerage.into(),
                opaque_config_json,
            }),
        )
        .await;
        match result {
            Ok(response)
                if matches!(
                    response.payload,
                    Some(ServerPayload::BrokerageOpened(ref opened))
                        if opened.brokerage_connection_id == brokerage_connection_id
                ) =>
            {
                Ok((brokerage_connection_id, ReceiverStream::new(receiver)))
            }
            Ok(_) => {
                inner
                    .brokerages
                    .lock()
                    .await
                    .remove(&brokerage_connection_id);
                bail!("sidecar returned an invalid OpenBrokerage acknowledgement")
            }
            Err(error) => {
                inner
                    .brokerages
                    .lock()
                    .await
                    .remove(&brokerage_connection_id);
                Err(error)
            }
        }
    }

    pub async fn close_brokerage(&self, brokerage_connection_id: u64) -> anyhow::Result<()> {
        let inner = self.session();
        let response = send_and_wait_on(
            &inner,
            ClientPayload::CloseBrokerage(CloseBrokerage {
                brokerage_connection_id,
            }),
        )
        .await?;
        inner
            .brokerages
            .lock()
            .await
            .remove(&brokerage_connection_id);
        match response.payload {
            Some(ServerPayload::BrokerageClosed(closed))
                if closed.brokerage_connection_id == brokerage_connection_id =>
            {
                Ok(())
            }
            _ => bail!("sidecar returned an invalid CloseBrokerage acknowledgement"),
        }
    }

    pub async fn brokerage_snapshot(
        &self,
        brokerage_connection_id: u64,
    ) -> anyhow::Result<BrokerageSnapshot> {
        let response = self
            .send_and_wait(ClientPayload::GetBrokerageSnapshot(GetBrokerageSnapshot {
                brokerage_connection_id,
            }))
            .await?;
        match response.payload {
            Some(ServerPayload::BrokerageSnapshot(snapshot))
                if snapshot.brokerage_connection_id == brokerage_connection_id =>
            {
                Ok(snapshot)
            }
            _ => bail!("sidecar returned an invalid BrokerageSnapshot response"),
        }
    }

    pub async fn submit_order(
        &self,
        brokerage_connection_id: u64,
        command_id: String,
        order: WireOrder,
    ) -> anyhow::Result<BrokerageCommandResult> {
        self.order_command(ClientPayload::SubmitOrder(SubmitOrder {
            brokerage_connection_id,
            command_id,
            order: Some(order),
        }))
        .await
    }

    pub async fn update_order(
        &self,
        brokerage_connection_id: u64,
        command_id: String,
        order: WireOrder,
    ) -> anyhow::Result<BrokerageCommandResult> {
        self.order_command(ClientPayload::UpdateOrder(UpdateOrder {
            brokerage_connection_id,
            command_id,
            order: Some(order),
        }))
        .await
    }

    pub async fn cancel_order(
        &self,
        brokerage_connection_id: u64,
        command_id: String,
        engine_order_id: i64,
        brokerage_order_ids: Vec<String>,
    ) -> anyhow::Result<BrokerageCommandResult> {
        self.order_command(ClientPayload::CancelOrder(CancelOrder {
            brokerage_connection_id,
            command_id,
            engine_order_id,
            brokerage_order_ids,
        }))
        .await
    }

    async fn order_command(
        &self,
        payload: ClientPayload,
    ) -> anyhow::Result<BrokerageCommandResult> {
        let response = self.send_and_wait(payload).await?;
        match response.payload {
            Some(ServerPayload::BrokerageCommandResult(result)) => Ok(result),
            _ => bail!("sidecar returned an invalid brokerage command response"),
        }
    }

    pub async fn add_subscription(
        &self,
        config: &SubscriptionDataConfig,
        mode: DeliveryMode,
    ) -> anyhow::Result<SubscriptionRegistration> {
        self.add_subscription_spec(SubscriptionSpec::from(config), mode)
            .await
    }

    /// Register an auxiliary subscription whose wire type is not represented
    /// by the ordinary market-data `SubscriptionDataConfig` (for example a
    /// factor file or map file).
    pub async fn add_subscription_spec(
        &self,
        spec: SubscriptionSpec,
        mode: DeliveryMode,
    ) -> anyhow::Result<SubscriptionRegistration> {
        let inner = self.session();
        let subscription_id = inner.next_subscription_id.fetch_add(1, Ordering::Relaxed);
        let response = send_and_wait_on(
            &inner,
            ClientPayload::AddSubscription(AddSubscription {
                subscription_id,
                mode: mode as i32,
                subscription: Some(spec),
            }),
        )
        .await?;
        match response.payload {
            Some(ServerPayload::SubscriptionAdded(added))
                if added.subscription_id == subscription_id =>
            {
                Ok(SubscriptionRegistration {
                    subscription_id,
                    available_from: added
                        .available_from_time_ns
                        .map(rlean_core::NanosecondTimestamp),
                })
            }
            _ => bail!("sidecar returned an invalid AddSubscription acknowledgement"),
        }
    }

    pub async fn remove_subscription(&self, subscription_id: u64) -> anyhow::Result<()> {
        let inner = self.session();
        inner.live.lock().await.remove(&subscription_id);
        inner.relay.lock().await.remove(&subscription_id);
        let response = send_and_wait_on(
            &inner,
            ClientPayload::RemoveSubscription(RemoveSubscription { subscription_id }),
        )
        .await?;
        match response.payload {
            Some(ServerPayload::SubscriptionRemoved(removed))
                if removed.subscription_id == subscription_id =>
            {
                Ok(())
            }
            _ => bail!("sidecar returned an invalid RemoveSubscription acknowledgement"),
        }
    }

    pub async fn query(
        &self,
        subscription_id: u64,
        start_time_ns: i64,
        end_time_ns: i64,
    ) -> anyhow::Result<DataBatchStream> {
        let inner = self.session();
        let request_id = inner.next_request_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel(16);
        inner.queries.lock().await.insert(request_id, sender);
        if let Err(error) = send_on(
            &inner,
            ClientMessage {
                protocol_version: PROTOCOL_VERSION,
                request_id,
                payload: Some(ClientPayload::BacktestQuery(BacktestQuery {
                    subscription_id,
                    start_time_ns,
                    end_time_ns,
                })),
            },
        )
        .await
        {
            inner.queries.lock().await.remove(&request_id);
            return Err(error);
        }
        Ok(ReceiverStream::new(receiver))
    }

    pub async fn subscribe_live(
        &self,
        config: &SubscriptionDataConfig,
    ) -> anyhow::Result<(u64, DataBatchStream)> {
        self.subscribe_live_spec(SubscriptionSpec::from(config))
            .await
    }

    /// Registers a live subscription from an already-canonical wire spec.
    /// Sidecar followers use this to subscribe to selected leader feeds without
    /// fabricating an engine-owned `SubscriptionDataConfig`.
    pub async fn subscribe_live_spec(
        &self,
        spec: SubscriptionSpec,
    ) -> anyhow::Result<(u64, DataBatchStream)> {
        let inner = self.session();
        if inner.live_data_feed.lock().await.is_none() {
            bail!("cannot add a live subscription before opening the live-data feed");
        }
        let subscription_id = inner.next_subscription_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel(256);
        inner.live.lock().await.insert(subscription_id, sender);
        let result = send_and_wait_on(
            &inner,
            ClientPayload::AddSubscription(AddSubscription {
                subscription_id,
                mode: DeliveryMode::Live as i32,
                subscription: Some(spec),
            }),
        )
        .await;
        match result {
            Ok(response)
                if matches!(
                    response.payload,
                    Some(ServerPayload::SubscriptionAdded(ref added))
                        if added.subscription_id == subscription_id
                ) =>
            {
                Ok((subscription_id, ReceiverStream::new(receiver)))
            }
            Ok(_) => {
                inner.live.lock().await.remove(&subscription_id);
                bail!("sidecar returned an invalid live AddSubscription acknowledgement")
            }
            Err(error) => {
                inner.live.lock().await.remove(&subscription_id);
                Err(error)
            }
        }
    }

    /// Registers one daemon-level subscription for every canonical batch
    /// ingested by the upstream leader.
    pub async fn subscribe_relay(&self) -> anyhow::Result<(u64, RelayBatchStream)> {
        let inner = self.session();
        let subscription_id = inner.next_subscription_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = mpsc::channel(256);
        inner.relay.lock().await.insert(subscription_id, sender);
        let result = send_and_wait_on(
            &inner,
            ClientPayload::AddSubscription(AddSubscription {
                subscription_id,
                mode: DeliveryMode::Relay as i32,
                subscription: Some(SubscriptionSpec::default()),
            }),
        )
        .await;
        match result {
            Ok(response)
                if matches!(
                    response.payload,
                    Some(ServerPayload::SubscriptionAdded(ref added))
                        if added.subscription_id == subscription_id
                ) =>
            {
                Ok((subscription_id, ReceiverStream::new(receiver)))
            }
            Ok(_) => {
                inner.relay.lock().await.remove(&subscription_id);
                bail!("sidecar returned an invalid relay AddSubscription acknowledgement")
            }
            Err(error) => {
                inner.relay.lock().await.remove(&subscription_id);
                Err(error)
            }
        }
    }

    async fn send_and_wait(&self, payload: ClientPayload) -> anyhow::Result<ServerMessage> {
        send_and_wait_on(&self.session(), payload).await
    }
}

/// A deployment keeps one logical id for idempotent brokerage commands, while
/// each transport generation gets a distinct protocol session id. This mirrors
/// LEAN's separation between an algorithm job and a concrete brokerage/data
/// connection and prevents a stale server-side session from rejecting recovery
/// as "already connected".
fn transport_session_id(logical_session_id: &str, epoch: u64) -> String {
    format!("{logical_session_id}:{epoch}")
}

/// Establish a fresh Flight session: open the bidirectional exchange, spawn the
/// server-message router, and complete the Initialize handshake. Used for the
/// first connect and for every reconnect after a sidecar restart.
async fn connect_session(
    config: &DataSidecarConfig,
    origin_id: &str,
    session_id: &str,
    epoch: u64,
) -> anyhow::Result<Arc<SessionInner>> {
    let channel = connect_channel(config).await?;
    let mut client = FlightServiceClient::new(channel)
        .max_decoding_message_size(16 * 1024 * 1024)
        .max_encoding_message_size(16 * 1024 * 1024);
    let (outbound, receiver) = mpsc::channel(256);
    let mut request = Request::new(ReceiverStream::new(receiver));
    if let Some(token) = &config.token {
        request.metadata_mut().insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {token}"))
                .context("invalid Flight authorization metadata")?,
        );
    }
    let response = client
        .do_exchange(request)
        .await
        .with_context(|| format!("failed to open Flight exchange at {}", config.endpoint))?
        .into_inner();
    let inner = Arc::new(SessionInner {
        outbound,
        controls: Mutex::new(HashMap::new()),
        queries: Mutex::new(HashMap::new()),
        live: Mutex::new(HashMap::new()),
        relay: Mutex::new(HashMap::new()),
        live_data_feed: Mutex::new(None),
        brokerages: Mutex::new(HashMap::new()),
        next_request_id: AtomicU64::new(1),
        next_subscription_id: AtomicU64::new(1),
        next_connection_id: AtomicU64::new(1),
        epoch,
    });
    tokio::spawn(route_server_messages(Arc::downgrade(&inner), response));
    let response = send_and_wait_on(
        &inner,
        ClientPayload::Initialize(Initialize {
            origin_id: origin_id.to_string(),
            session_id: session_id.to_string(),
        }),
    )
    .await?;
    if !matches!(response.payload, Some(ServerPayload::Initialized(_))) {
        bail!("sidecar returned an invalid Initialize acknowledgement");
    }
    Ok(inner)
}

/// Reopen a remembered live-data feed on a freshly reconnected session under its
/// original connection id, so the deployment's `live_data_feed_connection_id`
/// stays valid across a sidecar restart.
async fn reopen_live_data_feed(
    inner: &SessionInner,
    feed: &RememberedLiveFeed,
) -> anyhow::Result<()> {
    let response = send_and_wait_on(
        inner,
        ClientPayload::OpenLiveDataFeed(OpenLiveDataFeed {
            feed_connection_id: feed.feed_connection_id,
            provider: feed.provider.clone(),
            opaque_config_json: feed.opaque_config_json.clone(),
        }),
    )
    .await?;
    match response.payload {
        Some(ServerPayload::LiveDataFeedOpened(opened))
            if opened.feed_connection_id == feed.feed_connection_id =>
        {
            *inner.live_data_feed.lock().await = Some(feed.feed_connection_id);
            Ok(())
        }
        _ => bail!("sidecar returned an invalid OpenLiveDataFeed acknowledgement on reconnect"),
    }
}

async fn send_and_wait_on(
    inner: &SessionInner,
    payload: ClientPayload,
) -> anyhow::Result<ServerMessage> {
    let request_id = inner.next_request_id.fetch_add(1, Ordering::Relaxed);
    let (sender, receiver) = oneshot::channel();
    inner.controls.lock().await.insert(request_id, sender);
    if let Err(error) = send_on(
        inner,
        ClientMessage {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            payload: Some(payload),
        },
    )
    .await
    {
        inner.controls.lock().await.remove(&request_id);
        return Err(error);
    }
    receiver
        .await
        .context("Flight exchange closed before acknowledgement")?
}

async fn send_on(inner: &SessionInner, message: ClientMessage) -> anyhow::Result<()> {
    inner
        .outbound
        .send(message.into_flight_data())
        .await
        .context("Flight exchange request stream is closed")
}

async fn route_server_messages(
    inner: std::sync::Weak<SessionInner>,
    mut response: tonic::Streaming<FlightData>,
) {
    loop {
        let data = match response.message().await {
            Ok(Some(data)) => data,
            Ok(None) => break,
            Err(error) => {
                eprintln!("Flight exchange response stream failed: {error}");
                break;
            }
        };
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let message = match ServerMessage::decode_flight_data(&data) {
            Ok(message) if message.protocol_version == PROTOCOL_VERSION => message,
            Ok(message) => {
                fail_session(
                    inner.as_ref(),
                    format!(
                        "sidecar protocol version {} does not match {}",
                        message.protocol_version, PROTOCOL_VERSION
                    ),
                )
                .await;
                return;
            }
            Err(error) => {
                fail_session(inner.as_ref(), format!("invalid sidecar response: {error}")).await;
                return;
            }
        };
        match message.payload.as_ref() {
            Some(ServerPayload::DataBatch(metadata)) => {
                let batch = decode_record_batch(data.data_body.as_ref());
                if let Some(sender) = inner
                    .relay
                    .lock()
                    .await
                    .get(&metadata.subscription_id)
                    .cloned()
                {
                    let relay = match (batch, metadata.subscription.clone()) {
                        (Ok(batch), Some(subscription)) => {
                            crate::WireDataType::try_from(metadata.data_type)
                                .map(|data_type| RelayBatch {
                                    subscription,
                                    data_type,
                                    batch,
                                })
                                .map_err(|_| {
                                    anyhow::anyhow!(
                                        "invalid relayed data type {}",
                                        metadata.data_type
                                    )
                                })
                        }
                        (Err(error), _) => Err(error),
                        (_, None) => Err(anyhow::anyhow!(
                            "relayed batch is missing subscription identity"
                        )),
                    };
                    let _ = sender.send(relay).await;
                    continue;
                }
                let sender = if metadata.query_id == 0 {
                    inner
                        .live
                        .lock()
                        .await
                        .get(&metadata.subscription_id)
                        .cloned()
                } else {
                    inner.queries.lock().await.get(&metadata.query_id).cloned()
                };
                if let Some(sender) = sender {
                    let _ = sender.send(batch).await;
                }
            }
            Some(ServerPayload::BacktestQueryComplete(complete)) => {
                inner.queries.lock().await.remove(&complete.query_id);
            }
            Some(ServerPayload::BrokerageOrderUpdate(update)) => {
                if let Some(sender) = inner
                    .brokerages
                    .lock()
                    .await
                    .get(&update.brokerage_connection_id)
                    .cloned()
                {
                    let _ = sender.send(Ok(BrokerageEvent::Order(update.clone()))).await;
                }
            }
            Some(ServerPayload::BrokerageConnectionState(state)) => {
                if let Some(sender) = inner
                    .brokerages
                    .lock()
                    .await
                    .get(&state.brokerage_connection_id)
                    .cloned()
                {
                    let _ = sender
                        .send(Ok(BrokerageEvent::Connection(state.clone())))
                        .await;
                }
            }
            Some(ServerPayload::Error(error)) => {
                let error_message = error.message.clone();
                if let Some(sender) = inner.controls.lock().await.remove(&message.request_id) {
                    let _ = sender.send(Err(anyhow::anyhow!(error_message)));
                } else if let Some(sender) = inner.queries.lock().await.remove(&message.request_id)
                {
                    let _ = sender.send(Err(anyhow::anyhow!(error_message))).await;
                }
            }
            Some(
                ServerPayload::Initialized(_)
                | ServerPayload::Manifest(_)
                | ServerPayload::SubscriptionAdded(_)
                | ServerPayload::SubscriptionRemoved(_)
                | ServerPayload::LiveDataFeedOpened(_)
                | ServerPayload::LiveDataFeedClosed(_)
                | ServerPayload::BrokerageOpened(_)
                | ServerPayload::BrokerageClosed(_)
                | ServerPayload::BrokerageCommandResult(_)
                | ServerPayload::BrokerageSnapshot(_),
            ) => {
                if let Some(sender) = inner.controls.lock().await.remove(&message.request_id) {
                    let _ = sender.send(Ok(message));
                }
            }
            None => {}
        }
    }
    if let Some(inner) = inner.upgrade() {
        fail_session(inner.as_ref(), "Flight exchange closed".to_string()).await;
    }
}

async fn fail_session(inner: &SessionInner, message: String) {
    for (_, sender) in inner.controls.lock().await.drain() {
        let _ = sender.send(Err(anyhow::anyhow!(message.clone())));
    }
    for (_, sender) in inner.queries.lock().await.drain() {
        let _ = sender.send(Err(anyhow::anyhow!(message.clone()))).await;
    }
    for (_, sender) in inner.live.lock().await.drain() {
        let _ = sender.send(Err(anyhow::anyhow!(message.clone()))).await;
    }
    for (_, sender) in inner.relay.lock().await.drain() {
        let _ = sender.send(Err(anyhow::anyhow!(message.clone()))).await;
    }
    for (_, sender) in inner.brokerages.lock().await.drain() {
        let _ = sender.send(Err(anyhow::anyhow!(message.clone()))).await;
    }
}

async fn connect_channel(config: &DataSidecarConfig) -> anyhow::Result<Channel> {
    let endpoint = SidecarEndpoint::parse(&config.endpoint)?;
    let timeout = Duration::from_millis(config.connect_timeout_ms);
    match endpoint {
        SidecarEndpoint::Tcp { authority, tls } => {
            let scheme = if tls { "https" } else { "http" };
            let mut endpoint = Endpoint::from_shared(format!("{scheme}://{authority}"))?
                .connect_timeout(timeout)
                .tcp_nodelay(true)
                .http2_keep_alive_interval(Duration::from_secs(20))
                .keep_alive_timeout(Duration::from_secs(10))
                .keep_alive_while_idle(true);
            if tls {
                endpoint = endpoint
                    .tls_config(ClientTlsConfig::new().with_webpki_roots())
                    .context("failed to configure Flight TLS")?;
            }
            endpoint
                .connect()
                .await
                .with_context(|| format!("failed to connect to {}", config.endpoint))
        }
        #[cfg(unix)]
        SidecarEndpoint::Unix(path) => {
            let connector_path = path.clone();
            Endpoint::from_static("http://[::]:50051")
                .connect_timeout(timeout)
                .connect_with_connector(service_fn(move |_| {
                    let path = connector_path.clone();
                    async move {
                        tokio::net::UnixStream::connect(path)
                            .await
                            .map(TokioIo::new)
                    }
                }))
                .await
                .with_context(|| format!("failed to connect to Flight socket {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::transport_session_id;

    #[test]
    fn each_transport_generation_has_a_distinct_protocol_session() {
        assert_eq!(transport_session_id("deployment", 0), "deployment:0");
        assert_eq!(transport_session_id("deployment", 1), "deployment:1");
        assert_ne!(
            transport_session_id("deployment", 41),
            transport_session_id("deployment", 42)
        );
    }
}
