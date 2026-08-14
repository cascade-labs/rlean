use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::America::New_York;
use reqwest::{Client, StatusCode, Url};
use rlean_core::{
    DateTime, Market, MarketHoursDatabase, NanosecondTimestamp, OptionRight, OptionStyle,
    Resolution, SecurityType, Symbol, SymbolOptionsExt, TickType, TimeSpan,
};
use rlean_data::SubscriptionDataKind;
use rlean_data_tables::{Bar, OptionUniverseRow, QuoteBar, Tick, TradeBar};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore};

use crate::{HistoricalData, HistoricalDataProvider, HistoryRequest};

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:25510";
const DEFAULT_MAX_CONCURRENT: usize = 16;
const DEFAULT_REQUESTS_PER_SECOND: f64 = 20.0;
const MAX_RETRIES: u32 = 5;

#[derive(Debug, Clone)]
pub struct ThetaDataConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub max_concurrent: usize,
    pub requests_per_second: f64,
    pub timeout: Duration,
    pub start_date: NaiveDate,
}

impl ThetaDataConfig {
    pub fn new(api_key: Option<String>) -> Self {
        Self {
            api_key,
            base_url: DEFAULT_BASE_URL.to_string(),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            requests_per_second: DEFAULT_REQUESTS_PER_SECOND,
            timeout: Duration::from_secs(300),
            start_date: NaiveDate::from_ymd_opt(2018, 1, 1).expect("valid ThetaData epoch"),
        }
    }
}

#[derive(Clone)]
pub struct ThetaDataHistoricalDataProvider {
    client: Client,
    config: Arc<ThetaDataConfig>,
    concurrency: Arc<Semaphore>,
    limiter: Arc<RateLimiter>,
}

struct RateLimiter {
    interval: Duration,
    next_allowed: Mutex<Instant>,
}

impl RateLimiter {
    fn new(requests_per_second: f64) -> Self {
        let rate = if requests_per_second.is_finite() && requests_per_second > 0.0 {
            requests_per_second
        } else {
            DEFAULT_REQUESTS_PER_SECOND
        };
        Self {
            interval: Duration::from_secs_f64(1.0 / rate),
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

impl ThetaDataHistoricalDataProvider {
    pub fn new(config: ThetaDataConfig) -> Result<Self> {
        if config.base_url.trim().is_empty() {
            bail!("ThetaData base URL cannot be empty");
        }
        let client = Client::builder()
            .timeout(config.timeout)
            .gzip(true)
            .build()
            .context("build ThetaData HTTP client")?;
        let max_concurrent = config.max_concurrent.max(1);
        let limiter = Arc::new(RateLimiter::new(config.requests_per_second));
        Ok(Self {
            client,
            config: Arc::new(config),
            concurrency: Arc::new(Semaphore::new(max_concurrent)),
            limiter,
        })
    }

    async fn trade_bars(&self, request: &HistoryRequest) -> Result<Vec<TradeBar>> {
        let symbol = &request.configuration.symbol;
        match symbol.security_type() {
            SecurityType::Equity => self.stock_trade_bars(request).await,
            SecurityType::Option => self.option_trade_bars(request).await,
            other => bail!("ThetaData does not support {other:?} TradeBars"),
        }
    }

    async fn quote_bars(&self, request: &HistoryRequest) -> Result<Vec<QuoteBar>> {
        let symbol = &request.configuration.symbol;
        match symbol.security_type() {
            SecurityType::Equity => self.stock_quote_bars(request).await,
            SecurityType::Option => self.option_quote_bars(request).await,
            other => bail!("ThetaData does not support {other:?} QuoteBars"),
        }
    }

    async fn ticks(&self, request: &HistoryRequest) -> Result<Vec<Tick>> {
        let symbol = &request.configuration.symbol;
        match symbol.security_type() {
            SecurityType::Equity => self.stock_ticks(request).await,
            SecurityType::Option => self.option_ticks(request).await,
            other => bail!("ThetaData does not support {other:?} ticks"),
        }
    }

    async fn stock_trade_bars(&self, request: &HistoryRequest) -> Result<Vec<TradeBar>> {
        let ticker = request.configuration.symbol.permtick().to_ascii_uppercase();
        let (start, end) = request_dates(request, self.config.start_date)?;
        if start > end {
            return Ok(Vec::new());
        }
        if request.configuration.resolution == Resolution::Daily {
            let mut rows = Vec::new();
            for (chunk_start, chunk_end) in year_chunks(start, end) {
                let url = self.url(
                    "/v3/stock/history/eod",
                    &[
                        ("symbol", ticker.clone()),
                        ("start_date", vendor_date(chunk_start)),
                        ("end_date", vendor_date(chunk_end)),
                        ("venue", "utp_cta".to_string()),
                        ("format", "ndjson".to_string()),
                    ],
                )?;
                rows.extend(self.get_ndjson(url).await?.iter().filter_map(parse_eod));
            }
            rows.sort_by_key(|row| row.date);
            rows.dedup_by_key(|row| row.date);
            return rows
                .into_iter()
                .filter_map(|row| daily_trade_bar(request, row))
                .collect();
        }
        let (interval, period) = bar_resolution(request.configuration.resolution)?;
        let mut rows = Vec::new();
        for (chunk_start, chunk_end) in month_chunks(start, end) {
            let url = self.url(
                "/v3/stock/history/ohlc",
                &[
                    ("symbol", ticker.clone()),
                    ("start_date", vendor_date(chunk_start)),
                    ("end_date", vendor_date(chunk_end)),
                    ("interval", interval.to_string()),
                    ("venue", "utp_cta".to_string()),
                    ("format", "ndjson".to_string()),
                ],
            )?;
            for value in self.get_ndjson(url).await? {
                let Some(row) = parse_ohlc(&value) else {
                    continue;
                };
                if let Some(bar) = intraday_trade_bar(request, row, period)? {
                    rows.push(bar);
                }
            }
        }
        rows.sort_by_key(|row| row.end_time);
        rows.dedup_by_key(|row| row.end_time);
        Ok(rows)
    }

    async fn stock_quote_bars(&self, request: &HistoryRequest) -> Result<Vec<QuoteBar>> {
        let ticker = request.configuration.symbol.permtick().to_ascii_uppercase();
        let (start, end) = request_dates(request, self.config.start_date)?;
        if start > end {
            return Ok(Vec::new());
        }
        let daily = request.configuration.resolution == Resolution::Daily;
        let (interval, period) = if daily {
            ("1m", TimeSpan::from_secs(60))
        } else {
            bar_resolution(request.configuration.resolution)?
        };
        let mut bars = Vec::new();
        for date in dates(start, end) {
            if session_bounds(request, date).is_none() {
                continue;
            }
            let mut parameters = vec![
                ("symbol", ticker.clone()),
                ("date", vendor_date(date)),
                ("interval", interval.to_string()),
                ("venue", "utp_cta".to_string()),
                ("format", "ndjson".to_string()),
            ];
            if !request.configuration.extended_market_hours || daily {
                let (_, close) = session_bounds(request, date).expect("checked session");
                let close = close.to_tz(New_York);
                parameters.push(("start_time", "09:30:00".to_string()));
                parameters.push(("end_time", close.format("%H:%M:%S").to_string()));
            }
            let url = self.url("/v3/stock/history/quote", &parameters)?;
            let mut points = self
                .get_ndjson(url)
                .await?
                .iter()
                .filter_map(parse_quote)
                .filter(|row| session_allows(request, date, row.time))
                .collect::<Vec<_>>();
            points.sort_by_key(|row| row.time);
            points.dedup_by_key(|row| row.time);
            if daily {
                if let Some(bar) = daily_quote_bar(request, date, &points)? {
                    bars.push(bar);
                }
            } else {
                bars.extend(
                    points
                        .into_iter()
                        .filter_map(|row| quote_bar_from_point(request, row, period).transpose())
                        .collect::<Result<Vec<_>>>()?,
                );
            }
        }
        bars.sort_by_key(|row| row.end_time);
        bars.dedup_by_key(|row| row.end_time);
        Ok(bars)
    }

    async fn option_trade_bars(&self, request: &HistoryRequest) -> Result<Vec<TradeBar>> {
        let contract = option_contract(&request.configuration.symbol)?;
        let (start, end) = request_dates(request, self.config.start_date)?;
        if start > end {
            return Ok(Vec::new());
        }
        let daily = request.configuration.resolution == Resolution::Daily;
        let (interval, period) = if daily {
            ("1m", TimeSpan::from_secs(60))
        } else {
            bar_resolution(request.configuration.resolution)?
        };
        let mut bars = Vec::new();
        for date in dates(start, end.min(contract.expiration)) {
            if session_bounds(request, date).is_none() {
                continue;
            }
            let url = self.url(
                "/v3/option/history/ohlc",
                &[
                    ("symbol", contract.root.clone()),
                    ("expiration", vendor_date(contract.expiration)),
                    ("strike", contract.strike.normalize().to_string()),
                    ("right", option_right_query(contract.right).to_string()),
                    ("start_date", vendor_date(date)),
                    ("end_date", vendor_date(date)),
                    ("interval", interval.to_string()),
                    ("format", "ndjson".to_string()),
                ],
            )?;
            let mut day = self
                .get_ndjson(url)
                .await?
                .iter()
                .filter_map(parse_option_ohlc)
                .filter(|row| option_row_matches(row, &contract))
                .filter_map(|row| intraday_trade_bar(request, row.bar, period).transpose())
                .collect::<Result<Vec<_>>>()?;
            day.sort_by_key(|bar| bar.time);
            day.dedup_by_key(|bar| bar.time);
            if daily {
                if let Some(bar) = aggregate_daily_trade_bars(request, date, &day)? {
                    bars.push(bar);
                }
            } else {
                bars.extend(day);
            }
        }
        bars.sort_by_key(|row| row.end_time);
        bars.dedup_by_key(|row| row.end_time);
        Ok(bars)
    }

    async fn option_quote_bars(&self, request: &HistoryRequest) -> Result<Vec<QuoteBar>> {
        let contract = option_contract(&request.configuration.symbol)?;
        let (start, end) = request_dates(request, self.config.start_date)?;
        if start > end {
            return Ok(Vec::new());
        }
        let daily = request.configuration.resolution == Resolution::Daily;
        let (interval, period) = if daily {
            ("1m", TimeSpan::from_secs(60))
        } else {
            bar_resolution(request.configuration.resolution)?
        };
        let mut bars = Vec::new();
        for date in dates(start, end.min(contract.expiration)) {
            if session_bounds(request, date).is_none() {
                continue;
            }
            let url = self.url(
                "/v3/option/history/quote",
                &[
                    ("symbol", contract.root.clone()),
                    ("expiration", vendor_date(contract.expiration)),
                    ("strike", contract.strike.normalize().to_string()),
                    ("right", option_right_query(contract.right).to_string()),
                    ("date", vendor_date(date)),
                    ("interval", interval.to_string()),
                    ("format", "ndjson".to_string()),
                ],
            )?;
            let mut day = self
                .get_ndjson(url)
                .await?
                .iter()
                .filter_map(parse_option_quote)
                .filter(|row| option_quote_matches(row, &contract))
                .filter(|row| session_allows(request, date, row.quote.time))
                .map(|row| row.quote)
                .collect::<Vec<_>>();
            day.sort_by_key(|row| row.time);
            day.dedup_by_key(|row| row.time);
            if daily {
                if let Some(bar) = daily_quote_bar(request, date, &day)? {
                    bars.push(bar);
                }
            } else {
                bars.extend(
                    day.into_iter()
                        .filter_map(|row| quote_bar_from_point(request, row, period).transpose())
                        .collect::<Result<Vec<_>>>()?,
                );
            }
        }
        bars.sort_by_key(|row| row.end_time);
        bars.dedup_by_key(|row| row.end_time);
        Ok(bars)
    }

    async fn stock_ticks(&self, request: &HistoryRequest) -> Result<Vec<Tick>> {
        let ticker = request.configuration.symbol.permtick().to_ascii_uppercase();
        self.trade_quote_ticks(request, &ticker, None).await
    }

    async fn option_ticks(&self, request: &HistoryRequest) -> Result<Vec<Tick>> {
        let contract = option_contract(&request.configuration.symbol)?;
        self.trade_quote_ticks(request, &contract.root, Some(&contract))
            .await
    }

    async fn trade_quote_ticks(
        &self,
        request: &HistoryRequest,
        ticker: &str,
        contract: Option<&OptionContract>,
    ) -> Result<Vec<Tick>> {
        let (start, end) = request_dates(request, self.config.start_date)?;
        if start > end {
            return Ok(Vec::new());
        }
        let mut ticks = Vec::new();
        for date in dates(
            start,
            end.min(contract.map_or(end, |value| value.expiration)),
        ) {
            if session_bounds(request, date).is_none() {
                continue;
            }
            let path = if contract.is_some() {
                "/v3/option/history/trade_quote"
            } else {
                "/v3/stock/history/trade_quote"
            };
            let mut parameters = vec![
                ("symbol", ticker.to_string()),
                ("date", vendor_date(date)),
                ("format", "ndjson".to_string()),
            ];
            if let Some(contract) = contract {
                parameters.push(("expiration", vendor_date(contract.expiration)));
                parameters.push(("strike", contract.strike.normalize().to_string()));
                parameters.push(("right", option_right_query(contract.right).to_string()));
            } else {
                parameters.push(("venue", "utp_cta".to_string()));
            }
            let url = self.url(path, &parameters)?;
            for value in self.get_ndjson(url).await? {
                let Some(row) = parse_trade_quote(&value) else {
                    continue;
                };
                if contract.is_some_and(|contract| !trade_quote_matches(&row, contract)) {
                    continue;
                }
                let tick = match request.configuration.tick_type {
                    TickType::Trade => trade_tick(request, &row),
                    TickType::Quote => quote_tick(request, &row),
                    _ => None,
                };
                if let Some(tick) = tick.filter(|tick| {
                    tick.time >= request.range.start
                        && tick.time < request.range.end
                        && session_allows(request, date, tick.time)
                }) {
                    ticks.push(tick);
                }
            }
        }
        ticks.sort_by_key(|row| row.time);
        ticks.dedup_by(|left, right| {
            left.time == right.time
                && left.tick_type == right.tick_type
                && left.value == right.value
                && left.quantity == right.quantity
        });
        Ok(ticks)
    }

    async fn option_universe(&self, request: &HistoryRequest) -> Result<Vec<OptionUniverseRow>> {
        let metadata = request
            .configuration
            .option_chain
            .as_ref()
            .context("ThetaData option universe request is missing chain metadata")?;
        let underlying = request
            .configuration
            .symbol
            .underlying
            .as_deref()
            .cloned()
            .unwrap_or_else(|| Symbol::create_equity(&metadata.underlying_ticker, &Market::usa()));
        let exchange_hours = MarketHoursDatabase::global().exchange_hours(&underlying);
        let (start, end) = request_dates(request, self.config.start_date)?;
        if start > end {
            return Ok(Vec::new());
        }
        let mut output = Vec::new();
        for date in dates(start, end) {
            let Some((_open, close)) = exchange_hours.session_bounds(date) else {
                continue;
            };
            if close <= request.range.start || close > request.range.end {
                continue;
            }
            // C# LEAN BaseChainUniverseData for `date` becomes available at
            // the following midnight. OptionFilterUniverse evaluates expiry
            // against that local end time and advances it only when closed.
            let mut selection_date = date.succ_opt().context("option selection date overflow")?;
            while exchange_hours.session_bounds(selection_date).is_none() {
                selection_date = selection_date
                    .succ_opt()
                    .context("option selection date overflow")?;
            }
            let max_dte =
                option_query_max_dte(date, selection_date, metadata.filter.max_expiry_days);
            let url = self.url(
                "/v3/option/list/contracts/quote",
                &[
                    ("symbol", metadata.underlying_ticker.to_ascii_uppercase()),
                    ("date", vendor_date(date)),
                    ("max_dte", max_dte.to_string()),
                    ("format", "ndjson".to_string()),
                ],
            )?;
            let mut contracts = Vec::new();
            for value in self.get_ndjson(url).await? {
                let Some(expiration) = value
                    .get("expiration")
                    .and_then(Value::as_str)
                    .and_then(parse_date)
                else {
                    continue;
                };
                let days = (expiration - selection_date).num_days();
                if days < i64::from(metadata.filter.min_expiry_days)
                    || days > i64::from(metadata.filter.max_expiry_days)
                {
                    continue;
                }
                let Some(strike) = number(&value, "strike").and_then(Decimal::from_f64) else {
                    continue;
                };
                let Some(right) = value
                    .get("right")
                    .and_then(Value::as_str)
                    .and_then(parse_right)
                else {
                    continue;
                };
                let option = Symbol::create_option_osi(
                    underlying.clone(),
                    strike,
                    expiration,
                    right,
                    OptionStyle::American,
                    &Market::usa(),
                );
                contracts.push(OptionUniverseRow {
                    date,
                    market: request.configuration.symbol.market().as_str().to_string(),
                    security_type: "Option".to_string(),
                    symbol_sid: option.id.to_string(),
                    symbol_value: option.value.to_string(),
                    underlying_sid: Some(underlying.id.to_string()),
                    underlying_value: Some(metadata.underlying_ticker.clone()),
                    expiration: Some(expiration),
                    strike: Some(strike),
                    right: Some(
                        match right {
                            OptionRight::Call => "Call",
                            OptionRight::Put => "Put",
                        }
                        .to_string(),
                    ),
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
                });
            }
            if contracts.is_empty() {
                continue;
            }
            // Contract-list rows describe only the option identifiers.  They
            // intentionally carry no underlying price, so hydrate the
            // canonical underlying row from the source session before this
            // universe can be cached or selected.
            let eod_url = self.url(
                "/v3/stock/history/eod",
                &[
                    ("symbol", metadata.underlying_ticker.to_ascii_uppercase()),
                    ("start_date", vendor_date(date)),
                    ("end_date", vendor_date(date)),
                    ("venue", "utp_cta".to_string()),
                    ("format", "ndjson".to_string()),
                ],
            )?;
            let underlying_eod = self
                .get_ndjson(eod_url)
                .await?
                .iter()
                .filter_map(parse_eod)
                .find(|row| row.date == date)
                .with_context(|| {
                    format!(
                        "ThetaData has no valid underlying EOD OHLC for {} on {date}",
                        metadata.underlying_ticker
                    )
                })?;
            output.push(OptionUniverseRow {
                date,
                market: request.configuration.symbol.market().as_str().to_string(),
                security_type: "Equity".to_string(),
                symbol_sid: underlying.id.to_string(),
                symbol_value: metadata.underlying_ticker.clone(),
                underlying_sid: None,
                underlying_value: None,
                expiration: None,
                strike: None,
                right: None,
                open: decimal(underlying_eod.open)?,
                high: decimal(underlying_eod.high)?,
                low: decimal(underlying_eod.low)?,
                close: decimal(underlying_eod.close)?,
                volume: decimal(underlying_eod.volume)?,
                open_interest: None,
                implied_volatility: None,
                delta: None,
                gamma: None,
                vega: None,
                theta: None,
                rho: None,
            });
            output.extend(contracts);
        }
        Ok(output)
    }

    fn url(&self, path: &str, parameters: &[(impl AsRef<str>, String)]) -> Result<Url> {
        let mut url = Url::parse(&format!(
            "{}{}",
            self.config.base_url.trim_end_matches('/'),
            path
        ))?;
        {
            let mut query = url.query_pairs_mut();
            for (name, value) in parameters {
                query.append_pair(name.as_ref(), value);
            }
        }
        Ok(url)
    }

    async fn get_ndjson(&self, url: Url) -> Result<Vec<Value>> {
        for attempt in 0..=MAX_RETRIES {
            tracing::debug!(
                url = %redacted_url(&url),
                attempt,
                "requesting ThetaData history"
            );
            self.limiter.wait().await;
            let _permit = self
                .concurrency
                .acquire()
                .await
                .context("ThetaData concurrency limiter closed")?;
            let mut request = self.client.get(url.clone());
            if let Some(api_key) = self
                .config
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                request = request.bearer_auth(api_key);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) if attempt < MAX_RETRIES => {
                    drop(_permit);
                    let delay = Duration::from_secs(2_u64.pow(attempt + 1));
                    tracing::warn!(%error, ?delay, "ThetaData request failed; retrying");
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(error) => return Err(error).context("request ThetaData history"),
            };
            let status = response.status();
            if matches!(status.as_u16(), 472 | 475 | 572) || status == StatusCode::NOT_FOUND {
                return Ok(Vec::new());
            }
            if status == StatusCode::TOO_MANY_REQUESTS && attempt < MAX_RETRIES {
                drop(_permit);
                let delay = Duration::from_secs(2_u64.pow(attempt + 1));
                tracing::warn!(?delay, "ThetaData rate limited; retrying");
                tokio::time::sleep(delay).await;
                continue;
            }
            let body = response.text().await.context("read ThetaData response")?;
            if !status.is_success() {
                bail!(
                    "ThetaData HTTP {status} for {}: {}",
                    redacted_url(&url),
                    body.chars().take(500).collect::<String>()
                );
            }
            let rows = body
                .lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| match serde_json::from_str(line) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        tracing::warn!(%error, "skipping malformed ThetaData NDJSON row");
                        None
                    }
                })
                .collect::<Vec<_>>();
            tracing::debug!(
                url = %redacted_url(&url),
                rows = rows.len(),
                "received ThetaData history"
            );
            return Ok(rows);
        }
        unreachable!("ThetaData retry loop returns")
    }
}

fn option_query_max_dte(
    source_date: NaiveDate,
    selection_date: NaiveDate,
    max_expiry_days: i32,
) -> i64 {
    (selection_date - source_date).num_days() + i64::from(max_expiry_days.max(0))
}

#[async_trait]
impl HistoricalDataProvider for ThetaDataHistoricalDataProvider {
    fn name(&self) -> &str {
        "thetadata"
    }

    fn supports(&self, request: &HistoryRequest) -> bool {
        if request.configuration.option_chain.is_some() {
            return request.configuration.symbol.security_type() == SecurityType::Option;
        }
        request.configuration.data_kind == SubscriptionDataKind::Market
            && request.configuration.symbol.market() == &Market::usa()
            && matches!(
                request.configuration.symbol.security_type(),
                SecurityType::Equity | SecurityType::Option
            )
            && matches!(
                request.configuration.tick_type,
                TickType::Trade | TickType::Quote
            )
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
            other => bail!("ThetaData does not support {other:?} history"),
        }
    }
}

#[derive(Debug, Clone)]
struct OhlcRow {
    date: NaiveDate,
    time: DateTime,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

#[derive(Debug, Clone)]
struct EodRow {
    date: NaiveDate,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

#[derive(Debug, Clone)]
struct QuotePoint {
    time: DateTime,
    bid: f64,
    ask: f64,
    bid_size: f64,
    ask_size: f64,
}

#[derive(Debug, Clone)]
struct OptionOhlcRow {
    expiration: NaiveDate,
    strike: f64,
    right: OptionRight,
    bar: OhlcRow,
}

#[derive(Debug, Clone)]
struct OptionQuotePoint {
    expiration: NaiveDate,
    strike: f64,
    right: OptionRight,
    quote: QuotePoint,
}

#[derive(Debug, Clone)]
struct TradeQuoteRow {
    expiration: Option<NaiveDate>,
    strike: Option<f64>,
    right: Option<OptionRight>,
    trade_time: Option<DateTime>,
    quote_time: Option<DateTime>,
    price: f64,
    size: f64,
    exchange: Option<String>,
    condition: Option<String>,
    bid: f64,
    ask: f64,
    bid_size: f64,
    ask_size: f64,
    bid_exchange: Option<String>,
    bid_condition: Option<String>,
}

#[derive(Debug, Clone)]
struct OptionContract {
    root: String,
    expiration: NaiveDate,
    strike: Decimal,
    right: OptionRight,
}

fn option_contract(symbol: &Symbol) -> Result<OptionContract> {
    let expiration = symbol
        .id
        .expiry
        .context("option symbol has no expiration")?;
    let strike = symbol.id.strike.context("option symbol has no strike")?;
    let right = symbol
        .id
        .option_right
        .context("option symbol has no right")?;
    let root = symbol
        .underlying()
        .map(|underlying| underlying.permtick().to_ascii_uppercase())
        .unwrap_or_else(|| symbol.permtick().to_ascii_uppercase());
    Ok(OptionContract {
        root,
        expiration,
        strike,
        right,
    })
}

fn option_right_query(right: OptionRight) -> &'static str {
    match right {
        OptionRight::Call => "call",
        OptionRight::Put => "put",
    }
}

fn parse_ohlc(value: &Value) -> Option<OhlcRow> {
    let date = parse_vendor_date(value)?;
    let time = vendor_time(value, date)?;
    let row = OhlcRow {
        date,
        time,
        open: number(value, "open")?,
        high: number(value, "high")?,
        low: number(value, "low")?,
        close: number(value, "close")?,
        volume: number(value, "volume").unwrap_or_default(),
    };
    (row.open > 0.0 && row.high > 0.0 && row.low > 0.0 && row.close > 0.0).then_some(row)
}

fn parse_eod(value: &Value) -> Option<EodRow> {
    let date = parse_vendor_date(value)?;
    let row = EodRow {
        date,
        open: number(value, "open")?,
        high: number(value, "high")?,
        low: number(value, "low")?,
        close: number(value, "close")?,
        volume: number(value, "volume").unwrap_or_default(),
    };
    (row.open > 0.0 && row.high > 0.0 && row.low > 0.0 && row.close > 0.0).then_some(row)
}

fn parse_quote(value: &Value) -> Option<QuotePoint> {
    let date = parse_vendor_date(value)?;
    let row = QuotePoint {
        time: vendor_time(value, date)?,
        bid: number(value, "bid_price").or_else(|| number(value, "bid"))?,
        ask: number(value, "ask_price").or_else(|| number(value, "ask"))?,
        bid_size: number(value, "bid_size").unwrap_or_default(),
        ask_size: number(value, "ask_size").unwrap_or_default(),
    };
    (row.bid > 0.0 && row.ask > 0.0).then_some(row)
}

fn parse_option_ohlc(value: &Value) -> Option<OptionOhlcRow> {
    Some(OptionOhlcRow {
        expiration: value.get("expiration")?.as_str().and_then(parse_date)?,
        strike: number(value, "strike")?,
        right: value.get("right")?.as_str().and_then(parse_right)?,
        bar: parse_ohlc(value)?,
    })
}

fn parse_option_quote(value: &Value) -> Option<OptionQuotePoint> {
    Some(OptionQuotePoint {
        expiration: value.get("expiration")?.as_str().and_then(parse_date)?,
        strike: number(value, "strike")?,
        right: value.get("right")?.as_str().and_then(parse_right)?,
        quote: parse_quote(value)?,
    })
}

fn parse_trade_quote(value: &Value) -> Option<TradeQuoteRow> {
    Some(TradeQuoteRow {
        expiration: value
            .get("expiration")
            .and_then(Value::as_str)
            .and_then(parse_date),
        strike: number(value, "strike"),
        right: value
            .get("right")
            .and_then(Value::as_str)
            .and_then(parse_right),
        trade_time: value
            .get("trade_timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp),
        quote_time: value
            .get("quote_timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp),
        price: number(value, "price").unwrap_or_default(),
        size: number(value, "size").unwrap_or_default(),
        exchange: integer_string(value, "exchange"),
        condition: integer_string(value, "condition"),
        bid: number(value, "bid")
            .or_else(|| number(value, "bid_price"))
            .unwrap_or_default(),
        ask: number(value, "ask")
            .or_else(|| number(value, "ask_price"))
            .unwrap_or_default(),
        bid_size: number(value, "bid_size").unwrap_or_default(),
        ask_size: number(value, "ask_size").unwrap_or_default(),
        bid_exchange: integer_string(value, "bid_exchange"),
        bid_condition: integer_string(value, "bid_condition"),
    })
}

fn option_row_matches(row: &OptionOhlcRow, contract: &OptionContract) -> bool {
    row.expiration == contract.expiration
        && strike_matches(row.strike, contract.strike)
        && row.right == contract.right
}

fn option_quote_matches(row: &OptionQuotePoint, contract: &OptionContract) -> bool {
    row.expiration == contract.expiration
        && strike_matches(row.strike, contract.strike)
        && row.right == contract.right
}

fn trade_quote_matches(row: &TradeQuoteRow, contract: &OptionContract) -> bool {
    row.expiration == Some(contract.expiration)
        && row
            .strike
            .is_some_and(|strike| strike_matches(strike, contract.strike))
        && row.right == Some(contract.right)
}

fn strike_matches(value: f64, strike: Decimal) -> bool {
    let mills = (strike * Decimal::from(1000))
        .round()
        .to_i64()
        .unwrap_or_default();
    (value * 1000.0).round() as i64 == mills || value.round() as i64 == mills
}

fn daily_trade_bar(request: &HistoryRequest, row: EodRow) -> Option<Result<TradeBar>> {
    let (open, close) = session_bounds(request, row.date)?;
    (close > request.range.start && close <= request.range.end).then(|| {
        Ok(TradeBar {
            symbol: request.configuration.symbol.clone(),
            venue: Some(request.configuration.venue.clone()),
            time: open,
            end_time: close,
            open: decimal(row.open)?,
            high: decimal(row.high)?,
            low: decimal(row.low)?,
            close: decimal(row.close)?,
            volume: decimal(row.volume)?,
            period: close - open,
        })
    })
}

fn intraday_trade_bar(
    request: &HistoryRequest,
    row: OhlcRow,
    period: TimeSpan,
) -> Result<Option<TradeBar>> {
    let end = row.time + period;
    if end <= request.range.start
        || end > request.range.end
        || !session_allows(request, row.date, row.time)
    {
        return Ok(None);
    }
    Ok(Some(TradeBar {
        symbol: request.configuration.symbol.clone(),
        venue: Some(request.configuration.venue.clone()),
        time: row.time,
        end_time: end,
        open: decimal(row.open)?,
        high: decimal(row.high)?,
        low: decimal(row.low)?,
        close: decimal(row.close)?,
        volume: decimal(row.volume)?,
        period,
    }))
}

fn quote_bar_from_point(
    request: &HistoryRequest,
    row: QuotePoint,
    period: TimeSpan,
) -> Result<Option<QuoteBar>> {
    let end = row.time + period;
    if end <= request.range.start || end > request.range.end {
        return Ok(None);
    }
    let bid = decimal(row.bid)?;
    let ask = decimal(row.ask)?;
    Ok(Some(QuoteBar {
        symbol: request.configuration.symbol.clone(),
        venue: Some(request.configuration.venue.clone()),
        time: row.time,
        end_time: end,
        bid: Some(Bar::from_price(bid)),
        ask: Some(Bar::from_price(ask)),
        last_bid_size: decimal(row.bid_size)?,
        last_ask_size: decimal(row.ask_size)?,
        period,
    }))
}

fn daily_quote_bar(
    request: &HistoryRequest,
    date: NaiveDate,
    points: &[QuotePoint],
) -> Result<Option<QuoteBar>> {
    let Some(first) = points.first() else {
        return Ok(None);
    };
    let last = points.last().expect("non-empty quote points");
    let Some((open, close)) = session_bounds(request, date) else {
        return Ok(None);
    };
    if close <= request.range.start || close > request.range.end {
        return Ok(None);
    }
    let bid = Bar::new(
        decimal(first.bid)?,
        decimal(points.iter().map(|row| row.bid).fold(f64::MIN, f64::max))?,
        decimal(points.iter().map(|row| row.bid).fold(f64::MAX, f64::min))?,
        decimal(last.bid)?,
    );
    let ask = Bar::new(
        decimal(first.ask)?,
        decimal(points.iter().map(|row| row.ask).fold(f64::MIN, f64::max))?,
        decimal(points.iter().map(|row| row.ask).fold(f64::MAX, f64::min))?,
        decimal(last.ask)?,
    );
    Ok(Some(QuoteBar {
        symbol: request.configuration.symbol.clone(),
        venue: Some(request.configuration.venue.clone()),
        time: open,
        end_time: close,
        bid: Some(bid),
        ask: Some(ask),
        last_bid_size: decimal(last.bid_size)?,
        last_ask_size: decimal(last.ask_size)?,
        period: close - open,
    }))
}

fn aggregate_daily_trade_bars(
    request: &HistoryRequest,
    date: NaiveDate,
    bars: &[TradeBar],
) -> Result<Option<TradeBar>> {
    let Some(first) = bars.first() else {
        return Ok(None);
    };
    let last = bars.last().expect("non-empty trade bars");
    let Some((open, close)) = session_bounds(request, date) else {
        return Ok(None);
    };
    if close <= request.range.start || close > request.range.end {
        return Ok(None);
    }
    Ok(Some(TradeBar {
        symbol: request.configuration.symbol.clone(),
        venue: Some(request.configuration.venue.clone()),
        time: open,
        end_time: close,
        open: first.open,
        high: bars.iter().map(|bar| bar.high).max().unwrap_or_default(),
        low: bars.iter().map(|bar| bar.low).min().unwrap_or_default(),
        close: last.close,
        volume: bars.iter().map(|bar| bar.volume).sum(),
        period: close - open,
    }))
}

fn trade_tick(request: &HistoryRequest, row: &TradeQuoteRow) -> Option<Tick> {
    let time = row.trade_time?;
    (row.price > 0.0).then(|| {
        let mut tick = Tick::trade(
            request.configuration.symbol.clone(),
            time,
            decimal(row.price).ok()?,
            decimal(row.size.max(0.0)).ok()?,
        )
        .with_venue(request.configuration.venue.clone());
        tick.bid_price = decimal(row.bid.max(0.0)).ok()?;
        tick.ask_price = decimal(row.ask.max(0.0)).ok()?;
        tick.bid_size = decimal(row.bid_size.max(0.0)).ok()?;
        tick.ask_size = decimal(row.ask_size.max(0.0)).ok()?;
        tick.exchange.clone_from(&row.exchange);
        tick.sale_condition.clone_from(&row.condition);
        Some(tick)
    })?
}

fn quote_tick(request: &HistoryRequest, row: &TradeQuoteRow) -> Option<Tick> {
    let time = row.quote_time?;
    (row.bid > 0.0 && row.ask > 0.0).then(|| {
        let mut tick = Tick::quote(
            request.configuration.symbol.clone(),
            time,
            decimal(row.bid).ok()?,
            decimal(row.ask).ok()?,
            decimal(row.bid_size.max(0.0)).ok()?,
            decimal(row.ask_size.max(0.0)).ok()?,
        )
        .with_venue(request.configuration.venue.clone());
        tick.exchange.clone_from(&row.bid_exchange);
        tick.sale_condition.clone_from(&row.bid_condition);
        Some(tick)
    })?
}

fn request_dates(
    request: &HistoryRequest,
    lower_bound: NaiveDate,
) -> Result<(NaiveDate, NaiveDate)> {
    let timezone = request
        .configuration
        .data_time_zone
        .parse()
        .with_context(|| {
            format!(
                "invalid data timezone {}",
                request.configuration.data_time_zone
            )
        })?;
    let start = request
        .range
        .start
        .to_tz(timezone)
        .date_naive()
        .max(lower_bound);
    let inclusive_end = request.range.end - TimeSpan::from_nanos(1);
    let end = inclusive_end.to_tz(timezone).date_naive();
    Ok((start, end))
}

fn session_bounds(request: &HistoryRequest, date: NaiveDate) -> Option<(DateTime, DateTime)> {
    MarketHoursDatabase::global()
        .exchange_hours(&request.configuration.symbol)
        .session_bounds(date)
}

fn session_allows(request: &HistoryRequest, date: NaiveDate, time: DateTime) -> bool {
    if request.configuration.extended_market_hours {
        return true;
    }
    session_bounds(request, date).is_some_and(|(open, close)| time >= open && time < close)
}

fn bar_resolution(resolution: Resolution) -> Result<(&'static str, TimeSpan)> {
    match resolution {
        Resolution::Second => Ok(("1s", TimeSpan::from_secs(1))),
        Resolution::Minute => Ok(("1m", TimeSpan::from_secs(60))),
        Resolution::Hour => Ok(("1h", TimeSpan::from_secs(3_600))),
        other => bail!("ThetaData bars do not support {other:?} resolution"),
    }
}

fn parse_vendor_date(value: &Value) -> Option<NaiveDate> {
    value
        .get("date")
        .and_then(Value::as_str)
        .and_then(parse_date)
        .or_else(|| {
            ["timestamp", "last_trade", "created"]
                .iter()
                .find_map(|name| value.get(name).and_then(Value::as_str))
                .and_then(|value| value.get(..10))
                .and_then(parse_date)
        })
}

fn vendor_time(value: &Value, date: NaiveDate) -> Option<DateTime> {
    if let Some(ms) = value
        .get("ms_of_day")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
    {
        let local = date.and_hms_opt(0, 0, 0)? + chrono::Duration::milliseconds(ms as i64);
        let utc = New_York
            .from_local_datetime(&local)
            .single()?
            .with_timezone(&Utc);
        return utc.timestamp_nanos_opt().map(NanosecondTimestamp);
    }
    value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_timestamp)
}

fn parse_timestamp(value: &str) -> Option<DateTime> {
    if let Ok(value) = chrono::DateTime::parse_from_rfc3339(value) {
        return value
            .with_timezone(&Utc)
            .timestamp_nanos_opt()
            .map(NanosecondTimestamp);
    }
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(local) = NaiveDateTime::parse_from_str(value, format) {
            return New_York
                .from_local_datetime(&local)
                .single()?
                .with_timezone(&Utc)
                .timestamp_nanos_opt()
                .map(NanosecondTimestamp);
        }
    }
    None
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    let clean = value.replace('-', "");
    NaiveDate::parse_from_str(&clean, "%Y%m%d").ok()
}

fn parse_right(value: &str) -> Option<OptionRight> {
    match value.trim().to_ascii_lowercase().as_str() {
        "c" | "call" => Some(OptionRight::Call),
        "p" | "put" => Some(OptionRight::Put),
        _ => None,
    }
}

fn number(value: &Value, name: &str) -> Option<f64> {
    value.get(name).and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
    })
}

fn integer_string(value: &Value, name: &str) -> Option<String> {
    value.get(name).and_then(|value| {
        value
            .as_i64()
            .map(|value| value.to_string())
            .or_else(|| value.as_str().map(str::to_string))
    })
}

fn decimal(value: f64) -> Result<Decimal> {
    if !value.is_finite() {
        bail!("ThetaData returned a non-finite decimal")
    }
    Decimal::from_f64(value).context("ThetaData decimal is not representable")
}

fn vendor_date(date: NaiveDate) -> String {
    date.format("%Y%m%d").to_string()
}

fn dates(start: NaiveDate, end: NaiveDate) -> impl Iterator<Item = NaiveDate> {
    std::iter::successors(Some(start), |date| date.succ_opt()).take_while(move |date| *date <= end)
}

fn year_chunks(start: NaiveDate, end: NaiveDate) -> Vec<(NaiveDate, NaiveDate)> {
    let mut chunks = Vec::new();
    let mut cursor = start;
    while cursor <= end {
        let chunk_end = (cursor + chrono::Duration::days(364)).min(end);
        chunks.push((cursor, chunk_end));
        let Some(next) = chunk_end.succ_opt() else {
            break;
        };
        cursor = next;
    }
    chunks
}

fn month_chunks(start: NaiveDate, end: NaiveDate) -> Vec<(NaiveDate, NaiveDate)> {
    let mut chunks = Vec::new();
    let mut cursor = start;
    while cursor <= end {
        let (year, month) = if cursor.month() == 12 {
            (cursor.year() + 1, 1)
        } else {
            (cursor.year(), cursor.month() + 1)
        };
        let next_month = NaiveDate::from_ymd_opt(year, month, 1).expect("valid next month");
        let chunk_end = next_month.pred_opt().expect("month has prior day").min(end);
        chunks.push((cursor, chunk_end));
        cursor = next_month;
    }
    chunks
}

fn redacted_url(url: &Url) -> String {
    let mut value = url.clone();
    if value.query_pairs().any(|(name, _)| name == "api_key") {
        let pairs = value
            .query_pairs()
            .map(|(name, value)| {
                let value = if name == "api_key" {
                    "***".into()
                } else {
                    value
                };
                (name.into_owned(), value.into_owned())
            })
            .collect::<Vec<_>>();
        value.query_pairs_mut().clear().extend_pairs(pairs);
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::{DataNormalizationMode, NanosecondTimestamp};
    use rlean_data::{
        OptionChainFilterMetadata, OptionChainSubscriptionMetadata, SubscriptionDataConfig,
    };
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn option_universe_request(date: NaiveDate) -> HistoryRequest {
        let equity = Symbol::create_equity("SPY", &Market::usa());
        let canonical = Symbol::create_canonical_option(&equity, &Market::usa());
        let config = SubscriptionDataConfig::new_option_chain(
            canonical,
            Resolution::Minute,
            OptionChainSubscriptionMetadata {
                canonical_permtick: "?SPY".to_string(),
                underlying_ticker: "SPY".to_string(),
                filter: OptionChainFilterMetadata {
                    min_strike_rank: -5,
                    max_strike_rank: 5,
                    min_expiry_days: 0,
                    max_expiry_days: 0,
                },
            },
        );
        let (open, close) = MarketHoursDatabase::global()
            .exchange_hours(&equity)
            .session_bounds(date)
            .expect("trading session");
        HistoryRequest::new(config, open - TimeSpan::from_nanos(1), close).unwrap()
    }

    fn observed_theta_server(eod_body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for incoming in listener.incoming().take(2) {
                let mut stream = incoming.unwrap();
                let mut request = [0_u8; 8192];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let body = if request.contains("/v3/option/list/contracts/quote?") {
                    concat!(
                        "{\"symbol\":\"SPY\",\"strike\":585.0,\"expiration\":\"2024-11-19\",\"right\":\"CALL\"}\n",
                        "{\"symbol\":\"SPY\",\"strike\":590.0,\"expiration\":\"2024-11-19\",\"right\":\"CALL\"}\n"
                    )
                } else if request.contains("/v3/stock/history/eod?") {
                    eod_body
                } else {
                    panic!("unexpected ThetaData request: {request}");
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        format!("http://{address}")
    }

    fn transient_theta_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (attempt, incoming) in listener.incoming().take(2).enumerate() {
                let mut stream = incoming.unwrap();
                let mut request = [0_u8; 8192];
                stream.read(&mut request).unwrap();
                let (status, body) = if attempt == 0 {
                    ("500 Internal Server Error", "{\"error\":\"Proxy error\"}")
                } else {
                    ("200 OK", "{\"close\":552.08}\n")
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn retries_transient_server_error() {
        let mut config = ThetaDataConfig::new(None);
        config.base_url = transient_theta_server();
        let provider = ThetaDataHistoricalDataProvider::new(config).unwrap();
        let url = provider
            .url("/v3/stock/history/eod", &[("symbol", "SPY".to_string())])
            .unwrap();

        let rows = provider.get_ndjson(url).await.unwrap();

        assert_eq!(rows, vec![json!({"close": 552.08})]);
    }

    #[tokio::test]
    async fn option_universe_uses_observed_underlying_eod_row() {
        let mut config = ThetaDataConfig::new(None);
        config.base_url = observed_theta_server(
            "{\"created\":\"2024-11-18T21:01:00Z\",\"open\":586.24,\"high\":589.49,\"low\":585.34,\"close\":588.15,\"volume\":36905686}\n",
        );
        let provider = ThetaDataHistoricalDataProvider::new(config).unwrap();
        let data = provider
            .get_history(&option_universe_request(
                NaiveDate::from_ymd_opt(2024, 11, 18).unwrap(),
            ))
            .await
            .unwrap();
        let HistoricalData::OptionUniverse(rows) = data else {
            panic!("expected option universe");
        };
        let underlying = rows
            .iter()
            .find(|row| row.expiration.is_none())
            .expect("underlying row");
        assert_eq!(underlying.open, Decimal::from_str_exact("586.24").unwrap());
        assert_eq!(underlying.high, Decimal::from_str_exact("589.49").unwrap());
        assert_eq!(underlying.low, Decimal::from_str_exact("585.34").unwrap());
        assert_eq!(underlying.close, Decimal::from_str_exact("588.15").unwrap());
        assert_eq!(underlying.volume, Decimal::from(36_905_686));
    }

    #[tokio::test]
    async fn option_universe_rejects_missing_underlying_eod_row() {
        let mut config = ThetaDataConfig::new(None);
        config.base_url = observed_theta_server("");
        let provider = ThetaDataHistoricalDataProvider::new(config).unwrap();
        let result = provider
            .get_history(&option_universe_request(
                NaiveDate::from_ymd_opt(2024, 11, 18).unwrap(),
            ))
            .await;
        assert!(result.is_err(), "missing underlying data must fail closed");
    }

    #[test]
    fn option_query_bound_includes_lean_selection_session_gap() {
        let thursday = NaiveDate::from_ymd_opt(2025, 1, 2).unwrap();
        let friday = NaiveDate::from_ymd_opt(2025, 1, 3).unwrap();
        assert_eq!(option_query_max_dte(thursday, friday, 0), 1);

        let friday = NaiveDate::from_ymd_opt(2025, 1, 3).unwrap();
        let monday = NaiveDate::from_ymd_opt(2025, 1, 6).unwrap();
        assert_eq!(option_query_max_dte(friday, monday, 0), 3);
    }

    #[test]
    fn parses_stock_quote_as_new_york_time() {
        let row = parse_quote(&json!({
            "timestamp": "2026-07-13T09:30:00.000",
            "bid": 752.47,
            "ask": 752.51,
            "bid_size": 200,
            "ask_size": 240
        }))
        .expect("valid quote");
        assert_eq!(
            row.time.to_tz(New_York).format("%H:%M").to_string(),
            "09:30"
        );
    }

    #[test]
    fn exact_option_identity_accepts_dollar_and_mill_strikes() {
        let contract = OptionContract {
            root: "SPY".to_string(),
            expiration: NaiveDate::from_ymd_opt(2026, 8, 7).unwrap(),
            strike: Decimal::from(650),
            right: OptionRight::Call,
        };
        assert!(strike_matches(650.0, contract.strike));
        assert!(strike_matches(650_000.0, contract.strike));
        assert!(!strike_matches(651.0, contract.strike));
    }

    #[test]
    fn supports_equity_and_option_market_requests_and_option_universes() {
        let equity = Symbol::create_equity("SPY", &Market::usa());
        let request = HistoryRequest::new(
            SubscriptionDataConfig::new_equity(
                equity.clone(),
                Resolution::Daily,
                DataNormalizationMode::Raw,
            ),
            NanosecondTimestamp(1),
            NanosecondTimestamp(2),
        )
        .unwrap();
        let provider = ThetaDataHistoricalDataProvider::new(ThetaDataConfig::new(None)).unwrap();
        assert!(provider.supports(&request));

        let canonical = Symbol::create_canonical_option(&equity, &Market::usa());
        let chain = HistoryRequest::new(
            SubscriptionDataConfig::new_option_chain(
                canonical,
                Resolution::Minute,
                OptionChainSubscriptionMetadata {
                    canonical_permtick: "?SPY".to_string(),
                    underlying_ticker: "SPY".to_string(),
                    filter: OptionChainFilterMetadata {
                        min_strike_rank: -5,
                        max_strike_rank: 5,
                        min_expiry_days: 0,
                        max_expiry_days: 0,
                    },
                },
            ),
            NanosecondTimestamp(1),
            NanosecondTimestamp(2),
        )
        .unwrap();
        assert!(provider.supports(&chain));
    }

    #[test]
    fn daily_bar_uses_exchange_session_bounds() {
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let date = NaiveDate::from_ymd_opt(2026, 7, 6).unwrap();
        let (open, close) = MarketHoursDatabase::global()
            .exchange_hours(&symbol)
            .session_bounds(date)
            .unwrap();
        let request = HistoryRequest::new(
            SubscriptionDataConfig::new_equity(
                symbol,
                Resolution::Daily,
                DataNormalizationMode::Raw,
            ),
            open,
            close + TimeSpan::from_nanos(1),
        )
        .unwrap();
        let bar = daily_trade_bar(
            &request,
            EodRow {
                date,
                open: 1.0,
                high: 2.0,
                low: 0.5,
                close: 1.5,
                volume: 10.0,
            },
        )
        .unwrap()
        .unwrap();
        assert_eq!(bar.time, open);
        assert_eq!(bar.end_time, close);
    }
}
