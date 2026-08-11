use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::{SinkExt, Stream, StreamExt};
use reqwest::Client;
use rlean_core::{
    MarketHoursDatabase, NanosecondTimestamp, OptionRight, Resolution, TickType, TimeSpan,
};
use rlean_data_tables::{Bar, OptionUniverseRow, QuoteBar, TradeBar, TradeBarData};
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
const STREAM_SESSION_LIFETIME: Duration = Duration::from_secs(294);

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
    universe_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
            universe_worker: Mutex::new(None),
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
        if worker
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
        {
            let _ = self.shutdown.send(false);
            *worker = Some(tokio::spawn(run_feed(
                self.config.clone(),
                self.client.clone(),
                self.subscriptions.clone(),
                self.connected.clone(),
                self.event_tx.clone(),
                self.shutdown.subscribe(),
            )));
        }
        let mut universe_worker = self.universe_worker.lock().await;
        if universe_worker
            .as_ref()
            .is_none_or(tokio::task::JoinHandle::is_finished)
        {
            *universe_worker = Some(tokio::spawn(run_option_universes(
                self.config.clone(),
                self.client.clone(),
                self.subscriptions.clone(),
                self.event_tx.clone(),
                self.shutdown.subscribe(),
            )));
        }
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        let _ = self.shutdown.send(true);
        if let Some(worker) = self.worker.lock().await.take() {
            worker.await.context("join Tradier live worker")?;
        }
        if let Some(worker) = self.universe_worker.lock().await.take() {
            worker
                .await
                .context("join Tradier option-universe worker")?;
        }
        self.connected.store(false, Ordering::Release);
        Ok(())
    }

    async fn subscribe(&self, subscription: LiveSubscription) -> Result<()> {
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

/// Discover concrete option contracts through Tradier REST, then let the
/// existing websocket worker subscribe to the selected contracts after the
/// engine applies the universe membership.
async fn run_option_universes(
    config: Arc<TradierMarketDataConfig>,
    client: Client,
    subscriptions: Arc<RwLock<HashMap<u64, LiveSubscription>>>,
    events: mpsc::Sender<Result<LiveDataEvent>>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut next_refresh = HashMap::<u64, tokio::time::Instant>::new();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.changed() => if *shutdown.borrow() { return; },
            _ = interval.tick() => {
                let current = subscriptions
                    .read()
                    .await
                    .values()
                    .filter(|subscription| subscription.configuration.option_chain.is_some())
                    .cloned()
                    .collect::<Vec<_>>();
                next_refresh.retain(|id, _| current.iter().any(|subscription| subscription.id == *id));

                for subscription in current {
                    let now = tokio::time::Instant::now();
                    if next_refresh
                        .get(&subscription.id)
                        .is_some_and(|deadline| *deadline > now)
                    {
                        continue;
                    }
                    next_refresh.insert(subscription.id, now + Duration::from_secs(600));

                    match fetch_option_universe(&config, &client, &subscription).await {
                        Ok(rows) => {
                            let contracts = rows.iter().filter(|row| row.expiration.is_some()).count();
                            if events
                                .send(Ok(LiveDataEvent::Data {
                                    subscription_id: subscription.id,
                                    data: HistoricalData::OptionUniverse(rows),
                                }))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            tracing::info!(
                                subscription_id = subscription.id,
                                contracts,
                                "published Tradier option-universe membership"
                            );
                        }
                        Err(error) => tracing::warn!(
                            subscription_id = subscription.id,
                            error = %format!("{error:#}"),
                            "Tradier option-universe refresh failed"
                        ),
                    }
                }
            }
        }
    }
}

async fn fetch_option_universe(
    config: &TradierMarketDataConfig,
    client: &Client,
    subscription: &LiveSubscription,
) -> Result<Vec<OptionUniverseRow>> {
    let chain = subscription
        .configuration
        .option_chain
        .as_ref()
        .context("Tradier option-universe subscription is missing chain metadata")?;
    let underlying = chain.underlying_ticker.trim().to_ascii_uppercase();
    let underlying_symbol = subscription
        .configuration
        .symbol
        .underlying
        .as_deref()
        .unwrap_or(&subscription.configuration.symbol);
    let exchange_hours = MarketHoursDatabase::global().exchange_hours(underlying_symbol);
    let mut selection_date = chrono::Utc::now()
        .with_timezone(&chrono_tz::America::New_York)
        .date_naive();
    while exchange_hours.session_bounds(selection_date).is_none() {
        selection_date = selection_date
            .succ_opt()
            .context("Tradier option selection date overflow")?;
    }
    let mut source_date = selection_date
        .pred_opt()
        .context("Tradier option source date overflow")?;
    while exchange_hours.session_bounds(source_date).is_none() {
        source_date = source_date
            .pred_opt()
            .context("Tradier option source date overflow")?;
    }

    let expirations = option_expirations(config, client, &underlying)
        .await?
        .into_iter()
        .filter(|expiration| {
            let days = (*expiration - selection_date).num_days();
            days >= i64::from(chain.filter.min_expiry_days)
                && days <= i64::from(chain.filter.max_expiry_days)
        })
        .collect::<Vec<_>>();
    let underlying_quote = quotes(config, client, std::slice::from_ref(&underlying))
        .await?
        .into_iter()
        .find(|quote| quote.symbol.eq_ignore_ascii_case(&underlying))
        .with_context(|| format!("Tradier returned no quote for {underlying}"))?;
    let underlying_price = quote_mark(&underlying_quote)
        .with_context(|| format!("Tradier returned no positive price for {underlying}"))?;

    let market = subscription
        .configuration
        .symbol
        .market()
        .as_str()
        .to_string();
    let mut rows = vec![OptionUniverseRow {
        date: source_date,
        market: market.clone(),
        security_type: underlying_symbol.security_type().to_string(),
        symbol_sid: underlying_symbol.id.sid.to_string(),
        symbol_value: underlying.clone(),
        underlying_sid: None,
        underlying_value: None,
        expiration: None,
        strike: None,
        right: None,
        open: positive_decimal(underlying_quote.open).unwrap_or(underlying_price),
        high: positive_decimal(underlying_quote.high).unwrap_or(underlying_price),
        low: positive_decimal(underlying_quote.low).unwrap_or(underlying_price),
        close: underlying_price,
        volume: Decimal::from(underlying_quote.volume.max(0)),
        open_interest: None,
        implied_volatility: None,
        delta: None,
        gamma: None,
        vega: None,
        theta: None,
        rho: None,
    }];

    for expiration in expirations {
        for quote in option_chain(config, client, &underlying, expiration).await? {
            let Some(contract) = parse_tradier_option(&quote.symbol) else {
                continue;
            };
            let mark = quote_mark(&quote).unwrap_or_default();
            rows.push(OptionUniverseRow {
                date: source_date,
                market: market.clone(),
                security_type: "Option".to_string(),
                symbol_sid: quote.symbol.clone(),
                symbol_value: quote.symbol.to_ascii_uppercase(),
                underlying_sid: Some(underlying_symbol.id.sid.to_string()),
                underlying_value: Some(underlying.clone()),
                expiration: Some(contract.expiration),
                strike: Some(contract.strike),
                right: Some(contract.right.to_string()),
                open: positive_decimal(quote.open).unwrap_or(mark),
                high: positive_decimal(quote.high).unwrap_or(mark),
                low: positive_decimal(quote.low).unwrap_or(mark),
                close: mark,
                volume: Decimal::from(quote.volume.max(0)),
                open_interest: Some(Decimal::from(quote.open_interest.max(0))),
                implied_volatility: None,
                delta: None,
                gamma: None,
                vega: None,
                theta: None,
                rho: None,
            });
        }
    }

    // <DIV> LEAN keeps one daily option membership. rlean refreshes Tradier's
    // narrow live membership every ten minutes so an intraday move cannot move
    // every selected strike away from the underlying, matching the prior
    // production sidecar behavior.
    Ok(rows)
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
    while !subscriptions
        .read()
        .await
        .values()
        .any(|subscription| subscription.configuration.option_chain.is_none())
    {
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
    let mut received_equity_data = false;
    let mut received_option_data = false;
    let mut daily_bars = HashMap::<u64, DailyBarState>::new();
    let mut session_acquired = std::time::Instant::now();
    let mut refresh = tokio::time::interval(Duration::from_millis(500));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = refresh.tick() => {
                flush_completed_daily(&mut daily_bars, events).await?;
                let mut symbols = subscriptions.read().await.values()
                    .filter(|subscription| subscription.configuration.option_chain.is_none())
                    .map(|subscription| subscription.configuration.symbol.permtick.to_ascii_uppercase())
                    .collect::<Vec<_>>();
                symbols.sort();
                symbols.dedup();
                if symbols != last_symbols && !symbols.is_empty() {
                    if session_acquired.elapsed() >= STREAM_SESSION_LIFETIME {
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
                    let option_contracts = symbols
                        .iter()
                        .filter(|symbol| parse_tradier_option(symbol).is_some())
                        .count();
                    tracing::info!(
                        symbols = symbols.len(),
                        option_contracts,
                        "updated Tradier websocket subscriptions"
                    );
                    last_symbols = symbols;
                }
            }
            _ = shutdown.changed() => if *shutdown.borrow() { return Ok(()); },
            message = socket.next() => {
                let message = message.context("Tradier websocket closed")??;
                match message {
                    Message::Text(text) => {
                        let published = publish(&text, subscriptions, events, &mut daily_bars).await?;
                        log_first_live_data(
                            published,
                            &mut received_equity_data,
                            &mut received_option_data,
                        );
                    },
                    Message::Binary(bytes) => if let Ok(text) = std::str::from_utf8(&bytes) {
                        let published = publish(text, subscriptions, events, &mut daily_bars).await?;
                        log_first_live_data(
                            published,
                            &mut received_equity_data,
                            &mut received_option_data,
                        );
                    },
                    Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                    Message::Close(frame) => bail!("Tradier websocket closed: {frame:?}"),
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        }
    }
}

fn log_first_live_data(
    published: PublishedLiveData,
    received_equity_data: &mut bool,
    received_option_data: &mut bool,
) {
    if published.equity_events > 0 && !*received_equity_data {
        *received_equity_data = true;
        tracing::info!("received first Tradier equity websocket event");
    }
    if published.option_events > 0 && !*received_option_data {
        *received_option_data = true;
        tracing::info!("received first Tradier option websocket event");
    }
}

#[derive(Default)]
struct PublishedLiveData {
    equity_events: usize,
    option_events: usize,
}

async fn publish(
    text: &str,
    subscriptions: &RwLock<HashMap<u64, LiveSubscription>>,
    events: &mpsc::Sender<Result<LiveDataEvent>>,
    daily_bars: &mut HashMap<u64, DailyBarState>,
) -> Result<PublishedLiveData> {
    let mut published = PublishedLiveData::default();
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
            if let Some(data) = live_data(&subscription, &event, daily_bars)? {
                events
                    .send(Ok(LiveDataEvent::Data {
                        subscription_id: subscription.id,
                        data,
                    }))
                    .await
                    .context("Tradier live consumer closed")?;
                if parse_tradier_option(&symbol).is_some() {
                    published.option_events += 1;
                } else {
                    published.equity_events += 1;
                }
            }
        }
    }
    Ok(published)
}

fn live_data(
    subscription: &LiveSubscription,
    event: &StreamEvent,
    daily_bars: &mut HashMap<u64, DailyBarState>,
) -> Result<Option<HistoricalData>> {
    let config = &subscription.configuration;
    if config.resolution == Resolution::Daily {
        return daily_live_data(subscription, event, daily_bars);
    }
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

enum DailyBarState {
    Trade(TradeBar),
    Quote(QuoteBar),
}

async fn flush_completed_daily(
    states: &mut HashMap<u64, DailyBarState>,
    events: &mpsc::Sender<Result<LiveDataEvent>>,
) -> Result<()> {
    let now = NanosecondTimestamp::from_millis(chrono::Utc::now().timestamp_millis());
    let completed = states
        .iter()
        .filter_map(|(id, state)| {
            let end = match state {
                DailyBarState::Trade(bar) => bar.end_time,
                DailyBarState::Quote(bar) => bar.end_time,
            };
            (end <= now).then_some(*id)
        })
        .collect::<Vec<_>>();
    for subscription_id in completed {
        let data = match states.remove(&subscription_id) {
            Some(DailyBarState::Trade(bar)) => HistoricalData::TradeBars(vec![bar]),
            Some(DailyBarState::Quote(bar)) => HistoricalData::QuoteBars(vec![bar]),
            None => continue,
        };
        events
            .send(Ok(LiveDataEvent::Data {
                subscription_id,
                data,
            }))
            .await
            .context("Tradier live consumer closed")?;
    }
    Ok(())
}

fn daily_live_data(
    subscription: &LiveSubscription,
    event: &StreamEvent,
    states: &mut HashMap<u64, DailyBarState>,
) -> Result<Option<HistoricalData>> {
    let config = &subscription.configuration;
    let timestamp_ms = match event {
        StreamEvent::Trade { date, .. } | StreamEvent::Tradex { date, .. } => *date,
        StreamEvent::Timesale { date, .. } => *date,
        StreamEvent::Quote {
            biddate, askdate, ..
        } => (*biddate).max(*askdate),
        StreamEvent::Other => return Ok(None),
    };
    let time = NanosecondTimestamp::from_millis(timestamp_ms);
    let hours = MarketHoursDatabase::global().exchange_hours(&config.symbol);
    if !config.extended_market_hours && !hours.is_open_at(time) {
        return Ok(None);
    }
    let timezone = hours
        .timezone
        .parse()
        .context("invalid exchange timezone")?;
    let date = time.to_tz(timezone).date_naive();
    let Some((start, close)) = hours.session_bounds(date) else {
        return Ok(None);
    };
    let period = close - start;

    let next = match (config.tick_type, event) {
        (
            TickType::Trade,
            StreamEvent::Trade { price, size, .. } | StreamEvent::Tradex { price, size, .. },
        ) => {
            let Some(price) = Decimal::from_f64(*price) else {
                return Ok(None);
            };
            if let Some(DailyBarState::Trade(bar)) = states.get_mut(&subscription.id) {
                if bar.time == start {
                    bar.high = bar.high.max(price);
                    bar.low = bar.low.min(price);
                    bar.close = price;
                    bar.volume += Decimal::from(*size);
                    return Ok(None);
                }
            }
            DailyBarState::Trade(
                TradeBar::new(
                    config.symbol.clone(),
                    start,
                    period,
                    TradeBarData::new(price, price, price, price, Decimal::from(*size)),
                )
                .with_venue("tradier"),
            )
        }
        (
            TickType::Trade,
            StreamEvent::Timesale {
                last,
                size,
                cancel,
                correction,
                ..
            },
        ) if !cancel && !correction => {
            let Some(price) = Decimal::from_f64(*last) else {
                return Ok(None);
            };
            if let Some(DailyBarState::Trade(bar)) = states.get_mut(&subscription.id) {
                if bar.time == start {
                    bar.high = bar.high.max(price);
                    bar.low = bar.low.min(price);
                    bar.close = price;
                    bar.volume += Decimal::from(*size);
                    return Ok(None);
                }
            }
            DailyBarState::Trade(
                TradeBar::new(
                    config.symbol.clone(),
                    start,
                    period,
                    TradeBarData::new(price, price, price, price, Decimal::from(*size)),
                )
                .with_venue("tradier"),
            )
        }
        (
            TickType::Quote,
            StreamEvent::Quote {
                bid,
                ask,
                bidsz,
                asksz,
                ..
            },
        ) => {
            let (Some(bid), Some(ask)) = (Decimal::from_f64(*bid), Decimal::from_f64(*ask)) else {
                return Ok(None);
            };
            if let Some(DailyBarState::Quote(bar)) = states.get_mut(&subscription.id) {
                if bar.time == start {
                    if let Some(side) = bar.bid.as_mut() {
                        side.update(bid);
                    }
                    if let Some(side) = bar.ask.as_mut() {
                        side.update(ask);
                    }
                    bar.last_bid_size = Decimal::from(*bidsz);
                    bar.last_ask_size = Decimal::from(*asksz);
                    return Ok(None);
                }
            }
            DailyBarState::Quote(
                QuoteBar::new(
                    config.symbol.clone(),
                    start,
                    period,
                    Some(Bar::from_price(bid)),
                    Some(Bar::from_price(ask)),
                    Decimal::from(*bidsz),
                    Decimal::from(*asksz),
                )
                .with_venue("tradier"),
            )
        }
        _ => return Ok(None),
    };

    Ok(match states.insert(subscription.id, next) {
        Some(DailyBarState::Trade(bar)) => Some(HistoricalData::TradeBars(vec![bar])),
        Some(DailyBarState::Quote(bar)) => Some(HistoricalData::QuoteBars(vec![bar])),
        None => None,
    })
}

fn bucket_time(milliseconds: i64, period: TimeSpan) -> NanosecondTimestamp {
    let timestamp = milliseconds.saturating_mul(1_000_000);
    if period.nanos <= 0 {
        NanosecondTimestamp(timestamp)
    } else {
        NanosecondTimestamp(timestamp.div_euclid(period.nanos) * period.nanos)
    }
}

async fn option_expirations(
    config: &TradierMarketDataConfig,
    client: &Client,
    underlying: &str,
) -> Result<Vec<chrono::NaiveDate>> {
    let response = client
        .get(format!(
            "{}/markets/options/expirations",
            config.base_url.trim_end_matches('/')
        ))
        .bearer_auth(&config.access_token)
        .header("Accept", "application/json")
        .query(&[("symbol", underlying), ("includeAllRoots", "true")])
        .send()
        .await?;
    let response = checked_response(response, "option expirations request").await?;
    let container: ExpirationContainer = response.json().await?;
    let values: Vec<String> =
        normalize_optional_list(container.expirations.map(|expirations| expirations.date))?;
    let mut dates = values
        .into_iter()
        .map(|value| {
            chrono::NaiveDate::parse_from_str(&value, "%Y-%m-%d")
                .with_context(|| format!("Tradier returned invalid option expiration {value}"))
        })
        .collect::<Result<Vec<_>>>()?;
    dates.sort_unstable();
    dates.dedup();
    Ok(dates)
}

async fn option_chain(
    config: &TradierMarketDataConfig,
    client: &Client,
    underlying: &str,
    expiration: chrono::NaiveDate,
) -> Result<Vec<TradierQuote>> {
    let expiration = expiration.format("%Y-%m-%d").to_string();
    let response = client
        .get(format!(
            "{}/markets/options/chains",
            config.base_url.trim_end_matches('/')
        ))
        .bearer_auth(&config.access_token)
        .header("Accept", "application/json")
        .query(&[
            ("symbol", underlying),
            ("expiration", expiration.as_str()),
            ("greeks", "false"),
        ])
        .send()
        .await?;
    let response = checked_response(response, "option chain request").await?;
    let container: OptionChainContainer = response.json().await?;
    normalize_optional_list(container.options.map(|options| options.option))
}

async fn quotes(
    config: &TradierMarketDataConfig,
    client: &Client,
    symbols: &[String],
) -> Result<Vec<TradierQuote>> {
    if symbols.is_empty() {
        return Ok(Vec::new());
    }
    let response = client
        .get(format!(
            "{}/markets/quotes",
            config.base_url.trim_end_matches('/')
        ))
        .bearer_auth(&config.access_token)
        .header("Accept", "application/json")
        .query(&[
            ("symbols", symbols.join(",")),
            ("greeks", "false".to_string()),
        ])
        .send()
        .await?;
    let response = checked_response(response, "quote request").await?;
    let container: QuoteContainer = response.json().await?;
    normalize_optional_list(container.quotes.map(|quotes| quotes.quote))
}

async fn checked_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    bail!(
        "Tradier {operation} failed with HTTP {status}: {}",
        response.text().await.unwrap_or_default()
    )
}

fn normalize_optional_list<T: serde::de::DeserializeOwned>(value: Option<Value>) -> Result<Vec<T>> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(values)) => Ok(serde_json::from_value(Value::Array(values))?),
        Some(Value::Object(object)) => Ok(vec![serde_json::from_value(Value::Object(object))?]),
        Some(other) => bail!("expected Tradier object, array, or null, got {other}"),
    }
}

fn positive_decimal(value: f64) -> Option<Decimal> {
    Decimal::from_f64(value).filter(|value| *value > Decimal::ZERO)
}

fn quote_mark(quote: &TradierQuote) -> Option<Decimal> {
    positive_decimal(quote.last).or_else(|| {
        let bid = positive_decimal(quote.bid)?;
        let ask = positive_decimal(quote.ask)?;
        Some((bid + ask) / Decimal::TWO)
    })
}

struct ParsedTradierOption {
    expiration: chrono::NaiveDate,
    strike: Decimal,
    right: OptionRight,
}

fn parse_tradier_option(value: &str) -> Option<ParsedTradierOption> {
    let value = value.trim().to_ascii_uppercase();
    let suffix_start = value.len().checked_sub(15)?;
    let suffix = &value[suffix_start..];
    if suffix_start == 0
        || !suffix[..6]
            .chars()
            .all(|character| character.is_ascii_digit())
        || !suffix[7..]
            .chars()
            .all(|character| character.is_ascii_digit())
    {
        return None;
    }
    let expiration = chrono::NaiveDate::parse_from_str(&suffix[..6], "%y%m%d").ok()?;
    let right = match &suffix[6..7] {
        "C" => OptionRight::Call,
        "P" => OptionRight::Put,
        _ => return None,
    };
    let strike = Decimal::from_i128_with_scale(suffix[7..].parse().ok()?, 3);
    Some(ParsedTradierOption {
        expiration,
        strike,
        right,
    })
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
        // Tradier rejects an otherwise valid empty POST unless the zero-length
        // entity is explicit (HTTP 411 from the production gateway).
        .header(reqwest::header::CONTENT_LENGTH, "0")
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

#[derive(Debug, Clone, Default, Deserialize)]
struct TradierQuote {
    symbol: String,
    #[serde(default, deserialize_with = "null_default")]
    last: f64,
    #[serde(default, deserialize_with = "null_default")]
    bid: f64,
    #[serde(default, deserialize_with = "null_default")]
    ask: f64,
    #[serde(default, deserialize_with = "null_default")]
    open: f64,
    #[serde(default, deserialize_with = "null_default")]
    high: f64,
    #[serde(default, deserialize_with = "null_default")]
    low: f64,
    #[serde(default, deserialize_with = "null_default")]
    volume: i64,
    #[serde(default, deserialize_with = "null_default")]
    open_interest: i64,
}

fn null_default<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct QuoteContainer {
    quotes: Option<QuoteWrapper>,
}

#[derive(Deserialize)]
struct QuoteWrapper {
    quote: Value,
}

#[derive(Deserialize)]
struct ExpirationContainer {
    expirations: Option<ExpirationWrapper>,
}

#[derive(Deserialize)]
struct ExpirationWrapper {
    date: Value,
}

#[derive(Deserialize)]
struct OptionChainContainer {
    options: Option<OptionChainWrapper>,
}

#[derive(Deserialize)]
struct OptionChainWrapper {
    option: Value,
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
    use rlean_core::{DataNormalizationMode, Market, Symbol};
    use rlean_data::SubscriptionDataConfig;

    fn daily_subscription(tick_type: TickType) -> LiveSubscription {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut configuration = SubscriptionDataConfig::new_equity(
            symbol,
            Resolution::Daily,
            DataNormalizationMode::Raw,
        );
        configuration.set_tick_type(tick_type);
        LiveSubscription {
            id: 7,
            configuration,
        }
    }

    #[test]
    fn parses_tradier_quote_payload() {
        let event: StreamEvent = serde_json::from_str(r#"{"type":"quote","symbol":"SPY","bid":"500.1","ask":500.2,"bidsz":"10","asksz":11,"biddate":1720000000000,"askdate":1720000000001}"#).unwrap();
        assert_eq!(event.symbol(), "SPY");
    }

    #[test]
    fn parses_tradier_option_symbol() {
        let contract = parse_tradier_option("SPY260810C00635000").unwrap();
        assert_eq!(
            contract.expiration,
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10).unwrap()
        );
        assert_eq!(contract.strike, Decimal::new(635, 0));
        assert_eq!(contract.right, OptionRight::Call);
    }

    #[test]
    fn normalizes_single_and_multiple_tradier_quotes() {
        let one = normalize_optional_list::<TradierQuote>(Some(serde_json::json!({
            "symbol": "SPY",
            "last": null,
            "bid": 635.10,
            "ask": 635.12
        })))
        .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].last, 0.0);

        let many = normalize_optional_list::<TradierQuote>(Some(serde_json::json!([
            {"symbol": "SPY260810C00635000"},
            {"symbol": "SPY260810P00635000"}
        ])))
        .unwrap();
        assert_eq!(many.len(), 2);
    }

    #[tokio::test]
    async fn accepts_daily_websocket_subscription() {
        let provider = TradierLiveDataProvider::new(TradierMarketDataConfig::new(
            "test-token",
            TradierEnvironment::Live,
        ))
        .unwrap();

        provider
            .subscribe(daily_subscription(TickType::Trade))
            .await
            .unwrap();
    }

    #[test]
    fn daily_trade_events_aggregate_over_the_exchange_session() {
        let subscription = daily_subscription(TickType::Trade);
        let first = chrono::DateTime::parse_from_rfc3339("2026-08-10T15:00:00Z")
            .unwrap()
            .timestamp_millis();
        let second = chrono::DateTime::parse_from_rfc3339("2026-08-10T16:00:00Z")
            .unwrap()
            .timestamp_millis();
        let next_session = chrono::DateTime::parse_from_rfc3339("2026-08-11T15:00:00Z")
            .unwrap()
            .timestamp_millis();
        let mut states = HashMap::new();

        assert!(daily_live_data(
            &subscription,
            &StreamEvent::Trade {
                symbol: "SPY".to_owned(),
                price: 100.0,
                size: 2,
                date: first,
            },
            &mut states,
        )
        .unwrap()
        .is_none());
        assert!(daily_live_data(
            &subscription,
            &StreamEvent::Trade {
                symbol: "SPY".to_owned(),
                price: 102.0,
                size: 3,
                date: second,
            },
            &mut states,
        )
        .unwrap()
        .is_none());
        let data = daily_live_data(
            &subscription,
            &StreamEvent::Trade {
                symbol: "SPY".to_owned(),
                price: 101.0,
                size: 1,
                date: next_session,
            },
            &mut states,
        )
        .unwrap()
        .unwrap();
        let HistoricalData::TradeBars(rows) = data else {
            panic!("expected trade bars")
        };
        assert_eq!(rows[0].open, Decimal::from(100));
        assert_eq!(rows[0].high, Decimal::from(102));
        assert_eq!(rows[0].close, Decimal::from(102));
        assert_eq!(rows[0].volume, Decimal::from(5));
    }
}
