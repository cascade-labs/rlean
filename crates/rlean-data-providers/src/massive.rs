use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use reqwest::{Client, StatusCode, Url};
use rlean_core::{
    DateTime, MarketHoursDatabase, NanosecondTimestamp, Resolution, SecurityType, TickType,
    TimeSpan,
};
use rlean_data_tables::{Bar, QuoteBar, TradeBar};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::{HistoricalData, HistoricalDataProvider, HistoryRequest};

const DEFAULT_BASE_URL: &str = "https://api.massive.com";
const DEFAULT_REQUESTS_PER_SECOND: f64 = 5.0;
const MAX_RETRIES: usize = 5;

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
                        venue: Some("massive".to_string()),
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
        request.configuration.data_kind == rlean_data::SubscriptionDataKind::Market
            && matches!(
                request.configuration.symbol.security_type(),
                SecurityType::Equity | SecurityType::Option | SecurityType::Index
            )
            && matches!(
                request.configuration.tick_type,
                TickType::Trade | TickType::Quote
            )
    }

    async fn get_history(&self, request: &HistoryRequest) -> Result<HistoricalData> {
        match request.configuration.tick_type {
            TickType::Trade => Ok(HistoricalData::TradeBars(self.trade_bars(request).await?)),
            TickType::Quote => Ok(HistoricalData::QuoteBars(self.quote_bars(request).await?)),
            other => bail!("Massive does not support {other:?} history"),
        }
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
            venue: Some("massive".to_string()),
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
                },
                Quote {
                    participant_timestamp: 119_000_000_000,
                    sip_timestamp: 0,
                    bid_price: 99.0,
                    ask_price: 102.0,
                    bid_size: 12.0,
                    ask_size: 13.0,
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
