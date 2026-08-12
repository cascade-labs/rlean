use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::{SinkExt, Stream, StreamExt};
use rlean_core::{NanosecondTimestamp, Resolution, SecurityType, TickType, TimeSpan};
use rlean_data_tables::{Bar, QuoteBar, Tick, TradeBar, TradeBarData};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde_json::Value;
use tokio::sync::{mpsc, watch, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;

use crate::massive::massive_ticker;
use crate::{
    HistoricalData, HistoricalDataProvider, HistoryRequest, LiveDataEvent, LiveDataProvider,
    LiveSubscription, MassiveConfig, MassiveHistoricalDataProvider,
};

#[derive(Debug, Clone)]
pub struct MassiveLiveConfig {
    /// Credential sent to the live websocket endpoint.
    pub api_key: String,
    /// Credential used for REST-backed option-universe refreshes.
    pub historical_api_key: String,
    pub stocks_websocket_url: String,
    pub options_websocket_url: String,
    pub futures_websocket_url: String,
}

impl MassiveLiveConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        let api_key = api_key.into();
        Self {
            api_key: api_key.clone(),
            historical_api_key: api_key,
            stocks_websocket_url: "wss://socket.massive.com/stocks".to_string(),
            options_websocket_url: "wss://socket.massive.com/options".to_string(),
            futures_websocket_url: "wss://socket.massive.com/futures".to_string(),
        }
    }

    /// Route live sockets through a shared Massive-compatible relay.
    pub fn with_websocket_relay(
        mut self,
        base_url: impl AsRef<str>,
        relay_token: impl Into<String>,
    ) -> Result<Self> {
        let base_url = base_url.as_ref().trim().trim_end_matches('/');
        if base_url.is_empty() {
            bail!("Massive relay websocket base URL cannot be empty");
        }
        let relay_token = relay_token.into();
        if relay_token.trim().is_empty() {
            bail!("Massive relay token cannot be empty");
        }
        self.api_key = relay_token;
        self.stocks_websocket_url = format!("{base_url}/stocks");
        self.options_websocket_url = format!("{base_url}/options");
        self.futures_websocket_url = format!("{base_url}/futures");
        Ok(self)
    }
}

pub struct MassiveLiveDataProvider {
    config: Arc<MassiveLiveConfig>,
    subscriptions: Arc<RwLock<HashMap<u64, LiveSubscription>>>,
    connected: Arc<AtomicBool>,
    event_tx: mpsc::Sender<Result<LiveDataEvent>>,
    event_rx: Mutex<Option<mpsc::Receiver<Result<LiveDataEvent>>>>,
    shutdown: watch::Sender<bool>,
    workers: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl MassiveLiveDataProvider {
    pub fn new(config: MassiveLiveConfig) -> Result<Self> {
        if config.api_key.trim().is_empty() {
            bail!("Massive API key cannot be empty");
        }
        let (event_tx, event_rx) = mpsc::channel(4096);
        let (shutdown, _) = watch::channel(false);
        Ok(Self {
            config: Arc::new(config),
            subscriptions: Arc::new(RwLock::new(HashMap::new())),
            connected: Arc::new(AtomicBool::new(false)),
            event_tx,
            event_rx: Mutex::new(Some(event_rx)),
            shutdown,
            workers: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl LiveDataProvider for MassiveLiveDataProvider {
    fn name(&self) -> &str {
        "massive"
    }
    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
    async fn connect(&self) -> Result<()> {
        let mut workers = self.workers.lock().await;
        if workers.iter().any(|worker| !worker.is_finished()) {
            return Ok(());
        }
        workers.clear();
        let _ = self.shutdown.send(false);
        for class in [AssetClass::Stocks, AssetClass::Options, AssetClass::Futures] {
            workers.push(tokio::spawn(run_feed(
                self.config.clone(),
                class,
                self.subscriptions.clone(),
                self.connected.clone(),
                self.event_tx.clone(),
                self.shutdown.subscribe(),
            )));
        }
        workers.push(tokio::spawn(run_option_universes(
            self.config.clone(),
            self.subscriptions.clone(),
            self.event_tx.clone(),
            self.shutdown.subscribe(),
        )));
        Ok(())
    }
    async fn disconnect(&self) -> Result<()> {
        let _ = self.shutdown.send(true);
        for worker in self.workers.lock().await.drain(..) {
            worker.await.context("join Massive live worker")?;
        }
        self.connected.store(false, Ordering::Release);
        Ok(())
    }
    async fn subscribe(&self, subscription: LiveSubscription) -> Result<()> {
        if subscription.configuration.data_kind != rlean_data::SubscriptionDataKind::Market
            && subscription.configuration.option_chain.is_none()
        {
            bail!("Massive websocket accepts only market-data subscriptions");
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
        let rx = self
            .event_rx
            .lock()
            .await
            .take()
            .context("Massive event stream has already been taken")?;
        Ok(Box::pin(MpscStream(rx)))
    }
}

struct MpscStream<T>(mpsc::Receiver<T>);
impl<T> Stream for MpscStream<T> {
    type Item = T;
    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        self.0.poll_recv(cx)
    }
}

#[derive(Clone, Copy)]
enum AssetClass {
    Stocks,
    Options,
    Futures,
}
impl AssetClass {
    fn accepts(self, security_type: SecurityType) -> bool {
        match self {
            Self::Stocks => matches!(security_type, SecurityType::Equity | SecurityType::Index),
            Self::Options => matches!(
                security_type,
                SecurityType::Option | SecurityType::IndexOption | SecurityType::FutureOption
            ),
            Self::Futures => security_type == SecurityType::Future,
        }
    }
    fn url(self, config: &MassiveLiveConfig) -> &str {
        match self {
            Self::Stocks => &config.stocks_websocket_url,
            Self::Options => &config.options_websocket_url,
            Self::Futures => &config.futures_websocket_url,
        }
    }
}

async fn run_feed(
    config: Arc<MassiveLiveConfig>,
    class: AssetClass,
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
        while !has_subscriptions(class, &subscriptions).await {
            tokio::select! { _ = tokio::time::sleep(Duration::from_millis(250)) => {}, _ = shutdown.changed() => if *shutdown.borrow() { return; } }
        }
        match run_connection(
            &config,
            class,
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
                let _ = events
                    .send(Ok(LiveDataEvent::Disconnected {
                        reason: format!("{error:#}"),
                    }))
                    .await;
            }
        }
        tokio::select! { _ = tokio::time::sleep(delay) => {}, _ = shutdown.changed() => {} }
        delay = (delay * 2).min(Duration::from_secs(30));
    }
}

async fn has_subscriptions(
    class: AssetClass,
    subscriptions: &RwLock<HashMap<u64, LiveSubscription>>,
) -> bool {
    subscriptions.read().await.values().any(|sub| {
        sub.configuration.option_chain.is_none()
            && class.accepts(sub.configuration.symbol.security_type())
    })
}

async fn run_connection(
    config: &MassiveLiveConfig,
    class: AssetClass,
    subscriptions: &RwLock<HashMap<u64, LiveSubscription>>,
    connected: &AtomicBool,
    events: &mpsc::Sender<Result<LiveDataEvent>>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<()> {
    let (mut socket, _) = tokio_tungstenite::connect_async(class.url(config))
        .await
        .with_context(|| format!("connect Massive websocket {}", class.url(config)))?;
    socket
        .send(Message::Text(
            serde_json::json!({"action":"auth","params":config.api_key}).to_string(),
        ))
        .await?;
    let mut topics = HashSet::new();
    let mut refresh = tokio::time::interval(Duration::from_millis(250));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut bars = HashMap::<u64, BarState>::new();
    loop {
        tokio::select! {
            _ = refresh.tick() => {
                flush_completed(&mut bars, events).await?;
                let desired = desired_topics(class, subscriptions).await;
                let add = desired.difference(&topics).cloned().collect::<Vec<_>>();
                let remove = topics.difference(&desired).cloned().collect::<Vec<_>>();
                if !add.is_empty() { socket.send(command("subscribe", &add)).await?; }
                if !remove.is_empty() { socket.send(command("unsubscribe", &remove)).await?; }
                topics = desired;
            }
            _ = shutdown.changed() => if *shutdown.borrow() { return Ok(()); },
            incoming = socket.next() => {
                let message = incoming.context("Massive websocket closed")??;
                match message {
                    Message::Text(text) => publish(&text, class, subscriptions, events, &mut bars, connected).await?,
                    Message::Binary(bytes) => if let Ok(text) = std::str::from_utf8(&bytes) { publish(text, class, subscriptions, events, &mut bars, connected).await?; },
                    Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                    Message::Close(frame) => bail!("Massive websocket closed: {frame:?}"),
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        }
    }
}

fn command(action: &str, topics: &[String]) -> Message {
    Message::Text(serde_json::json!({"action":action,"params":topics.join(",")}).to_string())
}

async fn desired_topics(
    class: AssetClass,
    subscriptions: &RwLock<HashMap<u64, LiveSubscription>>,
) -> HashSet<String> {
    subscriptions
        .read()
        .await
        .values()
        .filter(|sub| {
            sub.configuration.option_chain.is_none()
                && class.accepts(sub.configuration.symbol.security_type())
        })
        .map(|sub| {
            let prefix = if sub.configuration.tick_type == TickType::Quote {
                "Q"
            } else {
                "T"
            };
            format!("{prefix}.{}", massive_ticker(&sub.configuration.symbol))
        })
        .collect()
}

async fn run_option_universes(
    config: Arc<MassiveLiveConfig>,
    subscriptions: Arc<RwLock<HashMap<u64, LiveSubscription>>>,
    events: mpsc::Sender<Result<LiveDataEvent>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let provider = match MassiveHistoricalDataProvider::new(MassiveConfig::new(
        config.historical_api_key.clone(),
    )) {
        Ok(provider) => provider,
        Err(error) => {
            let _ = events.send(Err(error)).await;
            return;
        }
    };
    let mut delivered = HashMap::<u64, chrono::NaiveDate>::new();
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { return; },
            _ = interval.tick() => {
                let now = NanosecondTimestamp::from_millis(chrono::Utc::now().timestamp_millis());
                let current = subscriptions.read().await.values().filter(|sub| sub.configuration.option_chain.is_some()).cloned().collect::<Vec<_>>();
                delivered.retain(|id, _| current.iter().any(|sub| sub.id == *id));
                for subscription in current {
                    let calendar_date = now.date_utc();
                    if delivered.get(&subscription.id) == Some(&calendar_date) { continue; }
                    let start = now - TimeSpan::from_days(7);
                    let request = match HistoryRequest::new(subscription.configuration.clone(), start, now) {
                        Ok(request) => request,
                        Err(error) => { let _ = events.send(Err(error)).await; continue; }
                    };
                    match provider.get_history(&request).await {
                        Ok(HistoricalData::OptionUniverse(mut rows)) => {
                            if let Some(latest) = rows.iter().map(|row| row.date).max() { rows.retain(|row| row.date == latest); }
                            if events.send(Ok(LiveDataEvent::Data { subscription_id: subscription.id, data: HistoricalData::OptionUniverse(rows) })).await.is_err() { return; }
                            delivered.insert(subscription.id, calendar_date);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            let _ = events.send(Ok(LiveDataEvent::Disconnected {
                                reason: format!("Massive option-universe refresh failed: {error:#}"),
                            })).await;
                        }
                    }
                }
            }
        }
    }
}

async fn publish(
    text: &str,
    class: AssetClass,
    subscriptions: &RwLock<HashMap<u64, LiveSubscription>>,
    events: &mpsc::Sender<Result<LiveDataEvent>>,
    bars: &mut HashMap<u64, BarState>,
    connected: &AtomicBool,
) -> Result<()> {
    let values: Vec<Value> = serde_json::from_str(text)?;
    for value in values {
        if value.get("ev").and_then(Value::as_str) == Some("status") {
            let status = value
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if status == "auth_failed" {
                bail!(
                    "Massive websocket authentication failed: {}",
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                );
            }
            if matches!(status, "auth_success" | "success") {
                connected.store(true, Ordering::Release);
                let _ = events.send(Ok(LiveDataEvent::Reconnected)).await;
            }
            continue;
        }
        let Some(symbol) = value.get("sym").and_then(Value::as_str) else {
            continue;
        };
        let subscriptions = subscriptions
            .read()
            .await
            .values()
            .filter(|sub| {
                class.accepts(sub.configuration.symbol.security_type())
                    && massive_ticker(&sub.configuration.symbol).eq_ignore_ascii_case(symbol)
            })
            .cloned()
            .collect::<Vec<_>>();
        for subscription in subscriptions {
            if let Some(data) = convert_event(&subscription, &value, bars)? {
                events
                    .send(Ok(LiveDataEvent::Data {
                        subscription_id: subscription.id,
                        data,
                    }))
                    .await
                    .context("Massive live consumer closed")?;
            }
        }
    }
    Ok(())
}

enum BarState {
    Trade(TradeBar),
    Quote(QuoteBar),
}

async fn flush_completed(
    states: &mut HashMap<u64, BarState>,
    events: &mpsc::Sender<Result<LiveDataEvent>>,
) -> Result<()> {
    let now = NanosecondTimestamp::from_millis(chrono::Utc::now().timestamp_millis());
    let completed = states
        .iter()
        .filter_map(|(id, state)| {
            let end = match state {
                BarState::Trade(bar) => bar.end_time,
                BarState::Quote(bar) => bar.end_time,
            };
            (end <= now).then_some(*id)
        })
        .collect::<Vec<_>>();
    for subscription_id in completed {
        let data = match states.remove(&subscription_id) {
            Some(BarState::Trade(bar)) => HistoricalData::TradeBars(vec![bar]),
            Some(BarState::Quote(bar)) => HistoricalData::QuoteBars(vec![bar]),
            None => continue,
        };
        events
            .send(Ok(LiveDataEvent::Data {
                subscription_id,
                data,
            }))
            .await
            .context("Massive live consumer closed")?;
    }
    Ok(())
}

fn convert_event(
    subscription: &LiveSubscription,
    value: &Value,
    states: &mut HashMap<u64, BarState>,
) -> Result<Option<HistoricalData>> {
    let config = &subscription.configuration;
    let event_type = value.get("ev").and_then(Value::as_str).unwrap_or_default();
    let timestamp_ms = value
        .get("t")
        .and_then(Value::as_i64)
        .or_else(|| value.get("s").and_then(Value::as_i64))
        .unwrap_or_default();
    let time = NanosecondTimestamp::from_millis(timestamp_ms);
    if !config.extended_market_hours
        && !rlean_core::MarketHoursDatabase::global()
            .exchange_hours(&config.symbol)
            .is_open_at(time)
    {
        return Ok(None);
    }
    if config.resolution == Resolution::Tick {
        let tick = match event_type {
            "T" => {
                let mut tick = Tick::trade(
                    config.symbol.clone(),
                    time,
                    number(value, "p")?,
                    number(value, "s")?,
                )
                .with_venue("massive");
                tick.exchange = value.get("x").map(ToString::to_string);
                tick
            }
            "Q" => {
                let mut tick = Tick::quote(
                    config.symbol.clone(),
                    time,
                    number(value, "bp")?,
                    number(value, "ap")?,
                    number(value, "bs")?,
                    number(value, "as")?,
                )
                .with_venue("massive");
                tick.exchange = value.get("bx").map(ToString::to_string);
                tick
            }
            _ => return Ok(None),
        };
        return Ok(Some(HistoricalData::Ticks(vec![tick])));
    }
    let requested_period = config
        .resolution
        .to_time_span()
        .context("Massive live bars require fixed resolution")?;
    let (start, period) = if config.resolution == Resolution::Daily {
        let hours = rlean_core::MarketHoursDatabase::global().exchange_hours(&config.symbol);
        let timezone = hours
            .timezone
            .parse()
            .context("invalid exchange timezone")?;
        let date = time.to_tz(timezone).date_naive();
        let Some((open, close)) = hours.session_bounds(date) else {
            return Ok(None);
        };
        (open, close - open)
    } else {
        (bucket(time, requested_period), requested_period)
    };
    match (event_type, states.get_mut(&subscription.id)) {
        ("T", Some(BarState::Trade(bar))) if bar.time == start => {
            let price = number(value, "p")?;
            bar.high = bar.high.max(price);
            bar.low = bar.low.min(price);
            bar.close = price;
            bar.volume += number(value, "s")?;
            return Ok(None);
        }
        ("Q", Some(BarState::Quote(bar))) if bar.time == start => {
            let bid = number(value, "bp")?;
            let ask = number(value, "ap")?;
            if let Some(side) = bar.bid.as_mut() {
                side.update(bid);
            }
            if let Some(side) = bar.ask.as_mut() {
                side.update(ask);
            }
            bar.last_bid_size = number(value, "bs")?;
            bar.last_ask_size = number(value, "as")?;
            return Ok(None);
        }
        ("T", _) | ("Q", _) => {}
        _ => return Ok(None),
    }
    let next = if event_type == "T" {
        let price = number(value, "p")?;
        BarState::Trade(
            TradeBar::new(
                config.symbol.clone(),
                start,
                period,
                TradeBarData::new(price, price, price, price, number(value, "s")?),
            )
            .with_venue("massive"),
        )
    } else {
        let bid = number(value, "bp")?;
        let ask = number(value, "ap")?;
        BarState::Quote(
            QuoteBar::new(
                config.symbol.clone(),
                start,
                period,
                Some(Bar::from_price(bid)),
                Some(Bar::from_price(ask)),
                number(value, "bs")?,
                number(value, "as")?,
            )
            .with_venue("massive"),
        )
    };
    Ok(match states.insert(subscription.id, next) {
        Some(BarState::Trade(bar)) => Some(HistoricalData::TradeBars(vec![bar])),
        Some(BarState::Quote(bar)) => Some(HistoricalData::QuoteBars(vec![bar])),
        None => None,
    })
}

fn number(value: &Value, key: &str) -> Result<Decimal> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .and_then(Decimal::from_f64)
        .with_context(|| format!("Massive event missing numeric {key}"))
}
fn bucket(time: NanosecondTimestamp, period: TimeSpan) -> NanosecondTimestamp {
    NanosecondTimestamp(time.0.div_euclid(period.nanos) * period.nanos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rlean_core::{
        DataNormalizationMode, Market, OptionRight, OptionStyle, Symbol, SymbolOptionsExt,
    };
    use rlean_data::SubscriptionDataConfig;

    fn subscription(tick_type: TickType, resolution: Resolution) -> LiveSubscription {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut configuration =
            SubscriptionDataConfig::new_equity(symbol, resolution, DataNormalizationMode::Raw);
        configuration.set_tick_type(tick_type);
        configuration.extended_market_hours = true;
        LiveSubscription {
            id: 7,
            configuration,
        }
    }

    #[test]
    fn formats_massive_osi_option_ticker() {
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let option = Symbol::create_option_osi(
            underlying,
            Decimal::new(500, 0),
            NaiveDate::from_ymd_opt(2026, 8, 4).unwrap(),
            OptionRight::Call,
            OptionStyle::American,
            &Market::usa(),
        );
        assert_eq!(massive_ticker(&option), "O:SPY260804C00500000");
    }

    #[test]
    fn relay_changes_only_live_websocket_credentials_and_urls() {
        let config = MassiveLiveConfig::new("upstream-key")
            .with_websocket_relay("ws://127.0.0.1:8190/", "relay-token")
            .unwrap();
        assert_eq!(config.api_key, "relay-token");
        assert_eq!(config.historical_api_key, "upstream-key");
        assert_eq!(config.stocks_websocket_url, "ws://127.0.0.1:8190/stocks");
        assert_eq!(config.options_websocket_url, "ws://127.0.0.1:8190/options");
        assert_eq!(config.futures_websocket_url, "ws://127.0.0.1:8190/futures");
    }

    #[test]
    fn tick_subscription_emits_canonical_trade_tick() {
        let mut states = HashMap::new();
        let data = convert_event(
            &subscription(TickType::Trade, Resolution::Tick),
            &serde_json::json!({
                "ev":"T", "sym":"SPY", "p":500.25, "s":3.0, "t":1_000_i64, "x":4
            }),
            &mut states,
        )
        .unwrap()
        .unwrap();
        let HistoricalData::Ticks(rows) = data else {
            panic!("expected ticks")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].value, Decimal::new(50025, 2));
        assert_eq!(rows[0].venue.as_deref(), Some("massive"));
    }

    #[test]
    fn minute_trade_bar_is_emitted_only_after_bucket_changes() {
        let subscription = subscription(TickType::Trade, Resolution::Minute);
        let mut states = HashMap::new();
        assert!(convert_event(
            &subscription,
            &serde_json::json!({"ev":"T","p":10.0,"s":2.0,"t":1_000_i64}),
            &mut states
        )
        .unwrap()
        .is_none());
        assert!(convert_event(
            &subscription,
            &serde_json::json!({"ev":"T","p":11.0,"s":3.0,"t":2_000_i64}),
            &mut states
        )
        .unwrap()
        .is_none());
        let data = convert_event(
            &subscription,
            &serde_json::json!({"ev":"T","p":12.0,"s":1.0,"t":61_000_i64}),
            &mut states,
        )
        .unwrap()
        .unwrap();
        let HistoricalData::TradeBars(rows) = data else {
            panic!("expected trade bars")
        };
        assert_eq!(rows[0].open, Decimal::from(10));
        assert_eq!(rows[0].close, Decimal::from(11));
        assert_eq!(rows[0].volume, Decimal::from(5));
    }
}
