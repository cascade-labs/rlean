use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::{Client, StatusCode, Url};
use rlean_core::{
    DateTime, MarketHoursDatabase, NanosecondTimestamp, Resolution, SecurityType, Symbol, TickType,
    TimeSpan,
};
use rlean_data::SubscriptionDataKind;
use rlean_data_tables::{
    Bar, FactorFileEntry, MapFileEntry, OptionUniverseRow, QuoteBar, Tick, TradeBar,
};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::{HistoricalData, HistoricalDataProvider, HistoryRequest};

const DEFAULT_BASE_URL: &str = "https://api.massive.com";
const DEFAULT_REQUESTS_PER_SECOND: f64 = 5.0;
const MAX_RETRIES: usize = 5;
const MARKET_DATA_START: &str = "2003-09-10";
const AUXILIARY_SENTINEL: &str = "2050-12-31";

#[derive(Debug, Clone)]
pub struct MassiveConfig {
    pub api_key: String,
    pub base_url: String,
    pub requests_per_second: f64,
    pub timeout: Duration,
}

impl MassiveConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            requests_per_second: DEFAULT_REQUESTS_PER_SECOND,
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Clone)]
pub struct MassiveHistoricalDataProvider {
    client: Client,
    config: Arc<MassiveConfig>,
    limiter: Arc<RateLimiter>,
}

struct RateLimiter {
    interval: Duration,
    next_allowed: Mutex<Instant>,
}

impl RateLimiter {
    fn new(requests_per_second: f64) -> Self {
        let requests_per_second = if requests_per_second.is_finite() && requests_per_second > 0.0 {
            requests_per_second
        } else {
            DEFAULT_REQUESTS_PER_SECOND
        };
        Self {
            interval: Duration::from_secs_f64(1.0 / requests_per_second),
            next_allowed: Mutex::new(Instant::now()),
        }
    }

    async fn wait(&self) {
        let delay = {
            let mut next = self.next_allowed.lock().await;
            let now = Instant::now();
            let allowed = (*next).max(now);
            *next = allowed + self.interval;
            allowed.saturating_duration_since(now)
        };
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
}

impl MassiveHistoricalDataProvider {
    pub fn new(config: MassiveConfig) -> Result<Self> {
        if config.api_key.trim().is_empty() {
            bail!("Massive API key cannot be empty");
        }
        let client = Client::builder()
            .timeout(config.timeout)
            .pool_max_idle_per_host(8)
            .build()
            .context("build Massive HTTP client")?;
        let limiter = Arc::new(RateLimiter::new(config.requests_per_second));
        Ok(Self {
            client,
            config: Arc::new(config),
            limiter,
        })
    }

    async fn trade_bars(&self, request: &HistoryRequest) -> Result<Vec<TradeBar>> {
        let (timespan, period) = resolution(request.configuration.resolution)?;
        let ticker = &request.configuration.symbol.permtick;
        let mut url = Url::parse(&format!(
            "{}/v2/aggs/ticker/{ticker}/range/1/{timespan}/{}/{}",
            self.config.base_url.trim_end_matches('/'),
            request.range.start.as_millis(),
            request.range.end.as_millis()
        ))?;
        url.query_pairs_mut()
            .append_pair("adjusted", "false")
            .append_pair("sort", "asc")
            .append_pair("limit", "50000");

        let mut rows: Vec<Aggregate> = self.paginated(url).await?;
        rows.sort_by_key(|row| row.timestamp_ms);
        let exchange_hours =
            MarketHoursDatabase::global().exchange_hours(&request.configuration.symbol);
        let exchange_timezone = exchange_hours
            .timezone
            .parse()
            .with_context(|| format!("invalid exchange timezone {}", exchange_hours.timezone))?;
        rows.into_iter()
            .filter_map(|row| {
                let raw_time = DateTime::from_millis(row.timestamp_ms);
                let (time, end_time, period) =
                    if request.configuration.resolution == Resolution::Daily {
                        let date = raw_time.to_tz(exchange_timezone).date_naive();
                        let (open, close) = exchange_hours.session_bounds(date)?;
                        (open, close, close - open)
                    } else {
                        (raw_time, raw_time + period, period)
                    };
                (end_time > request.range.start && end_time <= request.range.end).then(|| {
                    Ok(TradeBar {
                        symbol: request.configuration.symbol.clone(),
                        venue: Some(request.configuration.venue.clone()),
                        time,
                        end_time,
                        open: decimal(row.open)?,
                        high: decimal(row.high)?,
                        low: decimal(row.low)?,
                        close: decimal(row.close)?,
                        volume: decimal(row.volume)?,
                        period,
                    })
                })
            })
            .collect()
    }

    async fn quote_bars(&self, request: &HistoryRequest) -> Result<Vec<QuoteBar>> {
        if request.configuration.resolution == Resolution::Tick {
            bail!("Massive QuoteBar history requires Second or coarser resolution");
        }
        let ticker = &request.configuration.symbol.permtick;
        let mut url = Url::parse(&format!(
            "{}/v3/quotes/{ticker}",
            self.config.base_url.trim_end_matches('/')
        ))?;
        url.query_pairs_mut()
            .append_pair("timestamp.gte", &request.range.start.0.to_string())
            .append_pair("timestamp.lt", &request.range.end.0.to_string())
            .append_pair("sort", "timestamp")
            .append_pair("order", "asc")
            .append_pair("limit", "50000");
        let mut quotes: Vec<Quote> = self.paginated(url).await?;
        quotes.sort_by_key(Quote::timestamp_ns);
        aggregate_quotes(request, quotes)
    }

    async fn ticks(&self, request: &HistoryRequest) -> Result<Vec<Tick>> {
        let ticker = &request.configuration.symbol.permtick;
        match request.configuration.tick_type {
            TickType::Trade => {
                let mut url = Url::parse(&format!(
                    "{}/v3/trades/{ticker}",
                    self.config.base_url.trim_end_matches('/')
                ))?;
                url.query_pairs_mut()
                    .append_pair("timestamp.gte", &request.range.start.0.to_string())
                    .append_pair("timestamp.lt", &request.range.end.0.to_string())
                    .append_pair("sort", "timestamp")
                    .append_pair("order", "asc")
                    .append_pair("limit", "50000");
                let mut trades: Vec<Trade> = self.paginated(url).await?;
                trades.sort_by_key(Trade::timestamp_ns);
                trades
                    .into_iter()
                    .filter(|row| {
                        let time = NanosecondTimestamp(row.timestamp_ns());
                        time >= request.range.start && time < request.range.end
                    })
                    .map(|row| {
                        let mut tick = Tick::trade(
                            request.configuration.symbol.clone(),
                            NanosecondTimestamp(row.timestamp_ns()),
                            decimal(row.price)?,
                            decimal(row.size)?,
                        )
                        .with_venue(request.configuration.venue.clone());
                        tick.exchange = row.exchange.map(|value| value.to_string());
                        tick.sale_condition = (!row.conditions.is_empty()).then(|| {
                            row.conditions
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                                .join(",")
                        });
                        Ok(tick)
                    })
                    .collect()
            }
            TickType::Quote => {
                let mut url = Url::parse(&format!(
                    "{}/v3/quotes/{ticker}",
                    self.config.base_url.trim_end_matches('/')
                ))?;
                url.query_pairs_mut()
                    .append_pair("timestamp.gte", &request.range.start.0.to_string())
                    .append_pair("timestamp.lt", &request.range.end.0.to_string())
                    .append_pair("sort", "timestamp")
                    .append_pair("order", "asc")
                    .append_pair("limit", "50000");
                let mut quotes: Vec<Quote> = self.paginated(url).await?;
                quotes.sort_by_key(Quote::timestamp_ns);
                quotes
                    .into_iter()
                    .filter(|row| {
                        let time = NanosecondTimestamp(row.timestamp_ns());
                        time >= request.range.start && time < request.range.end
                    })
                    .map(|row| {
                        let mut tick = Tick::quote(
                            request.configuration.symbol.clone(),
                            NanosecondTimestamp(row.timestamp_ns()),
                            decimal(row.bid_price)?,
                            decimal(row.ask_price)?,
                            decimal(row.bid_size)?,
                            decimal(row.ask_size)?,
                        )
                        .with_venue(request.configuration.venue.clone());
                        tick.exchange = match (row.bid_exchange, row.ask_exchange) {
                            (Some(bid), Some(ask)) if bid != ask => Some(format!("{bid}/{ask}")),
                            (Some(exchange), _) | (_, Some(exchange)) => Some(exchange.to_string()),
                            _ => None,
                        };
                        Ok(tick)
                    })
                    .collect()
            }
            other => bail!("Massive does not support {other:?} ticks"),
        }
    }

    async fn option_universe(&self, request: &HistoryRequest) -> Result<Vec<OptionUniverseRow>> {
        let metadata = request
            .configuration
            .option_chain
            .as_ref()
            .context("option universe request is missing chain metadata")?;
        let exchange_hours = MarketHoursDatabase::global().exchange_hours(
            request
                .configuration
                .symbol
                .underlying
                .as_deref()
                .unwrap_or(&request.configuration.symbol),
        );
        let mut date = request.range.start.date_utc();
        let final_date = request.range.end.date_utc();
        let mut rows = Vec::new();
        while date <= final_date {
            let Some((_open, close)) = exchange_hours.session_bounds(date) else {
                date = date.succ_opt().context("option-universe date overflow")?;
                continue;
            };
            if close <= request.range.start || close > request.range.end {
                date = date.succ_opt().context("option-universe date overflow")?;
                continue;
            }
            let mut url = Url::parse(&format!(
                "{}/v3/reference/options/contracts",
                self.config.base_url.trim_end_matches('/')
            ))?;
            url.query_pairs_mut()
                .append_pair("underlying_ticker", &metadata.underlying_ticker)
                .append_pair("as_of", &date.to_string())
                .append_pair("expired", "true")
                .append_pair("expiration_date.gte", &date.to_string())
                .append_pair("order", "asc")
                .append_pair("limit", "1000");
            let contracts: Vec<OptionContractReference> = self.paginated(url).await?;
            let underlying_close = self.daily_close(&metadata.underlying_ticker, date).await?;
            rows.push(OptionUniverseRow {
                date,
                market: request.configuration.symbol.market().as_str().to_string(),
                security_type: request
                    .configuration
                    .symbol
                    .underlying
                    .as_deref()
                    .map(|s| s.security_type().to_string())
                    .unwrap_or_else(|| "Equity".to_string()),
                symbol_sid: metadata.underlying_ticker.clone(),
                symbol_value: metadata.underlying_ticker.clone(),
                underlying_sid: None,
                underlying_value: None,
                expiration: None,
                strike: None,
                right: None,
                open: underlying_close,
                high: underlying_close,
                low: underlying_close,
                close: underlying_close,
                volume: Decimal::ZERO,
                open_interest: None,
                implied_volatility: None,
                delta: None,
                gamma: None,
                vega: None,
                theta: None,
                rho: None,
            });
            rows.extend(contracts.into_iter().filter_map(|contract| {
                let expiration =
                    chrono::NaiveDate::parse_from_str(&contract.expiration_date, "%Y-%m-%d")
                        .ok()?;
                let strike = Decimal::from_f64(contract.strike_price)?;
                Some(OptionUniverseRow {
                    date,
                    market: request.configuration.symbol.market().as_str().to_string(),
                    security_type: "Option".to_string(),
                    symbol_sid: contract.ticker.clone(),
                    symbol_value: contract.ticker,
                    underlying_sid: Some(metadata.underlying_ticker.clone()),
                    underlying_value: Some(metadata.underlying_ticker.clone()),
                    expiration: Some(expiration),
                    strike: Some(strike),
                    right: Some(contract.contract_type),
                    open: Decimal::ZERO,
                    high: Decimal::ZERO,
                    low: Decimal::ZERO,
                    close: Decimal::ZERO,
                    volume: Decimal::ZERO,
                    open_interest: None,
                    implied_volatility: None,
                    delta: None,
                    gamma: None,
                    vega: None,
                    theta: None,
                    rho: None,
                })
            }));
            date = date.succ_opt().context("option-universe date overflow")?;
        }
        Ok(rows)
    }

    async fn daily_close(&self, ticker: &str, date: chrono::NaiveDate) -> Result<Decimal> {
        let mut url = Url::parse(&format!(
            "{}/v2/aggs/ticker/{ticker}/range/1/day/{date}/{date}",
            self.config.base_url.trim_end_matches('/')
        ))?;
        url.query_pairs_mut()
            .append_pair("adjusted", "false")
            .append_pair("limit", "1");
        let rows: Vec<Aggregate> = self.paginated(url).await?;
        rows.first()
            .map(|row| decimal(row.close))
            .transpose()
            .map(Option::unwrap_or_default)
    }

    async fn factor_file(&self, symbol: &Symbol) -> Result<Vec<FactorFileEntry>> {
        if symbol.security_type() != SecurityType::Equity {
            return Ok(Vec::new());
        }
        let ticker = symbol.permtick.as_ref();
        let start = chrono::NaiveDate::parse_from_str(MARKET_DATA_START, "%Y-%m-%d")?;
        let end = chrono::Utc::now().date_naive();
        let bars = self.reference_daily_prices(ticker, start, end).await?;
        let mut split_url = Url::parse(&format!(
            "{}/stocks/v1/splits",
            self.config.base_url.trim_end_matches('/')
        ))?;
        split_url
            .query_pairs_mut()
            .append_pair("ticker", ticker)
            .append_pair("execution_date.gte", "1900-01-01")
            .append_pair("execution_date.lte", &end.to_string())
            .append_pair("order", "asc")
            .append_pair("limit", "1000");
        let splits: Vec<SplitRow> = self.paginated(split_url).await?;
        let mut dividend_url = Url::parse(&format!(
            "{}/stocks/v1/dividends",
            self.config.base_url.trim_end_matches('/')
        ))?;
        dividend_url
            .query_pairs_mut()
            .append_pair("ticker", ticker)
            .append_pair("ex_dividend_date.gte", "1900-01-01")
            .append_pair("ex_dividend_date.lte", &end.to_string())
            .append_pair("order", "asc")
            .append_pair("limit", "1000");
        let dividends: Vec<DividendRow> = self.paginated(dividend_url).await?;
        compute_factor_rows(splits, dividends, bars, start)
    }

    async fn reference_daily_prices(
        &self,
        ticker: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> Result<HashMap<chrono::NaiveDate, f64>> {
        let mut url = Url::parse(&format!(
            "{}/v2/aggs/ticker/{ticker}/range/1/day/{start}/{end}",
            self.config.base_url.trim_end_matches('/')
        ))?;
        url.query_pairs_mut()
            .append_pair("adjusted", "false")
            .append_pair("sort", "asc")
            .append_pair("limit", "50000");
        let rows: Vec<Aggregate> = self.paginated(url).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                (
                    DateTime::from_millis(row.timestamp_ms).date_utc(),
                    row.close,
                )
            })
            .collect())
    }

    async fn map_file(&self, symbol: &Symbol) -> Result<Vec<MapFileEntry>> {
        if symbol.security_type() != SecurityType::Equity {
            return Ok(Vec::new());
        }
        let ticker = symbol.permtick.as_ref();
        let mut details_url = Url::parse(&format!(
            "{}/v3/reference/tickers/{ticker}",
            self.config.base_url.trim_end_matches('/')
        ))?;
        details_url
            .query_pairs_mut()
            .append_pair("apiKey", &self.config.api_key);
        let details: TickerDetailsResponse = self.get_json(details_url).await?;
        let mut events_url = Url::parse(&format!(
            "{}/vX/reference/tickers/{ticker}/events",
            self.config.base_url.trim_end_matches('/')
        ))?;
        events_url
            .query_pairs_mut()
            .append_pair("types", "ticker_change")
            .append_pair("apiKey", &self.config.api_key);
        let events: TickerEventsResponse = self.get_json(events_url).await?;
        let (event_list_date, changes) =
            assemble_ticker_changes(events.results.map(|r| r.events).unwrap_or_default());
        let list_date = details
            .results
            .as_ref()
            .and_then(|v| v.list_date.as_deref())
            .and_then(parse_date)
            .or(event_list_date);
        let delisted = details
            .results
            .as_ref()
            .and_then(|v| v.delisted_utc.as_deref())
            .and_then(parse_date);
        compute_map_rows(ticker, list_date, delisted, changes)
    }

    async fn paginated<T: DeserializeOwned>(&self, first: Url) -> Result<Vec<T>> {
        let mut next = Some(first);
        let mut rows = Vec::new();
        while let Some(mut url) = next.take() {
            if !url.query_pairs().any(|(key, _)| key == "apiKey") {
                url.query_pairs_mut()
                    .append_pair("apiKey", &self.config.api_key);
            }
            let response: Paginated<T> = self.get_json(url).await?;
            if response.status.eq_ignore_ascii_case("ERROR") {
                bail!(
                    "Massive request failed: {}",
                    response
                        .error
                        .unwrap_or_else(|| "unknown error".to_string())
                );
            }
            rows.extend(response.results.unwrap_or_default());
            next = response
                .next_url
                .filter(|value| !value.is_empty())
                .map(|value| Url::parse(&value))
                .transpose()?;
        }
        Ok(rows)
    }

    async fn get_json<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let mut delay = Duration::from_millis(250);
        for attempt in 0..=MAX_RETRIES {
            self.limiter.wait().await;
            let response = self.client.get(url.clone()).send().await;
            match response {
                Ok(response) if response.status().is_success() => {
                    return response
                        .json()
                        .await
                        .with_context(|| format!("decode Massive response from {url}"));
                }
                Ok(response)
                    if (response.status() == StatusCode::TOO_MANY_REQUESTS
                        || response.status().is_server_error())
                        && attempt < MAX_RETRIES => {}
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    bail!("Massive request {url} failed with HTTP {status}: {body}");
                }
                Err(error) if attempt < MAX_RETRIES => {
                    tracing::warn!(attempt, %error, "retrying Massive request");
                }
                Err(error) => return Err(error).context("request Massive API"),
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(8));
        }
        unreachable!("Massive retry loop always returns")
    }
}

#[async_trait]
impl HistoricalDataProvider for MassiveHistoricalDataProvider {
    fn name(&self) -> &str {
        "massive"
    }

    fn supports(&self, request: &HistoryRequest) -> bool {
        (request.configuration.data_kind == SubscriptionDataKind::Market
            && matches!(
                request.configuration.symbol.security_type(),
                SecurityType::Equity | SecurityType::Option | SecurityType::Index
            )
            && matches!(
                request.configuration.tick_type,
                TickType::Trade | TickType::Quote
            ))
            || request.configuration.option_chain.is_some()
    }

    async fn get_history(&self, request: &HistoryRequest) -> Result<HistoricalData> {
        if request.configuration.option_chain.is_some() {
            return Ok(HistoricalData::OptionUniverse(
                self.option_universe(request).await?,
            ));
        }
        if request.configuration.resolution == Resolution::Tick {
            return Ok(HistoricalData::Ticks(self.ticks(request).await?));
        }
        match request.configuration.tick_type {
            TickType::Trade => Ok(HistoricalData::TradeBars(self.trade_bars(request).await?)),
            TickType::Quote => Ok(HistoricalData::QuoteBars(self.quote_bars(request).await?)),
            other => bail!("Massive does not support {other:?} history"),
        }
    }

    async fn get_factor_file(&self, symbol: &Symbol) -> Result<Option<Vec<FactorFileEntry>>> {
        if symbol.security_type() != SecurityType::Equity {
            return Ok(None);
        }
        Ok(Some(self.factor_file(symbol).await?))
    }

    async fn get_map_file(&self, symbol: &Symbol) -> Result<Option<Vec<MapFileEntry>>> {
        if symbol.security_type() != SecurityType::Equity {
            return Ok(None);
        }
        Ok(Some(self.map_file(symbol).await?))
    }
}

fn aggregate_quotes(request: &HistoryRequest, quotes: Vec<Quote>) -> Result<Vec<QuoteBar>> {
    let period = request
        .configuration
        .resolution
        .to_time_span()
        .context("QuoteBars require a fixed resolution")?;
    let exchange_hours =
        MarketHoursDatabase::global().exchange_hours(&request.configuration.symbol);
    let exchange_timezone = exchange_hours
        .timezone
        .parse()
        .with_context(|| format!("invalid exchange timezone {}", exchange_hours.timezone))?;
    let mut bars: BTreeMap<i64, QuoteAccumulator> = BTreeMap::new();
    for quote in quotes {
        let timestamp = NanosecondTimestamp(quote.timestamp_ns());
        if timestamp < request.range.start || timestamp >= request.range.end {
            continue;
        }
        let (start, end) = if request.configuration.resolution == Resolution::Daily {
            let date = timestamp.to_tz(exchange_timezone).date_naive();
            let Some((open, close)) = exchange_hours.session_bounds(date) else {
                continue;
            };
            if timestamp < open || timestamp > close {
                continue;
            }
            (open, close)
        } else {
            let start_ns = timestamp.0.div_euclid(period.nanos) * period.nanos;
            let start = NanosecondTimestamp(start_ns);
            (start, start + period)
        };
        let bid = decimal(quote.bid_price)?;
        let ask = decimal(quote.ask_price)?;
        let bid_size = decimal(quote.bid_size)?;
        let ask_size = decimal(quote.ask_size)?;
        bars.entry(start.0)
            .and_modify(|bar| bar.update(bid, ask, bid_size, ask_size))
            .or_insert_with(|| QuoteAccumulator::new(start, end, bid, ask, bid_size, ask_size));
    }
    Ok(bars
        .into_values()
        .filter(|bar| bar.end > request.range.start && bar.end <= request.range.end)
        .map(|bar| QuoteBar {
            symbol: request.configuration.symbol.clone(),
            venue: Some(request.configuration.venue.clone()),
            time: bar.start,
            end_time: bar.end,
            bid: Some(bar.bid),
            ask: Some(bar.ask),
            last_bid_size: bar.bid_size,
            last_ask_size: bar.ask_size,
            period: bar.end - bar.start,
        })
        .collect())
}

struct QuoteAccumulator {
    start: DateTime,
    end: DateTime,
    bid: Bar,
    ask: Bar,
    bid_size: Decimal,
    ask_size: Decimal,
}

impl QuoteAccumulator {
    fn new(
        start: DateTime,
        end: DateTime,
        bid: Decimal,
        ask: Decimal,
        bid_size: Decimal,
        ask_size: Decimal,
    ) -> Self {
        Self {
            start,
            end,
            bid: Bar::from_price(bid),
            ask: Bar::from_price(ask),
            bid_size,
            ask_size,
        }
    }

    fn update(&mut self, bid: Decimal, ask: Decimal, bid_size: Decimal, ask_size: Decimal) {
        self.bid.update(bid);
        self.ask.update(ask);
        self.bid_size = bid_size;
        self.ask_size = ask_size;
    }
}

fn resolution(resolution: Resolution) -> Result<(&'static str, TimeSpan)> {
    match resolution {
        Resolution::Second => Ok(("second", TimeSpan::ONE_SECOND)),
        Resolution::Minute => Ok(("minute", TimeSpan::ONE_MINUTE)),
        Resolution::Hour => Ok(("hour", TimeSpan::ONE_HOUR)),
        Resolution::Daily => Ok(("day", TimeSpan::ONE_DAY)),
        Resolution::Tick => bail!("Massive TradeBar history does not support Tick resolution"),
    }
}

fn decimal(value: f64) -> Result<Decimal> {
    Decimal::from_f64(value).context("Massive returned a non-finite decimal value")
}

#[derive(Deserialize)]
struct Paginated<T> {
    #[serde(default)]
    status: String,
    results: Option<Vec<T>>,
    next_url: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct Aggregate {
    #[serde(rename = "t")]
    timestamp_ms: i64,
    #[serde(rename = "o")]
    open: f64,
    #[serde(rename = "h")]
    high: f64,
    #[serde(rename = "l")]
    low: f64,
    #[serde(rename = "c")]
    close: f64,
    #[serde(rename = "v")]
    volume: f64,
}

#[derive(Deserialize)]
struct Quote {
    #[serde(default)]
    participant_timestamp: i64,
    #[serde(default)]
    sip_timestamp: i64,
    bid_price: f64,
    ask_price: f64,
    bid_size: f64,
    ask_size: f64,
    #[serde(default)]
    bid_exchange: Option<i64>,
    #[serde(default)]
    ask_exchange: Option<i64>,
}

#[derive(Deserialize)]
struct Trade {
    #[serde(default)]
    participant_timestamp: i64,
    #[serde(default)]
    sip_timestamp: i64,
    price: f64,
    size: f64,
    #[serde(default)]
    exchange: Option<i64>,
    #[serde(default)]
    conditions: Vec<i64>,
}

impl Trade {
    fn timestamp_ns(&self) -> i64 {
        if self.sip_timestamp != 0 {
            self.sip_timestamp
        } else {
            self.participant_timestamp
        }
    }
}

#[derive(Deserialize)]
struct OptionContractReference {
    ticker: String,
    contract_type: String,
    expiration_date: String,
    strike_price: f64,
}

#[derive(Deserialize)]
struct SplitRow {
    execution_date: String,
    split_from: f64,
    split_to: f64,
}

#[derive(Deserialize)]
struct DividendRow {
    ex_dividend_date: String,
    cash_amount: f64,
    dividend_type: Option<String>,
}

#[derive(Deserialize)]
struct TickerDetailsResponse {
    results: Option<TickerDetails>,
}
#[derive(Deserialize)]
struct TickerDetails {
    list_date: Option<String>,
    delisted_utc: Option<String>,
}
#[derive(Deserialize)]
struct TickerEventsResponse {
    results: Option<TickerEventsResults>,
}
#[derive(Deserialize)]
struct TickerEventsResults {
    #[serde(default)]
    events: Vec<TickerEvent>,
}
#[derive(Deserialize)]
struct TickerEvent {
    date: String,
    #[serde(rename = "type")]
    event_type: String,
    ticker_change: TickerChange,
}
#[derive(Deserialize)]
struct TickerChange {
    ticker: String,
}

#[derive(Debug)]
struct TickerChangeEvent {
    effective_date: chrono::NaiveDate,
    old_ticker: String,
}

fn parse_date(value: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(value.get(..10).unwrap_or(value), "%Y-%m-%d").ok()
}

fn compute_factor_rows(
    splits: Vec<SplitRow>,
    dividends: Vec<DividendRow>,
    reference_prices: HashMap<chrono::NaiveDate, f64>,
    start: chrono::NaiveDate,
) -> Result<Vec<FactorFileEntry>> {
    enum Event {
        Split {
            date: chrono::NaiveDate,
            factor: f64,
        },
        Dividend {
            date: chrono::NaiveDate,
            amount: f64,
        },
    }
    let mut events = Vec::new();
    for row in splits {
        if let Some(date) = parse_date(&row.execution_date).and_then(|date| date.pred_opt()) {
            if row.split_to != 0.0 {
                let factor = row.split_from / row.split_to;
                if factor.is_finite() && factor != 0.0 {
                    events.push(Event::Split { date, factor });
                }
            }
        }
    }
    for row in dividends {
        if matches!(row.dividend_type.as_deref(), None | Some("CD") | Some("SC"))
            && row.cash_amount.is_finite()
            && row.cash_amount > 0.0
        {
            if let Some(date) = parse_date(&row.ex_dividend_date) {
                events.push(Event::Dividend {
                    date,
                    amount: row.cash_amount,
                });
            }
        }
    }
    events.sort_by_key(|event| {
        std::cmp::Reverse(match event {
            Event::Split { date, .. } | Event::Dividend { date, .. } => *date,
        })
    });
    let sentinel = chrono::NaiveDate::parse_from_str(AUXILIARY_SENTINEL, "%Y-%m-%d")?;
    let mut rows = vec![FactorFileEntry {
        date: sentinel,
        price_factor: Decimal::ONE,
        split_factor: Decimal::ONE,
        reference_price: Decimal::ZERO,
    }];
    let mut price_factor = Decimal::ONE;
    let mut split_factor = Decimal::ONE;
    for event in events {
        match event {
            Event::Split { date, factor } => {
                let reference_price =
                    prior_close(&reference_prices, date.succ_opt().unwrap_or(date))
                        .unwrap_or_default();
                rows.push(FactorFileEntry {
                    date,
                    price_factor,
                    split_factor,
                    reference_price: Decimal::from_f64(reference_price).unwrap_or_default(),
                });
                split_factor *= decimal(factor)?;
            }
            Event::Dividend { date, amount } => {
                let previous = prior_close(&reference_prices, date).unwrap_or_default();
                if previous <= 0.0 {
                    continue;
                }
                price_factor *= decimal((previous - amount) / previous)?;
                rows.push(FactorFileEntry {
                    date,
                    price_factor,
                    split_factor,
                    reference_price: decimal(previous)?,
                });
            }
        }
    }
    rows.push(FactorFileEntry {
        date: start,
        price_factor,
        split_factor,
        reference_price: Decimal::ZERO,
    });
    rows.sort_by_key(|row| row.date);
    rows.dedup_by_key(|row| row.date);
    Ok(rows)
}

fn prior_close(prices: &HashMap<chrono::NaiveDate, f64>, date: chrono::NaiveDate) -> Option<f64> {
    (1..=5).find_map(|days| prices.get(&(date - chrono::Duration::days(days))).copied())
}

fn assemble_ticker_changes(
    events: Vec<TickerEvent>,
) -> (Option<chrono::NaiveDate>, Vec<TickerChangeEvent>) {
    let mut events = events
        .into_iter()
        .filter(|event| event.event_type == "ticker_change")
        .filter_map(|event| {
            parse_date(&event.date)
                .map(|date| (date, event.ticker_change.ticker.to_ascii_uppercase()))
        })
        .collect::<Vec<_>>();
    events.sort_by_key(|(date, _)| *date);
    let Some((list_date, first_ticker)) = events.first().cloned() else {
        return (None, Vec::new());
    };
    let mut previous = first_ticker;
    let mut changes = Vec::new();
    for (effective_date, ticker) in events.into_iter().skip(1) {
        if ticker != previous {
            changes.push(TickerChangeEvent {
                effective_date,
                old_ticker: std::mem::replace(&mut previous, ticker),
            });
        }
    }
    (Some(list_date), changes)
}

fn compute_map_rows(
    ticker: &str,
    list_date: Option<chrono::NaiveDate>,
    delisting_date: Option<chrono::NaiveDate>,
    mut changes: Vec<TickerChangeEvent>,
) -> Result<Vec<MapFileEntry>> {
    let current = ticker.to_ascii_uppercase();
    let start = list_date.unwrap_or(chrono::NaiveDate::from_ymd_opt(1998, 1, 2).unwrap());
    let end = delisting_date.unwrap_or(chrono::NaiveDate::parse_from_str(
        AUXILIARY_SENTINEL,
        "%Y-%m-%d",
    )?);
    changes.retain(|event| event.effective_date >= start && event.effective_date <= end);
    changes.sort_by_key(|event| event.effective_date);
    let mut rows = vec![MapFileEntry {
        date: start,
        mapped_symbol: changes
            .first()
            .map(|e| e.old_ticker.clone())
            .unwrap_or_else(|| current.clone()),
        primary_exchange_code: String::new(),
        data_mapping_mode: None,
    }];
    rows.extend(changes.into_iter().filter_map(|event| {
        event.effective_date.pred_opt().map(|date| MapFileEntry {
            date,
            mapped_symbol: event.old_ticker,
            primary_exchange_code: String::new(),
            data_mapping_mode: None,
        })
    }));
    if start < end {
        rows.push(MapFileEntry {
            date: end,
            mapped_symbol: current,
            primary_exchange_code: String::new(),
            data_mapping_mode: None,
        });
    }
    rows.sort_by_key(|row| row.date);
    rows.dedup_by_key(|row| row.date);
    Ok(rows)
}

impl Quote {
    fn timestamp_ns(&self) -> i64 {
        if self.sip_timestamp != 0 {
            self.sip_timestamp
        } else {
            self.participant_timestamp
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::{DataNormalizationMode, Market, Symbol};
    use rlean_data::SubscriptionDataConfig;

    #[test]
    fn aggregates_quote_updates_into_one_minute_bar() {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut config = SubscriptionDataConfig::new_equity(
            symbol,
            Resolution::Minute,
            DataNormalizationMode::Raw,
        );
        config.set_tick_type(TickType::Quote);
        let request = HistoryRequest::new(
            config,
            NanosecondTimestamp(60_000_000_000),
            NanosecondTimestamp(180_000_000_000),
        )
        .unwrap();
        let bars = aggregate_quotes(
            &request,
            vec![
                Quote {
                    participant_timestamp: 61_000_000_000,
                    sip_timestamp: 0,
                    bid_price: 100.0,
                    ask_price: 101.0,
                    bid_size: 10.0,
                    ask_size: 11.0,
                    bid_exchange: None,
                    ask_exchange: None,
                },
                Quote {
                    participant_timestamp: 119_000_000_000,
                    sip_timestamp: 0,
                    bid_price: 99.0,
                    ask_price: 102.0,
                    bid_size: 12.0,
                    ask_size: 13.0,
                    bid_exchange: None,
                    ask_exchange: None,
                },
            ],
        )
        .unwrap();
        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].bid.as_ref().unwrap().open, Decimal::from(100));
        assert_eq!(bars[0].bid.as_ref().unwrap().low, Decimal::from(99));
        assert_eq!(bars[0].ask.as_ref().unwrap().high, Decimal::from(102));
        assert_eq!(bars[0].last_ask_size, Decimal::from(13));
    }
}
