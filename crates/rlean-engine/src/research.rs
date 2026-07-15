use chrono::NaiveDate;
use rlean_core::{DataNormalizationMode, DateTime, Market, Resolution, Symbol};
use rlean_data::SubscriptionDataConfig;
use rlean_data_sidecar::{
    decode_batch, CanonicalDataBatch, DataSidecarClient, DataSidecarConfig, DeliveryMode,
    WireDataType,
};
use rlean_data_tables::TradeBar;
use rlean_indicators::{indicator::Indicator, Atr, BollingerBands, Ema, Macd, Rsi, Sma};
pub use rlean_sdk::research::IndicatorResult;
use rlean_sdk::research::{date_str_from_ns, ResearchBackend};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use tokio_stream::StreamExt;
use tracing::warn;

/// A persistent sidecar session held on a dedicated runtime because the public
/// QuantBook API is synchronous.
struct SidecarRuntimeHandle {
    runtime: tokio::runtime::Runtime,
    client: Arc<DataSidecarClient>,
}

impl SidecarRuntimeHandle {
    fn connect_from_env() -> anyhow::Result<Self> {
        let endpoint = env_var("RLEAN_DATA_SIDECAR").ok_or_else(|| {
            anyhow::anyhow!("RLEAN_DATA_SIDECAR must be set to the Arrow Flight endpoint")
        })?;
        let config = DataSidecarConfig {
            endpoint,
            token: env_var("RLEAN_DATA_SIDECAR_TOKEN"),
            connect_timeout_ms: 10_000,
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()?;
        let client = runtime.block_on(DataSidecarClient::connect(config))?;
        Ok(Self {
            runtime,
            client: Arc::new(client),
        })
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub struct ResearchEngine {
    start_date: NaiveDate,
    end_date: NaiveDate,
    securities: HashMap<String, Symbol>,
    /// Lazily-connected Flight session kept alive for the engine's lifetime.
    sidecar: OnceLock<Option<SidecarRuntimeHandle>>,
}

impl ResearchEngine {
    pub fn new() -> Self {
        let today = chrono::Utc::now().date_naive();
        Self {
            start_date: today - chrono::Duration::days(365),
            end_date: today,
            securities: HashMap::new(),
            sidecar: OnceLock::new(),
        }
    }

    pub fn set_start_date(&mut self, year: i32, month: u32, day: u32) {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
            self.start_date = date;
        } else {
            warn!("Invalid start date: {}-{}-{}", year, month, day);
        }
    }

    pub fn set_end_date(&mut self, year: i32, month: u32, day: u32) {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
            self.end_date = date;
        } else {
            warn!("Invalid end date: {}-{}-{}", year, month, day);
        }
    }

    pub fn add_equity(&mut self, ticker: &str) -> Symbol {
        let symbol = Symbol::create_equity(ticker, &Market::usa());
        self.securities
            .insert(ticker.to_uppercase(), symbol.clone());
        symbol
    }

    pub fn add_option(&mut self, ticker: &str) -> Symbol {
        let canonical = format!("?{}", ticker.to_uppercase());
        let symbol = Symbol::create_equity(&canonical, &Market::usa());
        self.securities.insert(canonical, symbol.clone());
        symbol
    }

    pub fn add_future(&mut self, ticker: &str) -> Symbol {
        let symbol = Symbol::create_equity(ticker, &Market::usa());
        self.securities
            .insert(ticker.to_uppercase(), symbol.clone());
        symbol
    }

    pub fn symbol_for(&self, ticker: &str) -> Symbol {
        let upper = ticker.to_uppercase();
        self.securities
            .get(&upper)
            .cloned()
            .unwrap_or_else(|| Symbol::create_equity(&upper, &Market::usa()))
    }

    pub fn history_count(
        &self,
        symbol: &Symbol,
        bar_count: usize,
        resolution: Resolution,
    ) -> Vec<TradeBar> {
        let all = self.history_range(symbol, resolution, self.start_date, self.end_date);
        if all.len() <= bar_count {
            all
        } else {
            all[all.len() - bar_count..].to_vec()
        }
    }

    pub fn history_range(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Vec<TradeBar> {
        self.load_bars_from_sidecar(symbol, resolution, start, end)
    }

    pub fn indicator(
        &self,
        name: &str,
        symbol: &Symbol,
        period: usize,
        bar_count: usize,
        resolution: Resolution,
    ) -> IndicatorResult {
        let bars = self.history_count(symbol, bar_count, resolution);
        if bars.is_empty() {
            return IndicatorResult::default();
        }
        run_indicator(name, period, &bars)
    }

    pub fn option_chain(&self, ticker: &str) -> Vec<HashMap<String, String>> {
        warn!(
            "option_chain('{}') called but no options data provider is configured; returning empty list",
            ticker
        );
        vec![]
    }

    pub fn last_price(&self, symbol: &Symbol) -> Option<f64> {
        self.history_count(symbol, 1, Resolution::Daily)
            .last()
            .and_then(|bar| bar.close.to_f64())
    }

    pub fn start_date(&self) -> NaiveDate {
        self.start_date
    }

    pub fn end_date(&self) -> NaiveDate {
        self.end_date
    }

    pub fn security_keys(&self) -> Vec<String> {
        self.securities.keys().cloned().collect()
    }

    fn sidecar_handle(&self) -> Option<&SidecarRuntimeHandle> {
        self.sidecar
            .get_or_init(|| match SidecarRuntimeHandle::connect_from_env() {
                Ok(handle) => Some(handle),
                Err(error) => {
                    warn!("research: failed to connect sidecar: {error:#}");
                    None
                }
            })
            .as_ref()
    }

    fn load_bars_from_sidecar(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Vec<TradeBar> {
        let Some(handle) = self.sidecar_handle() else {
            return Vec::new();
        };
        let start_dt = date_to_datetime(start, 0, 0, 0);
        let end_dt = date_to_datetime(end, 23, 59, 59);
        let config = SubscriptionDataConfig::new_equity(
            symbol.clone(),
            resolution,
            DataNormalizationMode::Raw,
        );
        let client = handle.client.clone();
        let symbol = symbol.clone();
        handle.runtime.block_on(async move {
            let subscription_id = match client
                .add_subscription(&config, DeliveryMode::Backtest)
                .await
            {
                Ok(id) => id,
                Err(error) => {
                    warn!("research: failed to add sidecar subscription: {error:#}");
                    return Vec::new();
                }
            };
            let mut bars = Vec::new();
            match client.query(subscription_id, start_dt.0, end_dt.0).await {
                Ok(mut stream) => {
                    while let Some(item) = stream.next().await {
                        match item
                            .and_then(|batch| decode_batch(WireDataType::TradeBar, batch, &symbol))
                        {
                            Ok(CanonicalDataBatch::TradeBars(mut rows)) => bars.append(&mut rows),
                            Ok(_) => warn!("research: sidecar returned a non-TradeBar batch"),
                            Err(error) => warn!("research: sidecar query failed: {error:#}"),
                        }
                    }
                }
                Err(error) => warn!("research: failed to start sidecar query: {error:#}"),
            }
            if let Err(error) = client.remove_subscription(subscription_id).await {
                warn!("research: failed to remove sidecar subscription: {error:#}");
            }
            bars
        })
    }
}

impl Default for ResearchEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl ResearchBackend for ResearchEngine {
    fn set_start_date(&mut self, year: i32, month: u32, day: u32) {
        Self::set_start_date(self, year, month, day);
    }

    fn set_end_date(&mut self, year: i32, month: u32, day: u32) {
        Self::set_end_date(self, year, month, day);
    }

    fn add_equity(&mut self, ticker: &str) -> Symbol {
        Self::add_equity(self, ticker)
    }

    fn add_option(&mut self, ticker: &str) -> Symbol {
        Self::add_option(self, ticker)
    }

    fn add_future(&mut self, ticker: &str) -> Symbol {
        Self::add_future(self, ticker)
    }

    fn symbol_for(&self, ticker: &str) -> Symbol {
        Self::symbol_for(self, ticker)
    }

    fn history_count(
        &self,
        symbol: &Symbol,
        bar_count: usize,
        resolution: Resolution,
    ) -> Vec<TradeBar> {
        Self::history_count(self, symbol, bar_count, resolution)
    }

    fn history_range(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Vec<TradeBar> {
        Self::history_range(self, symbol, resolution, start, end)
    }

    fn indicator(
        &self,
        name: &str,
        symbol: &Symbol,
        period: usize,
        bar_count: usize,
        resolution: Resolution,
    ) -> IndicatorResult {
        Self::indicator(self, name, symbol, period, bar_count, resolution)
    }

    fn option_chain(&self, ticker: &str) -> Vec<HashMap<String, String>> {
        Self::option_chain(self, ticker)
    }

    fn last_price(&self, symbol: &Symbol) -> Option<f64> {
        Self::last_price(self, symbol)
    }

    fn start_date(&self) -> NaiveDate {
        Self::start_date(self)
    }

    fn end_date(&self) -> NaiveDate {
        Self::end_date(self)
    }

    fn security_keys(&self) -> Vec<String> {
        Self::security_keys(self)
    }
}

fn date_to_datetime(date: NaiveDate, h: u32, m: u32, s: u32) -> DateTime {
    use chrono::{TimeZone, Utc};
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap_or_default()))
}

fn run_indicator(name: &str, period: usize, bars: &[TradeBar]) -> IndicatorResult {
    match name.to_uppercase().as_str() {
        "SMA" => run_single(bars, &mut Sma::new(period)),
        "EMA" => run_single(bars, &mut Ema::new(period)),
        "RSI" => run_single(bars, &mut Rsi::new(period)),
        "ATR" => run_single(bars, &mut Atr::new(period)),
        "MACD" => {
            let slow = (period * 2 + 2).max(period + 1);
            let signal = (period / 2).max(1);
            run_macd(bars, period, slow, signal)
        }
        "BB" | "BOLLINGERBANDS" | "BOLLINGER" => run_bb(bars, period, dec!(2.0)),
        other => {
            warn!("Unknown indicator '{}' — returning empty result", other);
            IndicatorResult::default()
        }
    }
}

fn run_single(bars: &[TradeBar], indicator: &mut dyn Indicator) -> IndicatorResult {
    let mut result = IndicatorResult::default();
    for bar in bars {
        let value = indicator.update_bar(bar);
        if value.is_ready() {
            result.time.push(date_str_from_ns(bar.time.0));
            result.value.push(value.value.to_f64().unwrap_or(0.0));
        }
    }
    result
}

fn run_macd(bars: &[TradeBar], fast: usize, slow: usize, signal: usize) -> IndicatorResult {
    let mut indicator = Macd::new(fast, slow, signal);
    let mut result = IndicatorResult::default();
    for bar in bars {
        let value = indicator.update_bar(bar);
        if value.is_ready() {
            result.time.push(date_str_from_ns(bar.time.0));
            result
                .value
                .push(indicator.macd_line.to_f64().unwrap_or(0.0));
            result
                .signal
                .push(indicator.signal_line.to_f64().unwrap_or(0.0));
            result
                .histogram
                .push(indicator.histogram.to_f64().unwrap_or(0.0));
        }
    }
    result
}

fn run_bb(bars: &[TradeBar], period: usize, k: Decimal) -> IndicatorResult {
    let mut indicator = BollingerBands::new(period, k);
    let mut result = IndicatorResult::default();
    for bar in bars {
        let value = indicator.update_bar(bar);
        if value.is_ready() {
            result.time.push(date_str_from_ns(bar.time.0));
            result.value.push(indicator.middle.to_f64().unwrap_or(0.0));
            result.upper.push(indicator.upper.to_f64().unwrap_or(0.0));
            result.lower.push(indicator.lower.to_f64().unwrap_or(0.0));
        }
    }
    result
}
