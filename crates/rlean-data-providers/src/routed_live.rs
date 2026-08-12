use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::{
    HistoricalData, HistoricalDataStore, HistoryRequest, LiveDataEvent, LiveDataProvider,
    LiveSubscription, VerglasHistoricalDataStore,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::{stream, Stream};
use rlean_core::{DateTime, TimeSpan};
use rlean_data::SubscriptionDataKind;
use tokio::sync::{mpsc, watch, Mutex, RwLock};

/// Routes LEAN subscription intent to independent market and custom-data live
/// sources while exposing one event stream to the engine synchronizer.
pub struct RoutedLiveDataProvider {
    market: Arc<dyn LiveDataProvider>,
    custom: Arc<dyn LiveDataProvider>,
    routes: RwLock<HashMap<u64, SubscriptionDataKind>>,
}

impl RoutedLiveDataProvider {
    pub fn new(market: Arc<dyn LiveDataProvider>, custom: Arc<dyn LiveDataProvider>) -> Self {
        Self {
            market,
            custom,
            routes: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl LiveDataProvider for RoutedLiveDataProvider {
    fn name(&self) -> &str {
        "routed"
    }

    fn is_connected(&self) -> bool {
        self.market.is_connected() && self.custom.is_connected()
    }

    async fn connect(&self) -> Result<()> {
        self.market.connect().await?;
        self.custom.connect().await?;
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        let (market, custom) = tokio::join!(self.market.disconnect(), self.custom.disconnect());
        market?;
        custom?;
        Ok(())
    }

    async fn subscribe(&self, subscription: LiveSubscription) -> Result<()> {
        let kind = subscription.configuration.data_kind;
        let provider = if kind == SubscriptionDataKind::Custom {
            &self.custom
        } else {
            &self.market
        };
        provider.subscribe(subscription.clone()).await?;
        self.routes.write().await.insert(subscription.id, kind);
        Ok(())
    }

    async fn unsubscribe(&self, subscription_id: u64) -> Result<()> {
        let kind = self.routes.write().await.remove(&subscription_id);
        match kind {
            Some(SubscriptionDataKind::Custom) => self.custom.unsubscribe(subscription_id).await,
            Some(_) => self.market.unsubscribe(subscription_id).await,
            None => Ok(()),
        }
    }

    async fn events(&self) -> Result<futures::stream::BoxStream<'static, Result<LiveDataEvent>>> {
        let market = self.market.events().await?;
        let custom = self.custom.events().await?;
        Ok(Box::pin(stream::select(market, custom)))
    }
}

#[derive(Clone)]
struct CustomSubscriptionState {
    subscription: LiveSubscription,
    frontier: DateTime,
}

/// Subscribes to durable Verglas table commits and publishes custom-data rows.
/// Rows remain provider-neutral canonical `rlean.custom_points` records.
pub struct VerglasCustomLiveDataProvider {
    database: verglas_sdk::Database,
    store: VerglasHistoricalDataStore,
    consumer_group: String,
    consumer_owner: String,
    subscriptions: Arc<RwLock<HashMap<u64, CustomSubscriptionState>>>,
    event_tx: mpsc::Sender<Result<LiveDataEvent>>,
    event_rx: Mutex<Option<mpsc::Receiver<Result<LiveDataEvent>>>>,
    connected: Arc<AtomicBool>,
    shutdown: watch::Sender<bool>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl VerglasCustomLiveDataProvider {
    pub async fn new(client: verglas_sdk::Database, consumer_group: String) -> Result<Self> {
        if consumer_group.trim().is_empty() {
            bail!("Verglas custom subscription consumer group must not be empty");
        }
        let store = VerglasHistoricalDataStore::new(client.clone()).await?;
        let (event_tx, event_rx) = mpsc::channel(4_096);
        let (shutdown, _) = watch::channel(false);
        Ok(Self {
            database: client,
            store,
            consumer_group,
            consumer_owner: format!("rlean-{}-{}", std::process::id(), uuid::Uuid::new_v4()),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            event_rx: Mutex::new(Some(event_rx)),
            connected: Arc::new(AtomicBool::new(false)),
            shutdown,
            worker: Mutex::new(None),
        })
    }
}

#[async_trait]
impl LiveDataProvider for VerglasCustomLiveDataProvider {
    fn name(&self) -> &str {
        "verglas-custom"
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    async fn connect(&self) -> Result<()> {
        let mut worker = self.worker.lock().await;
        if worker.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return Ok(());
        }
        let _ = self.shutdown.send(false);
        *worker = Some(tokio::spawn(
            CustomSubscriptionWorker {
                database: self.database.clone(),
                store: self.store.clone(),
                consumer_group: self.consumer_group.clone(),
                consumer_owner: self.consumer_owner.clone(),
                subscriptions: self.subscriptions.clone(),
                events: self.event_tx.clone(),
                connected: self.connected.clone(),
            }
            .run(self.shutdown.subscribe()),
        ));
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        let _ = self.shutdown.send(true);
        if let Some(worker) = self.worker.lock().await.take() {
            worker
                .await
                .context("join Verglas custom subscription worker")?;
        }
        self.connected.store(false, Ordering::Release);
        Ok(())
    }

    async fn subscribe(&self, subscription: LiveSubscription) -> Result<()> {
        if subscription.configuration.data_kind != SubscriptionDataKind::Custom {
            bail!("Verglas custom live provider accepts only custom subscriptions");
        }
        let custom = subscription
            .configuration
            .custom
            .as_ref()
            .context("custom live subscription has no metadata")?;
        tracing::info!(
            subscription_id = subscription.id,
            provider = %custom.source_type,
            feed = %custom.ticker,
            resolution = %subscription.configuration.resolution,
            "subscribed to Verglas custom-data events"
        );
        let lookback = subscription
            .configuration
            .resolution
            .to_time_span()
            .unwrap_or(TimeSpan::ONE_MINUTE);
        let start =
            DateTime::now() - TimeSpan::from_nanos((lookback.nanos * 2).max(600_000_000_000));
        self.subscriptions.write().await.insert(
            subscription.id,
            CustomSubscriptionState {
                subscription: subscription.clone(),
                frontier: start,
            },
        );
        if let Err(error) = publish_subscription(
            &self.store,
            &self.subscriptions,
            &self.event_tx,
            subscription.id,
            DateTime::now() + TimeSpan::ONE_MINUTE,
        )
        .await
        {
            self.subscriptions.write().await.remove(&subscription.id);
            return Err(error.context("initial Verglas custom-data catch-up"));
        }
        Ok(())
    }

    async fn unsubscribe(&self, subscription_id: u64) -> Result<()> {
        self.subscriptions.write().await.remove(&subscription_id);
        Ok(())
    }

    async fn events(&self) -> Result<futures::stream::BoxStream<'static, Result<LiveDataEvent>>> {
        let receiver = self
            .event_rx
            .lock()
            .await
            .take()
            .context("Verglas custom event stream has already been taken")?;
        Ok(Box::pin(MpscStream(receiver)))
    }
}

struct CustomSubscriptionWorker {
    database: verglas_sdk::Database,
    store: VerglasHistoricalDataStore,
    consumer_group: String,
    consumer_owner: String,
    subscriptions: Arc<RwLock<HashMap<u64, CustomSubscriptionState>>>,
    events: mpsc::Sender<Result<LiveDataEvent>>,
    connected: Arc<AtomicBool>,
}

// A table commit wakes every filtered custom-data subscription that shares the
// table. Claim one commit at a time so a subscriber never holds fenced receipts
// while another commit's bounded query runs through the serialized dispatcher.
const TABLE_EVENT_MAX_IN_FLIGHT: usize = 1;
const TABLE_EVENT_LEASE_SECONDS: u64 = 10 * 60;

impl CustomSubscriptionWorker {
    async fn run(self, mut shutdown: watch::Receiver<bool>) {
        use futures::StreamExt;
        use verglas_sdk::TableSubscriptionEvent;

        let mut feed = match self.database.subscribe(
            &self.consumer_group,
            &self.consumer_owner,
            ["rlean.custom_points"],
            Some(TABLE_EVENT_MAX_IN_FLIGHT),
            TABLE_EVENT_LEASE_SECONDS,
        ) {
            Ok(feed) => feed,
            Err(error) => {
                let _ = self
                    .events
                    .send(Err(
                        anyhow::Error::new(error).context("subscribe to Verglas table events")
                    ))
                    .await;
                return;
            }
        };
        loop {
            tokio::select! {
                _ = shutdown.changed() => if *shutdown.borrow() { return; },
                next = feed.next() => match next {
                Some(Ok(TableSubscriptionEvent::Connected)) => {
                    self.connected.store(true, Ordering::Release);
                    let ids = self.subscriptions.read().await.keys().copied().collect::<Vec<_>>();
                    let end = DateTime::now() + TimeSpan::ONE_MINUTE;
                    for id in ids {
                        if let Err(error) = publish_subscription(
                            &self.store,
                            &self.subscriptions,
                            &self.events,
                            id,
                            end,
                        ).await {
                            let _ = self.events.send(Err(error.context(
                                "Verglas custom-data reconnect catch-up",
                            ))).await;
                        }
                    }
                    let _ = self.events.send(Ok(LiveDataEvent::Reconnected)).await;
                }
                Some(Ok(TableSubscriptionEvent::Disconnected)) => {
                    self.connected.store(false, Ordering::Release);
                    let _ = self.events.send(Ok(LiveDataEvent::Disconnected {
                        reason: "Verglas table subscription disconnected; SDK is reconnecting"
                            .to_owned(),
                    })).await;
                }
                Some(Ok(TableSubscriptionEvent::Delivery(delivery))) => {
                    let ids = self.subscriptions.read().await.keys().copied().collect::<Vec<_>>();
                    let end = DateTime::now() + TimeSpan::ONE_MINUTE;
                    let mut published = true;
                    for id in ids {
                        if let Err(error) = publish_subscription(
                            &self.store,
                            &self.subscriptions,
                            &self.events,
                            id,
                            end,
                        ).await {
                            published = false;
                            let _ = self.events.send(Err(error.context(
                                "Verglas custom-data commit delivery",
                            ))).await;
                        }
                    }
                    if published {
                        if let Err(error) =
                            self.database.ack(&self.consumer_group, &delivery.receipt).await
                        {
                            let _ = self.events
                                .send(Err(anyhow::Error::new(error).context(
                                    "acknowledge Verglas custom-data commit",
                                )))
                                .await;
                        }
                    }
                }
                Some(Err(error)) => {
                    self.connected.store(false, Ordering::Release);
                    let _ = self.events.send(Err(anyhow::Error::new(error).context(
                        "Verglas table subscription failed",
                    ))).await;
                    return;
                }
                None => {
                    self.connected.store(false, Ordering::Release);
                    let _ = self.events.send(Err(anyhow::anyhow!(
                        "Verglas table subscription ended unexpectedly",
                    ))).await;
                    return;
                }
                },
            }
        }
    }
}

async fn publish_subscription(
    store: &VerglasHistoricalDataStore,
    subscriptions: &RwLock<HashMap<u64, CustomSubscriptionState>>,
    events: &mpsc::Sender<Result<LiveDataEvent>>,
    subscription_id: u64,
    end: DateTime,
) -> Result<()> {
    let state = subscriptions.read().await.get(&subscription_id).cloned();
    let Some(state) = state else {
        return Ok(());
    };
    if end <= state.frontier {
        return Ok(());
    }
    let request = HistoryRequest::new(
        state.subscription.configuration.clone(),
        state.frontier,
        end,
    )?;
    let data = store.read(&request).await?;
    let HistoricalData::CustomPoints(rows) = data else {
        bail!("Verglas custom live query returned non-custom data");
    };
    let custom = state
        .subscription
        .configuration
        .custom
        .as_ref()
        .context("custom live subscription has no metadata")?;
    if rows.is_empty() {
        return Ok(());
    }
    let frontier = rows
        .iter()
        .map(|row| row.end_time)
        .max()
        .unwrap_or(state.frontier);
    if let Some(current) = subscriptions.write().await.get_mut(&subscription_id) {
        // Store reads include the lower bound so every row sharing the final
        // event timestamp arrives together. Advance one nanosecond only after
        // publishing that complete timestamp group to prevent the next poll or
        // commit notification from relaying it again.
        current.frontier = current.frontier.max(frontier + TimeSpan::from_nanos(1));
    }
    let row_count = rows.len();
    events
        .send(Ok(LiveDataEvent::Data {
            subscription_id,
            data: HistoricalData::CustomPoints(rows),
        }))
        .await
        .context("publish Verglas custom live data")?;
    tracing::info!(
        subscription_id,
        provider = %custom.source_type,
        feed = %custom.ticker,
        rows = row_count,
        frontier_ns = frontier.0,
        "published Verglas custom-data events"
    );
    Ok(())
}

struct MpscStream<T>(mpsc::Receiver<T>);

impl<T> Stream for MpscStream<T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        self.0.poll_recv(context)
    }
}
