/// PyQuantBook — research-mode equivalent of QCAlgorithm.
///
/// Exposes historical data, indicators, and universe tools interactively,
/// mirroring the LEAN `QuantBook` API that Python research notebooks use.
///
/// ```python
/// from lean_rust import QuantBook, Resolution
///
/// qb = QuantBook()
/// qb.set_start_date(2022, 1, 1)
/// qb.set_end_date(2023, 1, 1)
/// qb.set_data_folder("/data")
///
/// equity = qb.add_equity("SPY")
/// df_dict = qb.history("SPY", 252, Resolution.DAILY)
///
/// ema_dict = qb.indicator("EMA", "SPY", 20, 252, Resolution.DAILY)
/// chain   = qb.option_chain("SPY")
/// price   = qb.get_last_price("SPY")
/// ```
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::NaiveDate;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use tracing::warn;

use lean_core::{DateTime, Market, Resolution, Symbol, TickType};
use lean_data::TradeBar;
use lean_indicators::{indicator::Indicator, Atr, BollingerBands, Ema, Macd, Rsi, Sma};
use lean_storage::{ParquetReader, PathResolver, QueryParams};

use crate::py_types::{PyResolution, PySecurity, PySymbol};

// ─── Data provider configuration ─────────────────────────────────────────────

/// Selects which data back-end `PyQuantBook` uses for history requests.
#[derive(Debug, Clone)]
pub enum DataProviderConfig {
    /// Load bars from local Parquet files written by the runner.
    Local { data_folder: PathBuf },
    /// Fetch bars from the ThetaData REST API.
    ThetaData { api_token: String },
    /// Fetch bars from the Polygon REST API.
    Polygon { api_key: String },
}

impl Default for DataProviderConfig {
    fn default() -> Self {
        DataProviderConfig::Local {
            data_folder: PathBuf::from("data"),
        }
    }
}

// ─── PyQuantBook ──────────────────────────────────────────────────────────────

/// Research-mode book — mirrors LEAN's `QuantBook` for interactive / notebook use.
#[pyclass(name = "QuantBook")]
pub struct PyQuantBook {
    /// Inclusive start date for history requests.
    start_date: NaiveDate,
    /// Inclusive end date for history requests.
    end_date: NaiveDate,
    /// Subscribed securities: uppercase ticker → Symbol.
    securities: HashMap<String, Symbol>,
    /// Root path for local Parquet data.
    data_folder: PathBuf,
    /// Active data provider.
    provider: DataProviderConfig,
}

// ─── Private helpers ──────────────────────────────────────────────────────────

impl PyQuantBook {
    /// Resolve a Symbol from a ticker string (upper-cased US equity).
    fn symbol_for(&self, ticker: &str) -> Symbol {
        let upper = ticker.to_uppercase();
        self.securities
            .get(&upper)
            .cloned()
            .unwrap_or_else(|| Symbol::create_equity(&upper, &Market::usa()))
    }

    /// Resolve the symbol from a Python object that is either a `PySymbol`
    /// or a plain ticker string.
    fn resolve_symbol(&self, arg: &Bound<'_, PyAny>) -> PyResult<Symbol> {
        if let Ok(sym) = arg.cast::<PySymbol>() {
            return Ok(sym.get().inner.clone());
        }
        if let Ok(s) = arg.extract::<String>() {
            return Ok(self.symbol_for(&s));
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "Expected Symbol or ticker string",
        ))
    }

    /// Load trade bars from local Parquet files for [start, end] (inclusive).
    ///
    /// For daily/hour resolution a single file covers all dates; for
    /// minute/second resolution files are date-partitioned and we gather them
    /// with a glob scan.
    fn load_bars_local(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Vec<TradeBar> {
        let resolver = PathResolver::new(&self.data_folder);
        let paths: Vec<PathBuf> = if resolution.is_high_resolution() {
            let mut ps = Vec::new();
            let mut d = start;
            while d <= end {
                let p = resolver.market_data_partition(symbol, resolution, TickType::Trade, d);
                if p.exists() {
                    ps.push(p);
                }
                d += chrono::Duration::days(1);
            }
            ps
        } else {
            let p = resolver.market_data_partition(symbol, resolution, TickType::Trade, start);
            if p.exists() {
                vec![p]
            } else {
                vec![]
            }
        };

        if paths.is_empty() {
            warn!(
                "No local data found for {} ({:?}) in {:?}",
                symbol.value, resolution, self.data_folder,
            );
            return vec![];
        }

        // Build a time-range predicate.
        let start_dt = date_to_datetime(start, 0, 0, 0);
        let end_dt = date_to_datetime(end, 23, 59, 59);
        let params = QueryParams::new().with_time_range(start_dt, end_dt);

        // Run via a one-shot Tokio runtime (QuantBook is used interactively,
        // not inside an existing async context).
        let reader = Arc::new(ParquetReader::new());
        let symbol_clone = symbol.clone();

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|rt| {
                rt.block_on(async move {
                    let mut bars = Vec::new();
                    for path in paths {
                        bars.extend(
                            reader
                                .read_trade_bar_partition(&path, &symbol_clone, &params)
                                .unwrap_or_default()
                                .into_iter()
                                .filter(|bar| bar.symbol.id.sid == symbol_clone.id.sid),
                        );
                    }
                    bars
                })
            })
            .unwrap_or_default()
    }

    /// Load the most recent `bar_count` bars ending at `self.end_date`.
    fn load_bars_count(
        &self,
        symbol: &Symbol,
        bar_count: usize,
        resolution: Resolution,
    ) -> Vec<TradeBar> {
        let all = self.load_bars_local(symbol, resolution, self.start_date, self.end_date);
        if all.len() <= bar_count {
            all
        } else {
            all[all.len() - bar_count..].to_vec()
        }
    }
}

// ─── ns helpers ──────────────────────────────────────────────────────────────

fn date_to_datetime(date: NaiveDate, h: u32, m: u32, s: u32) -> DateTime {
    use chrono::{TimeZone, Utc};
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(h, m, s).unwrap_or_default()))
}

fn ns_to_date_str(ns: i64) -> String {
    use chrono::{DateTime as ChrDt, Utc};
    let secs = ns / 1_000_000_000;
    let nsub = (ns % 1_000_000_000) as u32;
    let dt: ChrDt<Utc> = chrono::DateTime::from_timestamp(secs, nsub).unwrap_or_default();
    dt.format("%Y-%m-%d").to_string()
}

fn f2d(f: f64) -> Decimal {
    Decimal::from_f64(f).unwrap_or_default()
}

// ─── pymethods ────────────────────────────────────────────────────────────────

#[pymethods]
impl PyQuantBook {
    // ── Constructor ──────────────────────────────────────────────────────────

    #[new]
    pub fn new() -> Self {
        let today = chrono::Utc::now().date_naive();
        PyQuantBook {
            start_date: today - chrono::Duration::days(365),
            end_date: today,
            securities: HashMap::new(),
            data_folder: PathBuf::from("data"),
            provider: DataProviderConfig::default(),
        }
    }

    // ── Configuration ────────────────────────────────────────────────────────

    /// Set the inclusive start date for history requests.
    fn set_start_date(&mut self, year: i32, month: u32, day: u32) {
        if let Some(d) = NaiveDate::from_ymd_opt(year, month, day) {
            self.start_date = d;
        } else {
            warn!("Invalid start date: {}-{}-{}", year, month, day);
        }
    }

    /// Set the inclusive end date for history requests.
    fn set_end_date(&mut self, year: i32, month: u32, day: u32) {
        if let Some(d) = NaiveDate::from_ymd_opt(year, month, day) {
            self.end_date = d;
        } else {
            warn!("Invalid end date: {}-{}-{}", year, month, day);
        }
    }

    /// Set the root folder for local Parquet data files.
    fn set_data_folder(&mut self, path: &str) {
        self.data_folder = PathBuf::from(path);
        self.provider = DataProviderConfig::Local {
            data_folder: self.data_folder.clone(),
        };
    }

    /// Configure ThetaData as the history provider.
    fn set_thetadata_provider(&mut self, api_token: &str) {
        self.provider = DataProviderConfig::ThetaData {
            api_token: api_token.to_string(),
        };
    }

    /// Configure Polygon as the history provider.
    fn set_polygon_provider(&mut self, api_key: &str) {
        self.provider = DataProviderConfig::Polygon {
            api_key: api_key.to_string(),
        };
    }

    // ── Universe ─────────────────────────────────────────────────────────────

    /// Subscribe to an equity and return a `Security` object with a `.symbol`.
    #[pyo3(signature = (ticker))]
    fn add_equity(&mut self, ticker: &str) -> PySecurity {
        let sym = Symbol::create_equity(ticker, &Market::usa());
        self.securities.insert(ticker.to_uppercase(), sym.clone());
        PySecurity::from_symbol(PySymbol { inner: sym })
    }

    /// Subscribe to an option chain.  Returns a `Security` for the canonical
    /// option ticker (e.g. `?SPY`).
    #[pyo3(signature = (ticker))]
    fn add_option(&mut self, ticker: &str) -> PySecurity {
        // Create a canonical option symbol — the underlying equity permtick
        // prefixed with `?` matches LEAN's convention.
        let canonical = format!("?{}", ticker.to_uppercase());
        let sym = Symbol::create_equity(&canonical, &Market::usa());
        self.securities.insert(canonical.clone(), sym.clone());
        PySecurity::from_symbol(PySymbol { inner: sym })
    }

    /// Subscribe to a futures contract.  Returns a `Security` for the ticker.
    #[pyo3(signature = (ticker))]
    fn add_future(&mut self, ticker: &str) -> PySecurity {
        let sym = Symbol::create_equity(ticker, &Market::usa());
        self.securities.insert(ticker.to_uppercase(), sym.clone());
        PySecurity::from_symbol(PySymbol { inner: sym })
    }

    // ── History ───────────────────────────────────────────────────────────────

    /// Return historical trade bars as a dict suitable for `pd.DataFrame(...)`.
    ///
    /// `symbol`     — ticker string or `Symbol` object
    /// `bar_count`  — number of bars to return (most recent)
    /// `resolution` — `Resolution.DAILY`, `Resolution.MINUTE`, etc.
    ///
    /// Returns `{"time": [...], "open": [...], "high": [...], "low": [...],
    ///           "close": [...], "volume": [...]}`.
    #[pyo3(signature = (symbol, bar_count, resolution))]
    fn history(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
        bar_count: usize,
        resolution: PyResolution,
    ) -> PyResult<Py<PyAny>> {
        let sym = self.resolve_symbol(symbol)?;
        let res: Resolution = resolution.into();

        let bars = self.load_bars_count(&sym, bar_count, res);

        bars_to_pydict(py, &bars)
    }

    /// Return historical trade bars for an explicit date range.
    ///
    /// `symbol`     — ticker string or `Symbol` object
    /// `start`      — (year, month, day) tuple
    /// `end`        — (year, month, day) tuple
    /// `resolution` — Resolution
    ///
    /// Returns the same dict schema as `history()`.
    #[pyo3(signature = (symbol, start, end, resolution))]
    fn history_range(
        &self,
        py: Python<'_>,
        symbol: &Bound<'_, PyAny>,
        start: (i32, u32, u32),
        end: (i32, u32, u32),
        resolution: PyResolution,
    ) -> PyResult<Py<PyAny>> {
        let sym = self.resolve_symbol(symbol)?;
        let res: Resolution = resolution.into();

        let start_date = NaiveDate::from_ymd_opt(start.0, start.1, start.2)
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Invalid start date"))?;
        let end_date = NaiveDate::from_ymd_opt(end.0, end.1, end.2)
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("Invalid end date"))?;

        let bars = self.load_bars_local(&sym, res, start_date, end_date);
        bars_to_pydict(py, &bars)
    }

    // ── Indicators ────────────────────────────────────────────────────────────

    /// Compute an indicator over historical bars.
    ///
    /// `name`       — "SMA", "EMA", "RSI", "MACD", "BB", "ATR" (case-insensitive)
    /// `symbol`     — ticker string or `Symbol`
    /// `period`     — look-back window
    /// `bar_count`  — bars of history to feed (must be >= period)
    /// `resolution` — Resolution
    ///
    /// Returns `{"time": [...], "value": [...]}`.
    /// For MACD returns `{"time": [...], "value": [...], "signal": [...], "histogram": [...]}`.
    /// For BB  returns `{"time": [...], "value": [...], "upper": [...], "lower": [...]}`.
    #[pyo3(signature = (name, symbol, period, bar_count, resolution))]
    fn indicator(
        &self,
        py: Python<'_>,
        name: &str,
        symbol: &Bound<'_, PyAny>,
        period: usize,
        bar_count: usize,
        resolution: PyResolution,
    ) -> PyResult<Py<PyAny>> {
        let sym = self.resolve_symbol(symbol)?;
        let res: Resolution = resolution.into();
        let bars = self.load_bars_count(&sym, bar_count, res);

        if bars.is_empty() {
            let dict = PyDict::new(py);
            dict.set_item("time", PyList::empty(py))?;
            dict.set_item("value", PyList::empty(py))?;
            return Ok(dict.into());
        }

        run_indicator(py, name, period, &bars)
    }

    // ── Option chain ──────────────────────────────────────────────────────────

    /// Return a snapshot of the option chain for `ticker`.
    ///
    /// Each element in the returned list is a dict with:
    /// `symbol`, `strike`, `expiry`, `right`, `last_price`, `bid`, `ask`,
    /// `volume`, `open_interest`.
    ///
    /// Returns an empty list when no options data is available locally.
    #[pyo3(signature = (ticker))]
    fn option_chain(&self, py: Python<'_>, ticker: &str) -> PyResult<Py<PyAny>> {
        // Local option chain data is not implemented yet.
        // Return an empty list with the correct schema so callers can rely on it.
        warn!(
            "option_chain('{}') called but no options data provider is configured — \
             returning empty list",
            ticker
        );
        Ok(PyList::empty(py).into())
    }

    // ── Last price ────────────────────────────────────────────────────────────

    /// Return the most recent close price for `symbol`.
    ///
    /// Loads the last bar from the local data store and returns its close.
    /// Returns `None` when data is not available.
    #[pyo3(signature = (symbol))]
    fn get_last_price(&self, symbol: &Bound<'_, PyAny>) -> PyResult<Option<f64>> {
        let sym = self.resolve_symbol(symbol)?;
        let bars = self.load_bars_count(&sym, 1, Resolution::Daily);
        Ok(bars.last().and_then(|b| b.close.to_f64()))
    }

    // ── Repr ─────────────────────────────────────────────────────────────────

    fn __repr__(&self) -> String {
        format!(
            "QuantBook(start={}, end={}, securities=[{}])",
            self.start_date,
            self.end_date,
            self.securities
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

impl Default for PyQuantBook {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Helper: build history dict ───────────────────────────────────────────────

fn bars_to_pydict(py: Python<'_>, bars: &[TradeBar]) -> PyResult<Py<PyAny>> {
    let mut times: Vec<String> = Vec::with_capacity(bars.len());
    let mut opens: Vec<f64> = Vec::with_capacity(bars.len());
    let mut highs: Vec<f64> = Vec::with_capacity(bars.len());
    let mut lows: Vec<f64> = Vec::with_capacity(bars.len());
    let mut closes: Vec<f64> = Vec::with_capacity(bars.len());
    let mut volumes: Vec<f64> = Vec::with_capacity(bars.len());

    for b in bars {
        times.push(ns_to_date_str(b.time.0));
        opens.push(b.open.to_f64().unwrap_or(0.0));
        highs.push(b.high.to_f64().unwrap_or(0.0));
        lows.push(b.low.to_f64().unwrap_or(0.0));
        closes.push(b.close.to_f64().unwrap_or(0.0));
        volumes.push(b.volume.to_f64().unwrap_or(0.0));
    }

    let dict = PyDict::new(py);
    dict.set_item("time", times)?;
    dict.set_item("open", opens)?;
    dict.set_item("high", highs)?;
    dict.set_item("low", lows)?;
    dict.set_item("close", closes)?;
    dict.set_item("volume", volumes)?;
    Ok(dict.into())
}

// ─── Helper: run indicator over bars ─────────────────────────────────────────

fn run_indicator(
    py: Python<'_>,
    name: &str,
    period: usize,
    bars: &[TradeBar],
) -> PyResult<Py<PyAny>> {
    match name.to_uppercase().as_str() {
        "SMA" => run_single(py, bars, &mut Sma::new(period)),
        "EMA" => run_single(py, bars, &mut Ema::new(period)),
        "RSI" => run_single(py, bars, &mut Rsi::new(period)),
        "ATR" => run_single(py, bars, &mut Atr::new(period)),
        "MACD" => {
            // Standard MACD defaults: fast=12, slow=26, signal=9.
            // The user-supplied `period` tunes the fast period.
            let slow = (period * 2 + 2).max(period + 1);
            let signal = (period / 2).max(1);
            run_macd(py, bars, period, slow, signal)
        }
        "BB" | "BOLLINGERBANDS" | "BOLLINGER" => run_bb(py, bars, period, f2d(2.0)),
        other => {
            warn!("Unknown indicator '{}' — returning empty result", other);
            let dict = PyDict::new(py);
            dict.set_item("time", PyList::empty(py))?;
            dict.set_item("value", PyList::empty(py))?;
            Ok(dict.into())
        }
    }
}

/// Run any single-value indicator and collect ready results into a dict.
fn run_single(py: Python<'_>, bars: &[TradeBar], ind: &mut dyn Indicator) -> PyResult<Py<PyAny>> {
    let mut times: Vec<String> = Vec::new();
    let mut values: Vec<f64> = Vec::new();

    for bar in bars {
        let result = ind.update_bar(bar);
        if result.is_ready() {
            times.push(ns_to_date_str(bar.time.0));
            values.push(result.value.to_f64().unwrap_or(0.0));
        }
    }

    let dict = PyDict::new(py);
    dict.set_item("time", times)?;
    dict.set_item("value", values)?;
    Ok(dict.into())
}

fn run_macd(
    py: Python<'_>,
    bars: &[TradeBar],
    fast: usize,
    slow: usize,
    signal: usize,
) -> PyResult<Py<PyAny>> {
    let mut ind = Macd::new(fast, slow, signal);
    let mut times: Vec<String> = Vec::new();
    let mut values: Vec<f64> = Vec::new();
    let mut signals: Vec<f64> = Vec::new();
    let mut histograms: Vec<f64> = Vec::new();

    for bar in bars {
        let result = ind.update_bar(bar);
        if result.is_ready() {
            times.push(ns_to_date_str(bar.time.0));
            values.push(ind.macd_line.to_f64().unwrap_or(0.0));
            signals.push(ind.signal_line.to_f64().unwrap_or(0.0));
            histograms.push(ind.histogram.to_f64().unwrap_or(0.0));
        }
    }

    let dict = PyDict::new(py);
    dict.set_item("time", times)?;
    dict.set_item("value", values)?;
    dict.set_item("signal", signals)?;
    dict.set_item("histogram", histograms)?;
    Ok(dict.into())
}

fn run_bb(py: Python<'_>, bars: &[TradeBar], period: usize, k: Decimal) -> PyResult<Py<PyAny>> {
    let mut ind = BollingerBands::new(period, k);
    let mut times: Vec<String> = Vec::new();
    let mut middles: Vec<f64> = Vec::new();
    let mut uppers: Vec<f64> = Vec::new();
    let mut lowers: Vec<f64> = Vec::new();

    for bar in bars {
        let result = ind.update_bar(bar);
        if result.is_ready() {
            times.push(ns_to_date_str(bar.time.0));
            middles.push(ind.middle.to_f64().unwrap_or(0.0));
            uppers.push(ind.upper.to_f64().unwrap_or(0.0));
            lowers.push(ind.lower.to_f64().unwrap_or(0.0));
        }
    }

    let dict = PyDict::new(py);
    dict.set_item("time", times)?;
    dict.set_item("value", middles)?;
    dict.set_item("upper", uppers)?;
    dict.set_item("lower", lowers)?;
    Ok(dict.into())
}
