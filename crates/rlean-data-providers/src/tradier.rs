use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::{SinkExt, Stream, StreamExt};
use reqwest::Client;
use rlean_core::{NanosecondTimestamp, Resolution, TickType, TimeSpan};
use rlean_data_tables::{Bar, QuoteBar, TradeBar, TradeBarData};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, watch, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;

use crate::{HistoricalData, LiveDataEvent, LiveDataProvider, LiveSubscription};

const LIVE_BASE: &str = "https://api.tradier.com/v1";
const PAPER_BASE: &str = "https://sandbox.tradier.com/v1";
const DEFAULT_WEBSOCKET: &str = "wss://ws.tradier.com/v1/markets/events";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradierEnvironment {
    Live,
    Paper,
}

#[derive(Debug, Clone)]
pub struct TradierMarketDataConfig {
    pub access_token: String,
    pub environment: TradierEnvironment,
    pub base_url: String,
}

impl TradierMarketDataConfig {
    pub fn new(access_token: impl Into<String>, environment: TradierEnvironment) -> Self {
        Self {
            access_token: access_token.into(),
            environment,
            base_url: match environment {
                TradierEnvironment::Live => LIVE_BASE,
                TradierEnvironment::Paper => PAPER_BASE,
            }
            .to_string(),
        }
    }
}

pub struct TradierLiveDataProvider {
    config: Arc<TradierMarketDataConfig>,
    client: Client,
    subscriptions: Arc<RwLock<HashMap<u64, LiveSubscription>>>,
    connected: Arc<AtomicBool>,
    event_tx: mpsc::Sender<Result<LiveDataEvent>>,
    event_rx: Mutex<Option<mpsc::Receiver<Result<LiveDataEvent>>>>,
    shutdown: watch::Sender<bool>,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl TradierLiveDataProvider {
    pub fn new(config: TradierMarketDataConfig) -> Result<Self> {
        if config.access_token.trim().is_empty() {
            bail!("Tradier market-data access token cannot be empty");
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .context("build Tradier HTTP client")?;
        let (event_tx, event_rx) = mpsc::channel(1024);
        let (shutdown, _) = watch::channel(false);
        Ok(Self {
            config: Arc::new(config),
            client,
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            connected: Arc::new(AtomicBool::new(false)),
            event_tx,
            event_rx: Mutex::new(Some(event_rx)),
            shutdown,
            worker: Mutex::new(None),
        })
    }
}

#[async_trait]
impl LiveDataProvider for TradierLiveDataProvider {
    fn name(&self) -> &str {
        "tradier"
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    async fn connect(&self) -> Result<()> {
        let mut worker = self.worker.lock().await;
        if worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Ok(());
        }
        let _ = self.shutdown.send(false);
        let task = tokio::spawn(run_feed(
            self.config.clone(),
            self.client.clone(),
            self.subscriptions.clone(),
            self.connected.clone(),
            self.event_tx.clone(),
            self.shutdown.subscribe(),
        ));
        *worker = Some(task);
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        let _ = self.shutdown.send(true);
        if let Some(worker) = self.worker.lock().await.take() {
            worker.await.context("join Tradier live worker")?;
        }
        self.connected.store(false, Ordering::Release);
        Ok(())
    }

    async fn subscribe(&self, subscription: LiveSubscription) -> Result<()> {
        let config = &subscription.configuration;
        if config.resolution == Resolution::Daily {
            bail!("Tradier websocket subscriptions require Hour or finer resolution");
        }
        self.subscriptions
            .write()
            .await
            .insert(subscription.id, subscription);
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
            .context("Tradier event stream has already been taken")?;
        Ok(Box::pin(MpscStream(receiver)))
    }
}

struct MpscStream<T>(mpsc::Receiver<T>);

impl<T> Stream for MpscStream<T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(context)
    }
}

async fn run_feed(
    config: Arc<TradierMarketDataConfig>,
    client: Client,
    subscriptions: Arc<RwLock<HashMap<u64, LiveSubscription>>>,
    connected: Arc<AtomicBool>,
    events: mpsc::Sender<Result<LiveDataEvent>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut delay = Duration::from_secs(1);
    loop {
        if *shutdown.borrow() {
            return;
        }
        match run_connection(
            &config,
            &client,
            &subscriptions,
            &connected,
            &events,
            &mut shutdown,
        )
        .await
        {
            Ok(()) => return,
            Err(error) => {
                connected.store(false, Ordering::Release);
                let reason = format!("{error:#}");
                let _ = events
                    .send(Ok(LiveDataEvent::Disconnected { reason }))
                    .await;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.changed() => {}
        }
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

async fn run_connection(
    config: &TradierMarketDataConfig,
    client: &Client,
    subscriptions: &RwLock<HashMap<u64, LiveSubscription>>,
    connected: &AtomicBool,
    events: &mpsc::Sender<Result<LiveDataEvent>>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    while subscriptions.read().await.is_empty() {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
            _ = shutdown.changed() => if *shutdown.borrow() { return Ok(()); }
        }
    }
    let mut session = create_session(config, client).await?;
    let websocket_url = session.websocket_url();
    let (mut socket, _) = tokio_tungstenite::connect_async(&websocket_url)
        .await
        .with_context(|| format!("connect Tradier websocket {websocket_url}"))?;
    connected.store(true, Ordering::Release);
    events.send(Ok(LiveDataEvent::Reconnected)).await.ok();

    let mut last_symbols = Vec::new();
    let mut session_acquired = std::time::Instant::now();
    let mut refresh = tokio::time::interval(Duration::from_millis(500));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = refresh.tick() => {
                let mut symbols = subscriptions.read().await.values()
                    .map(|subscription| subscription.configuration.symbol.permtick.to_ascii_uppercase())
                    .collect::<Vec<_>>();
                symbols.sort();
                symbols.dedup();
                if symbols != last_symbols && !symbols.is_empty() {
                    if session_acquired.elapsed() >= Duration::from_secs(55 * 60) {
                        session = create_session(config, client).await?;
                        session_acquired = std::time::Instant::now();
                    }
                    socket.send(Message::Text(serde_json::json!({
                        "symbols": symbols,
                        "filter": ["quote", "trade", "timesale", "tradex"],
                        "sessionid": session.sessionid,
                        "linebreak": true,
                        "validOnly": true
                    }).to_string())).await?;
                    last_symbols = symbols;
                }
            }
            _ = shutdown.changed() => if *shutdown.borrow() { return Ok(()); },
            message = socket.next() => {
                let message = message.context("Tradier websocket closed")??;
                match message {
                    Message::Text(text) => publish(&text, subscriptions, events).await?,
                    Message::Binary(bytes) => if let Ok(text) = std::str::from_utf8(&bytes) {
                        publish(text, subscriptions, events).await?;
                    },
                    Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                    Message::Close(frame) => bail!("Tradier websocket closed: {frame:?}"),
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        }
    }
}

async fn publish(
    text: &str,
    subscriptions: &RwLock<HashMap<u64, LiveSubscription>>,
    events: &mpsc::Sender<Result<LiveDataEvent>>,
) -> Result<()> {
    for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let value: Value = serde_json::from_str(line)?;
        if value.get("success").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        if let Some(error) = value.get("error").and_then(Value::as_str) {
            bail!("Tradier websocket error: {error}");
        }
        let event: StreamEvent = serde_json::from_value(value)?;
        let symbol = event.symbol().to_ascii_uppercase();
        let current = subscriptions
            .read()
            .await
            .values()
            .filter(|subscription| {
                subscription
                    .configuration
                    .symbol
                    .permtick
                    .eq_ignore_ascii_case(&symbol)
            })
            .cloned()
            .collect::<Vec<_>>();
        for subscription in current {
            if let Some(data) = live_data(&subscription, &event)? {
                events
                    .send(Ok(LiveDataEvent::Data {
                        subscription_id: subscription.id,
                        data,
                    }))
                    .await
                    .context("Tradier live consumer closed")?;
            }
        }
    }
    Ok(())
}

fn live_data(
    subscription: &LiveSubscription,
    event: &StreamEvent,
) -> Result<Option<HistoricalData>> {
    let config = &subscription.configuration;
    let period = config.resolution.to_time_span().unwrap_or(TimeSpan::ZERO);
    match (config.tick_type, event) {
        (
            TickType::Trade,
            StreamEvent::Trade {
                price, size, date, ..
            },
        )
        | (
            TickType::Trade,
            StreamEvent::Tradex {
                price, size, date, ..
            },
        ) => {
            let Some(price) = Decimal::from_f64(*price) else {
                return Ok(None);
            };
            let time = bucket_time(*date, period);
            Ok(Some(HistoricalData::TradeBars(vec![TradeBar::new(
                config.symbol.clone(),
                time,
                period,
                TradeBarData::new(price, price, price, price, Decimal::from(*size)),
            )
            .with_venue("tradier")])))
        }
        (
            TickType::Trade,
            StreamEvent::Timesale {
                last,
                size,
                date,
                cancel,
                correction,
                ..
            },
        ) if !cancel && !correction => {
            let Some(price) = Decimal::from_f64(*last) else {
                return Ok(None);
            };
            let time = bucket_time(*date, period);
            Ok(Some(HistoricalData::TradeBars(vec![TradeBar::new(
                config.symbol.clone(),
                time,
                period,
                TradeBarData::new(price, price, price, price, Decimal::from(*size)),
            )
            .with_venue("tradier")])))
        }
        (
            TickType::Quote,
            StreamEvent::Quote {
                bid,
                ask,
                bidsz,
                asksz,
                biddate,
                askdate,
                ..
            },
        ) => {
            let (Some(bid), Some(ask)) = (Decimal::from_f64(*bid), Decimal::from_f64(*ask)) else {
                return Ok(None);
            };
            let time = bucket_time((*biddate).max(*askdate), period);
            Ok(Some(HistoricalData::QuoteBars(vec![QuoteBar::new(
                config.symbol.clone(),
                time,
                period,
                Some(Bar::from_price(bid)),
                Some(Bar::from_price(ask)),
                Decimal::from(*bidsz),
                Decimal::from(*asksz),
            )
            .with_venue("tradier")])))
        }
        _ => Ok(None),
    }
}

fn bucket_time(milliseconds: i64, period: TimeSpan) -> NanosecondTimestamp {
    let timestamp = milliseconds.saturating_mul(1_000_000);
    if period.nanos <= 0 {
        NanosecondTimestamp(timestamp)
    } else {
        NanosecondTimestamp(timestamp.div_euclid(period.nanos) * period.nanos)
    }
}

async fn create_session(
    config: &TradierMarketDataConfig,
    client: &Client,
) -> Result<MarketSession> {
    let response = client
        .post(format!(
            "{}/markets/events/session",
            config.base_url.trim_end_matches('/')
        ))
        .bearer_auth(&config.access_token)
        .header("Accept", "application/json")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body("")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        bail!(
            "Tradier streaming session failed with HTTP {status}: {}",
            response.text().await.unwrap_or_default()
        );
    }
    let response: MarketSessionResponse = response.json().await?;
    if response.stream.sessionid.trim().is_empty() {
        bail!("Tradier streaming session omitted sessionid");
    }
    Ok(response.stream)
}

#[derive(Deserialize)]
struct MarketSessionResponse {
    stream: MarketSession,
}

#[derive(Deserialize)]
struct MarketSession {
    #[serde(default)]
    url: String,
    sessionid: String,
}

impl MarketSession {
    fn websocket_url(&self) -> String {
        let url = self.url.trim();
        if url.is_empty() {
            DEFAULT_WEBSOCKET.to_string()
        } else if let Some(rest) = url.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = url.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            url.to_string()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum StreamEvent {
    Quote {
        symbol: String,
        #[serde(deserialize_with = "flex_f64")]
        bid: f64,
        #[serde(deserialize_with = "flex_f64")]
        ask: f64,
        #[serde(deserialize_with = "flex_i64")]
        bidsz: i64,
        #[serde(deserialize_with = "flex_i64")]
        asksz: i64,
        #[serde(deserialize_with = "flex_i64")]
        biddate: i64,
        #[serde(deserialize_with = "flex_i64")]
        askdate: i64,
    },
    Trade {
        symbol: String,
        #[serde(deserialize_with = "flex_f64")]
        price: f64,
        #[serde(deserialize_with = "flex_i64")]
        size: i64,
        #[serde(deserialize_with = "flex_i64")]
        date: i64,
    },
    Tradex {
        symbol: String,
        #[serde(deserialize_with = "flex_f64")]
        price: f64,
        #[serde(deserialize_with = "flex_i64")]
        size: i64,
        #[serde(deserialize_with = "flex_i64")]
        date: i64,
    },
    Timesale {
        symbol: String,
        #[serde(deserialize_with = "flex_f64")]
        last: f64,
        #[serde(deserialize_with = "flex_i64")]
        size: i64,
        #[serde(deserialize_with = "flex_i64")]
        date: i64,
        #[serde(default)]
        cancel: bool,
        #[serde(default)]
        correction: bool,
    },
    #[serde(other)]
    Other,
}

impl StreamEvent {
    fn symbol(&self) -> &str {
        match self {
            Self::Quote { symbol, .. }
            | Self::Trade { symbol, .. }
            | Self::Tradex { symbol, .. }
            | Self::Timesale { symbol, .. } => symbol,
            Self::Other => "",
        }
    }
}

fn flex_f64<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<f64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .ok_or_else(|| serde::de::Error::custom("invalid decimal"))
}

fn flex_i64<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<i64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value
        .as_i64()
        .or_else(|| value.as_str()?.parse().ok())
        .ok_or_else(|| serde::de::Error::custom("invalid integer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tradier_quote_payload() {
        let event: StreamEvent = serde_json::from_str(r#"{"type":"quote","symbol":"SPY","bid":"500.1","ask":500.2,"bidsz":"10","asksz":11,"biddate":1720000000000,"askdate":1720000000001}"#).unwrap();
        assert_eq!(event.symbol(), "SPY");
    }
}
