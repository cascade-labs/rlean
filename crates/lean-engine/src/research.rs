use chrono::NaiveDate;
use lean_core::{DateTime, Market, Resolution, Symbol, TickType};
use lean_data::TradeBar;
use lean_indicators::{indicator::Indicator, Atr, BollingerBands, Ema, Macd, Rsi, Sma};
pub use lean_sdk::research::IndicatorResult;
use lean_sdk::research::{date_str_from_ns, ResearchBackend};
use lean_storage::{IcebergStore, QueryParams};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Clone)]
pub enum ResearchDataProviderConfig {
    Local { data_folder: PathBuf },
    ThetaData { api_token: String },
    Polygon { api_key: String },
}

impl Default for ResearchDataProviderConfig {
    fn default() -> Self {
        Self::Local {
            data_folder: PathBuf::from("data"),
        }
    }
}

pub struct ResearchEngine {
    start_date: NaiveDate,
    end_date: NaiveDate,
    securities: HashMap<String, Symbol>,
    data_folder: PathBuf,
    provider: ResearchDataProviderConfig,
}

impl ResearchEngine {
    pub fn new() -> Self {
        let today = chrono::Utc::now().date_naive();
        Self {
            start_date: today - chrono::Duration::days(365),
            end_date: today,
            securities: HashMap::new(),
            data_folder: PathBuf::from("data"),
            provider: ResearchDataProviderConfig::default(),
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

    pub fn set_data_folder(&mut self, path: impl Into<PathBuf>) {
        self.data_folder = path.into();
        self.provider = ResearchDataProviderConfig::Local {
            data_folder: self.data_folder.clone(),
        };
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
            ResearchDataProviderConfig::Local { data_folder } => {
                Self::load_bars_local(data_folder, symbol, resolution, start, end)
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

    fn load_bars_local(
        data_folder: &Path,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Vec<TradeBar> {
        let start_dt = date_to_datetime(start, 0, 0, 0);
        let end_dt = date_to_datetime(end, 23, 59, 59);
        let sid = symbol.id.sid;
        let params = QueryParams::new()
            .with_time_range(start_dt, end_dt)
            .with_symbols(vec![sid]);
        let symbol_clone = symbol.clone();
        let data_folder = data_folder.to_path_buf();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| {
                rt.block_on(async move {
                    let store = IcebergStore::connect_local(data_folder).await.ok()?;
                    Some(
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
                            .unwrap_or_default(),
                    )
                })
            })
            .ok()
            .flatten()
            .unwrap_or_default()
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
