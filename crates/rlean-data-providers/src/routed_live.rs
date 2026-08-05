use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::{stream, Stream, StreamExt};
use rlean_core::{DateTime, TimeSpan};
use rlean_data::SubscriptionDataKind;
use tokio::sync::{mpsc, watch, Mutex, RwLock};
use verglas_sdk::Client;

use crate::{
    HistoricalData, HistoricalDataStore, HistoryRequest, LiveDataEvent, LiveDataProvider,
    LiveSubscription, VerglasHistoricalDataStore,
};

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

/// Turns Verglas table commits into bounded custom-data reads and unsolicited
/// live events. The catalog feed carries only commit notifications; rows remain
/// provider-neutral canonical `rlean.custom_points` records.
pub struct VerglasCustomLiveDataProvider {
    client: Client,
    store: VerglasHistoricalDataStore,
    subscriptions: Arc<RwLock<HashMap<u64, CustomSubscriptionState>>>,
    event_tx: mpsc::Sender<Result<LiveDataEvent>>,
    event_rx: Mutex<Option<mpsc::Receiver<Result<LiveDataEvent>>>>,
    connected: Arc<AtomicBool>,
    shutdown: watch::Sender<bool>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl VerglasCustomLiveDataProvider {
    pub async fn new(client: Client) -> Result<Self> {
        let store = VerglasHistoricalDataStore::new(client.clone()).await?;
        let (event_tx, event_rx) = mpsc::channel(4_096);
        let (shutdown, _) = watch::channel(false);
        Ok(Self {
            client,
            store,
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            event_rx: Mutex::new(Some(event_rx)),
            connected: Arc::new(AtomicBool::new(false)),
            shutdown,
            worker: Mutex::new(None),
        })
    }

    async fn publish_subscription(&self, subscription_id: u64, end: DateTime) -> Result<()> {
        publish_subscription(
            &self.store,
            &self.subscriptions,
            &self.event_tx,
            subscription_id,
            end,
        )
        .await
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
        *worker = Some(tokio::spawn(run_follow(
            self.client.clone(),
            self.store.clone(),
            self.subscriptions.clone(),
            self.event_tx.clone(),
            self.connected.clone(),
            self.shutdown.subscribe(),
        )));
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        let _ = self.shutdown.send(true);
        if let Some(worker) = self.worker.lock().await.take() {
            worker.await.context("join Verglas custom follow worker")?;
        }
        self.connected.store(false, Ordering::Release);
        Ok(())
    }

    async fn subscribe(&self, subscription: LiveSubscription) -> Result<()> {
        if subscription.configuration.data_kind != SubscriptionDataKind::Custom {
            bail!("Verglas custom live provider accepts only custom subscriptions");
        }
        let lookback = subscription
            .configuration
            .resolution
            .to_time_span()
            .unwrap_or(TimeSpan::ONE_MINUTE);
        let start =
            DateTime::now() - TimeSpan::from_nanos((lookback.nanos * 2).max(120_000_000_000));
        self.subscriptions.write().await.insert(
            subscription.id,
            CustomSubscriptionState {
                subscription: subscription.clone(),
                frontier: start,
            },
        );
        if let Err(error) = self
            .publish_subscription(subscription.id, DateTime::now() + TimeSpan::ONE_MINUTE)
            .await
        {
            tracing::warn!(
                subscription_id = subscription.id,
                %error,
                "initial durable custom-data read failed; the live provider will retry"
            );
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

async fn run_follow(
    client: Client,
    store: VerglasHistoricalDataStore,
    subscriptions: Arc<RwLock<HashMap<u64, CustomSubscriptionState>>>,
    events: mpsc::Sender<Result<LiveDataEvent>>,
    connected: Arc<AtomicBool>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut retry = Duration::from_millis(250);
    loop {
        if *shutdown.borrow() {
            return;
        }
        let mut changes = match client.follow(["rlean.custom_points"], None) {
            Ok(changes) => changes,
            Err(error) => {
                let _ = events.send(Err(error.into())).await;
                return;
            }
        };
        connected.store(true, Ordering::Release);
        // A commit can land while the follow stream is reconnecting. Read from
        // each durable frontier before waiting for the next notification so a
        // quiet table cannot strand that data indefinitely.
        let ids = subscriptions
            .read()
            .await
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let end = DateTime::now() + TimeSpan::ONE_MINUTE;
        for id in ids {
            if let Err(error) = publish_subscription(&store, &subscriptions, &events, id, end).await
            {
                tracing::warn!(subscription_id = id, %error, "durable custom-data read failed");
            }
        }
        loop {
            tokio::select! {
                _ = shutdown.changed() => if *shutdown.borrow() { return; },
                change = changes.next() => match change {
                    Some(Ok(_)) => {
                        let ids = subscriptions.read().await.keys().copied().collect::<Vec<_>>();
                        let end = DateTime::now() + TimeSpan::ONE_MINUTE;
                        for id in ids {
                            if let Err(error) = publish_subscription(&store, &subscriptions, &events, id, end).await {
                                tracing::warn!(subscription_id = id, %error, "durable custom-data read failed");
                            }
                        }
                        retry = Duration::from_millis(250);
                    }
                    Some(Err(error)) => {
                        if is_unsupported_follow(&error) {
                            tracing::info!(
                                "Verglas catalog follow is unavailable; polling durable custom-data frontiers"
                            );
                            run_polling(
                                &store,
                                &subscriptions,
                                &events,
                                &connected,
                                &mut shutdown,
                            )
                            .await;
                            return;
                        }
                        connected.store(false, Ordering::Release);
                        tracing::warn!(%error, "Verglas custom-data follow disconnected");
                        break;
                    }
                    None => {
                        connected.store(false, Ordering::Release);
                        break;
                    }
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(retry) => {},
            _ = shutdown.changed() => if *shutdown.borrow() { return; },
        }
        retry = (retry * 2).min(Duration::from_secs(30));
    }
}

fn is_unsupported_follow(error: &verglas_sdk::ClientError) -> bool {
    matches!(
        error,
        verglas_sdk::ClientError::Http { status, .. }
            if *status == reqwest::StatusCode::NOT_FOUND
                || *status == reqwest::StatusCode::NOT_IMPLEMENTED
    ) || error.to_string().contains("404 Not Found")
}

async fn run_polling(
    store: &VerglasHistoricalDataStore,
    subscriptions: &RwLock<HashMap<u64, CustomSubscriptionState>>,
    events: &mpsc::Sender<Result<LiveDataEvent>>,
    connected: &AtomicBool,
    shutdown: &mut watch::Receiver<bool>,
) {
    connected.store(true, Ordering::Release);
    loop {
        let ids = subscriptions
            .read()
            .await
            .keys()
            .copied()
            .collect::<Vec<_>>();
        let end = DateTime::now() + TimeSpan::ONE_MINUTE;
        for id in ids {
            if let Err(error) = publish_subscription(store, subscriptions, events, id, end).await {
                tracing::warn!(subscription_id = id, %error, "durable custom-data poll failed");
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(5)) => {},
            _ = shutdown.changed() => if *shutdown.borrow() { return; },
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
    events
        .send(Ok(LiveDataEvent::Data {
            subscription_id,
            data: HistoricalData::CustomPoints(rows),
        }))
        .await
        .context("publish Verglas custom live data")
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
