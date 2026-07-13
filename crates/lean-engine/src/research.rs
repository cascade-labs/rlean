use chrono::NaiveDate;
use lean_core::{DateTime, Market, Resolution, Symbol, TickType};
use lean_data::TradeBar;
use lean_indicators::{indicator::Indicator, Atr, BollingerBands, Ema, Macd, Rsi, Sma};
pub use lean_sdk::research::IndicatorResult;
use lean_sdk::research::{date_str_from_ns, ResearchBackend};
use lean_storage::{IcebergStore, QueryParams, RestCatalogConfig, SigV4Config, DEFAULT_NAMESPACE};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tracing::warn;

#[derive(Debug, Clone, Default)]
pub enum ResearchDataProviderConfig {
    /// Read history from the REST Iceberg catalog (the default market-data
    /// store). The catalog connection is resolved from the `RLEAN_DATA_*`
    /// environment, not from a filesystem path.
    #[default]
    Catalog,
    ThetaData {
        api_token: String,
    },
    Polygon {
        api_key: String,
    },
}

/// A REST catalog store held on a dedicated long-lived runtime thread.
///
/// The research kernel's history calls are synchronous, but the store connect
/// starts a SigV4 signing proxy as a background task that must outlive the
/// store. Connecting on a throwaway current-thread runtime that is dropped
/// after `block_on` would abort that proxy task. This handle owns a
/// multi-thread runtime on its own thread for the whole process, so the proxy
/// stays alive and every `block_on` runs on that same runtime.
struct StoreRuntimeHandle {
    runtime: tokio::runtime::Runtime,
    store: Arc<IcebergStore>,
}

impl StoreRuntimeHandle {
    fn connect_from_env() -> anyhow::Result<Self> {
        let config = catalog_config_from_env()?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(1)
            .build()?;
        let store = runtime.block_on(IcebergStore::connect(config))?;
        Ok(Self {
            runtime,
            store: Arc::new(store),
        })
    }
}

/// Resolve the REST catalog connection for research from the `RLEAN_DATA_*`
/// environment, matching the maintenance binary's contract. Research runs in
/// the same process environment as `rlean`, which sets these before launching
/// the kernel.
fn catalog_config_from_env() -> anyhow::Result<RestCatalogConfig> {
    let uri = env_var("RLEAN_DATA_CATALOG").ok_or_else(|| {
        anyhow::anyhow!("RLEAN_DATA_CATALOG must be set to the REST catalog base URI")
    })?;
    let warehouse = env_var("RLEAN_DATA_WAREHOUSE").ok_or_else(|| {
        anyhow::anyhow!("RLEAN_DATA_WAREHOUSE must be set to the warehouse identifier")
    })?;
    let sigv4 = env_var("RLEAN_DATA_SIGV4_REGION").map(|region| SigV4Config {
        region,
        signing_name: env_var("RLEAN_DATA_SIGV4_NAME").unwrap_or_else(|| "s3tables".to_string()),
    });
    let namespace =
        env_var("RLEAN_DATA_NAMESPACE").unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());
    Ok(RestCatalogConfig {
        uri,
        warehouse,
        sigv4,
        namespace,
    })
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
    provider: ResearchDataProviderConfig,
    /// Lazily-connected REST catalog store, kept alive for the engine's
    /// lifetime (see [`StoreRuntimeHandle`]). Connected on first history call.
    store: OnceLock<Option<StoreRuntimeHandle>>,
}

impl ResearchEngine {
    pub fn new() -> Self {
        let today = chrono::Utc::now().date_naive();
        Self {
            start_date: today - chrono::Duration::days(365),
            end_date: today,
            securities: HashMap::new(),
            provider: ResearchDataProviderConfig::default(),
            store: OnceLock::new(),
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

    /// Select the REST catalog as the history source.
    ///
    /// The market-data store is always the REST Iceberg catalog resolved from
    /// the `RLEAN_DATA_*` environment; the supplied path is not a store path and
    /// is ignored for reads. The parameter is retained for LEAN API
    /// compatibility (`QuantBook.set_data_folder`).
    pub fn set_data_folder(&mut self, _path: impl Into<PathBuf>) {
        self.provider = ResearchDataProviderConfig::Catalog;
    }

    pub fn set_thetadata_provider(&mut self, api_token: impl Into<String>) {
        self.provider = ResearchDataProviderConfig::ThetaData {
            api_token: api_token.into(),
        };
    }

    pub fn set_polygon_provider(&mut self, api_key: impl Into<String>) {
        self.provider = ResearchDataProviderConfig::Polygon {
            api_key: api_key.into(),
        };
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
        match &self.provider {
            ResearchDataProviderConfig::Catalog => {
                self.load_bars_from_catalog(symbol, resolution, start, end)
            }
            ResearchDataProviderConfig::ThetaData { .. }
            | ResearchDataProviderConfig::Polygon { .. } => {
                warn!(
                    "Configured research provider is not implemented for {}; returning empty history",
                    symbol.value
                );
                vec![]
            }
        }
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

    /// The lazily-connected REST catalog store, or `None` when the catalog is
    /// not configured / connect failed (logged once).
    fn store_handle(&self) -> Option<&StoreRuntimeHandle> {
        self.store
            .get_or_init(|| match StoreRuntimeHandle::connect_from_env() {
                Ok(handle) => Some(handle),
                Err(error) => {
                    warn!("research: failed to connect REST catalog store: {error:#}");
                    None
                }
            })
            .as_ref()
    }

    fn load_bars_from_catalog(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Vec<TradeBar> {
        let Some(handle) = self.store_handle() else {
            return Vec::new();
        };
        let start_dt = date_to_datetime(start, 0, 0, 0);
        let end_dt = date_to_datetime(end, 23, 59, 59);
        let sid = symbol.id.sid;
        let params = QueryParams::new()
            .with_time_range(start_dt, end_dt)
            .with_symbols(vec![sid]);
        let symbol_clone = symbol.clone();
        let store = handle.store.clone();
        handle.runtime.block_on(async move {
            store
                .scan_trade_bar_partitions_grouped(
                    &HashMap::from([(sid, symbol_clone)]),
                    resolution,
                    TickType::Trade,
                    &params,
                )
                .await
                .unwrap_or_default()
                .remove(&sid)
                .unwrap_or_default()
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

    fn set_data_folder(&mut self, path: &str) {
        Self::set_data_folder(self, path);
    }

    fn set_thetadata_provider(&mut self, api_token: &str) {
        Self::set_thetadata_provider(self, api_token);
    }

    fn set_polygon_provider(&mut self, api_key: &str) {
        Self::set_polygon_provider(self, api_key);
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
