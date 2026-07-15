//! SDK-owned research and QuantBook APIs.

use chrono::NaiveDate;
use rlean_core::{Market, OptionRight, OptionStyle, Resolution, Symbol};
use rlean_data_tables::TradeBar;
use rust_decimal::prelude::ToPrimitive;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Default)]
pub struct IndicatorResult {
    pub time: Vec<String>,
    pub value: Vec<f64>,
    pub signal: Vec<f64>,
    pub histogram: Vec<f64>,
    pub upper: Vec<f64>,
    pub lower: Vec<f64>,
}

#[derive(Debug, Clone, Default)]
pub struct HistoryFrameView {
    pub time: Vec<String>,
    pub open: Vec<f64>,
    pub high: Vec<f64>,
    pub low: Vec<f64>,
    pub close: Vec<f64>,
    pub volume: Vec<f64>,
}

impl HistoryFrameView {
    pub fn from_trade_bars(bars: &[TradeBar]) -> Self {
        let mut view = Self {
            time: Vec::with_capacity(bars.len()),
            open: Vec::with_capacity(bars.len()),
            high: Vec::with_capacity(bars.len()),
            low: Vec::with_capacity(bars.len()),
            close: Vec::with_capacity(bars.len()),
            volume: Vec::with_capacity(bars.len()),
        };

        for bar in bars {
            view.time.push(date_str_from_ns(bar.time.0));
            view.open.push(decimal_to_f64(bar.open));
            view.high.push(decimal_to_f64(bar.high));
            view.low.push(decimal_to_f64(bar.low));
            view.close.push(decimal_to_f64(bar.close));
            view.volume.push(decimal_to_f64(bar.volume));
        }

        view
    }

    pub fn columns(&self) -> [(&'static str, &[f64]); 5] {
        [
            ("open", &self.open),
            ("high", &self.high),
            ("low", &self.low),
            ("close", &self.close),
            ("volume", &self.volume),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndicatorSeries {
    Signal,
    Histogram,
    Upper,
    Lower,
}

impl IndicatorSeries {
    pub fn name(self) -> &'static str {
        match self {
            Self::Signal => "signal",
            Self::Histogram => "histogram",
            Self::Upper => "upper",
            Self::Lower => "lower",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct IndicatorFrameView {
    result: IndicatorResult,
    extra_series: Vec<IndicatorSeries>,
}

impl IndicatorFrameView {
    pub fn new(name: &str, result: IndicatorResult) -> Self {
        let extra_series = match name.to_uppercase().as_str() {
            "MACD" => vec![IndicatorSeries::Signal, IndicatorSeries::Histogram],
            "BB" | "BOLLINGERBANDS" | "BOLLINGER" => {
                vec![IndicatorSeries::Upper, IndicatorSeries::Lower]
            }
            _ => vec![],
        };

        Self {
            result,
            extra_series,
        }
    }

    pub fn time(&self) -> &[String] {
        &self.result.time
    }

    pub fn value(&self) -> &[f64] {
        &self.result.value
    }

    pub fn extra_series(&self) -> impl Iterator<Item = (&'static str, &[f64])> {
        self.extra_series.iter().map(|series| {
            let values = match series {
                IndicatorSeries::Signal => &self.result.signal,
                IndicatorSeries::Histogram => &self.result.histogram,
                IndicatorSeries::Upper => &self.result.upper,
                IndicatorSeries::Lower => &self.result.lower,
            };
            (series.name(), values.as_slice())
        })
    }
}

#[derive(Debug, Clone)]
pub enum ResearchSymbol {
    Symbol(Symbol),
    Ticker(String),
}

impl ResearchSymbol {
    fn resolve(&self, book: &ResearchBook) -> Symbol {
        match self {
            Self::Symbol(symbol) => symbol.clone(),
            Self::Ticker(ticker) => book.symbol_for(ticker),
        }
    }
}

pub fn date_str_from_ns(ns: i64) -> String {
    use chrono::{DateTime as ChronoDateTime, Utc};
    let secs = ns / 1_000_000_000;
    let nanos = (ns % 1_000_000_000) as u32;
    let dt: ChronoDateTime<Utc> = chrono::DateTime::from_timestamp(secs, nanos).unwrap_or_default();
    dt.format("%Y-%m-%d").to_string()
}

fn decimal_to_f64(value: rust_decimal::Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

pub trait ResearchBackend: Send {
    fn set_start_date(&mut self, year: i32, month: u32, day: u32);
    fn set_end_date(&mut self, year: i32, month: u32, day: u32);
    fn add_equity(&mut self, ticker: &str) -> Symbol;
    fn add_option(&mut self, ticker: &str) -> Symbol;
    fn add_future(&mut self, ticker: &str) -> Symbol;
    fn symbol_for(&self, ticker: &str) -> Symbol;
    fn history_count(
        &self,
        symbol: &Symbol,
        bar_count: usize,
        resolution: Resolution,
    ) -> Vec<TradeBar>;
    fn history_range(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> Vec<TradeBar>;
    fn indicator(
        &self,
        name: &str,
        symbol: &Symbol,
        period: usize,
        bar_count: usize,
        resolution: Resolution,
    ) -> IndicatorResult;
    fn option_chain(&self, ticker: &str) -> Vec<HashMap<String, String>>;
    fn last_price(&self, symbol: &Symbol) -> Option<f64>;
    fn start_date(&self) -> chrono::NaiveDate;
    fn end_date(&self) -> chrono::NaiveDate;
    fn security_keys(&self) -> Vec<String>;
}

#[derive(Clone)]
#[cfg_attr(feature = "python", pyo3::pyclass(name = "QuantBook"))]
pub struct ResearchBook {
    backend: Arc<Mutex<Box<dyn ResearchBackend>>>,
}

impl ResearchBook {
    pub fn default_book() -> Self {
        Self::new(Box::new(EmptyResearchBackend::default()))
    }

    pub fn new(backend: Box<dyn ResearchBackend>) -> Self {
        Self {
            backend: Arc::new(Mutex::new(backend)),
        }
    }

    pub fn set_start_date(&self, year: i32, month: u32, day: u32) {
        self.backend
            .lock()
            .unwrap()
            .set_start_date(year, month, day);
    }

    pub fn set_end_date(&self, year: i32, month: u32, day: u32) {
        self.backend.lock().unwrap().set_end_date(year, month, day);
    }

    pub fn add_equity(&self, ticker: &str) -> Symbol {
        self.backend.lock().unwrap().add_equity(ticker)
    }

    pub fn add_option(&self, ticker: &str) -> Symbol {
        self.backend.lock().unwrap().add_option(ticker)
    }

    pub fn add_future(&self, ticker: &str) -> Symbol {
        self.backend.lock().unwrap().add_future(ticker)
    }

    pub fn symbol_for(&self, ticker: &str) -> Symbol {
        self.backend.lock().unwrap().symbol_for(ticker)
    }

    pub fn history_count(
        &self,
        symbol: &Symbol,
        bar_count: usize,
        resolution: Resolution,
    ) -> Vec<TradeBar> {
        self.backend
            .lock()
            .unwrap()
            .history_count(symbol, bar_count, resolution)
    }

    pub fn history_count_view(
        &self,
        symbol: &Symbol,
        bar_count: usize,
        resolution: Resolution,
    ) -> HistoryFrameView {
        HistoryFrameView::from_trade_bars(&self.history_count(symbol, bar_count, resolution))
    }

    pub fn history(&self, ticker: String, bar_count: usize, resolution: Resolution) -> String {
        let _ = self.history_for_symbol(ResearchSymbol::Ticker(ticker), bar_count, resolution);
        String::new()
    }

    pub fn history_for_symbol(
        &self,
        symbol: ResearchSymbol,
        bar_count: usize,
        resolution: Resolution,
    ) -> HistoryFrameView {
        let symbol = symbol.resolve(self);
        self.history_count_view(&symbol, bar_count, resolution)
    }

    pub fn history_range_bars(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> Vec<TradeBar> {
        self.backend
            .lock()
            .unwrap()
            .history_range(symbol, resolution, start, end)
    }

    pub fn history_range_view(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> HistoryFrameView {
        HistoryFrameView::from_trade_bars(&self.history_range_bars(symbol, resolution, start, end))
    }

    pub fn history_range(
        &self,
        ticker: String,
        start: NaiveDate,
        end: NaiveDate,
        resolution: Resolution,
    ) -> String {
        let _ =
            self.history_range_for_symbol(ResearchSymbol::Ticker(ticker), start, end, resolution);
        String::new()
    }

    pub fn history_range_for_symbol(
        &self,
        symbol: ResearchSymbol,
        start: NaiveDate,
        end: NaiveDate,
        resolution: Resolution,
    ) -> HistoryFrameView {
        let symbol = symbol.resolve(self);
        self.history_range_view(&symbol, resolution, start, end)
    }

    pub fn indicator_result(
        &self,
        name: &str,
        symbol: &Symbol,
        period: usize,
        bar_count: usize,
        resolution: Resolution,
    ) -> IndicatorResult {
        self.backend
            .lock()
            .unwrap()
            .indicator(name, symbol, period, bar_count, resolution)
    }

    pub fn indicator_view(
        &self,
        name: &str,
        symbol: &Symbol,
        period: usize,
        bar_count: usize,
        resolution: Resolution,
    ) -> IndicatorFrameView {
        IndicatorFrameView::new(
            name,
            self.indicator_result(name, symbol, period, bar_count, resolution),
        )
    }

    pub fn indicator(
        &self,
        name: &str,
        ticker: String,
        period: usize,
        bar_count: usize,
        resolution: Resolution,
    ) -> String {
        let symbol = ResearchSymbol::Ticker(ticker).resolve(self);
        let _ = self.indicator_result(name, &symbol, period, bar_count, resolution);
        String::new()
    }

    pub fn indicator_frame(
        &self,
        name: &str,
        ticker: String,
        period: usize,
        bar_count: usize,
        resolution: Resolution,
    ) -> String {
        let _ = self.indicator_frame_for_symbol(
            name,
            ResearchSymbol::Ticker(ticker),
            period,
            bar_count,
            resolution,
        );
        String::new()
    }

    pub fn indicator_frame_for_symbol(
        &self,
        name: &str,
        symbol: ResearchSymbol,
        period: usize,
        bar_count: usize,
        resolution: Resolution,
    ) -> IndicatorFrameView {
        let symbol = symbol.resolve(self);
        self.indicator_view(name, &symbol, period, bar_count, resolution)
    }

    pub fn option_chain(&self, ticker: &str) -> String {
        let _ = self.option_chain_rows(ticker);
        String::new()
    }

    pub fn option_chain_rows(&self, ticker: &str) -> Vec<HashMap<String, String>> {
        self.backend.lock().unwrap().option_chain(ticker)
    }

    pub fn get_last_price(&self, ticker: String) -> Option<f64> {
        self.get_last_price_for_symbol(ResearchSymbol::Ticker(ticker))
    }

    pub fn get_last_price_for_symbol(&self, symbol: ResearchSymbol) -> Option<f64> {
        let symbol = symbol.resolve(self);
        self.last_price(&symbol)
    }

    pub fn last_price(&self, symbol: &Symbol) -> Option<f64> {
        self.backend.lock().unwrap().last_price(symbol)
    }
    pub fn start_date(&self) -> chrono::NaiveDate {
        self.backend.lock().unwrap().start_date()
    }
    pub fn end_date(&self) -> chrono::NaiveDate {
        self.backend.lock().unwrap().end_date()
    }
    pub fn security_keys(&self) -> Vec<String> {
        self.backend.lock().unwrap().security_keys()
    }
}

#[cfg(feature = "python")]
#[pyo3::pymethods]
impl ResearchBook {
    #[new]
    fn py_new() -> Self {
        Self::default_book()
    }

    #[pyo3(name = "set_start_date")]
    fn py_set_start_date(&self, year: i32, month: u32, day: u32) {
        self.set_start_date(year, month, day);
    }

    #[pyo3(name = "set_end_date")]
    fn py_set_end_date(&self, year: i32, month: u32, day: u32) {
        self.set_end_date(year, month, day);
    }

    #[pyo3(name = "add_equity")]
    fn py_add_equity(&self, ticker: String) -> crate::securities::SymbolHandle {
        crate::securities::SymbolHandle::new(self.add_equity(&ticker))
    }

    #[pyo3(name = "add_option")]
    fn py_add_option(&self, ticker: String) -> crate::securities::SymbolHandle {
        crate::securities::SymbolHandle::new(self.add_option(&ticker))
    }

    #[pyo3(name = "history")]
    fn py_history(
        &self,
        ticker: String,
        bar_count: usize,
        resolution: crate::types::Resolution,
    ) -> String {
        self.history(ticker, bar_count, resolution.into())
    }

    #[pyo3(name = "history_range")]
    fn py_history_range(
        &self,
        ticker: String,
        start: NaiveDate,
        end: NaiveDate,
        resolution: crate::types::Resolution,
    ) -> String {
        self.history_range(ticker, start, end, resolution.into())
    }

    #[pyo3(name = "indicator")]
    fn py_indicator(
        &self,
        name: &str,
        ticker: String,
        period: usize,
        bar_count: usize,
        resolution: crate::types::Resolution,
    ) -> String {
        self.indicator(name, ticker, period, bar_count, resolution.into())
    }

    #[pyo3(name = "indicator_frame")]
    fn py_indicator_frame(
        &self,
        name: &str,
        ticker: String,
        period: usize,
        bar_count: usize,
        resolution: crate::types::Resolution,
    ) -> String {
        self.indicator_frame(name, ticker, period, bar_count, resolution.into())
    }

    #[pyo3(name = "option_chain")]
    fn py_option_chain(&self, ticker: &str) -> String {
        self.option_chain(ticker)
    }

    #[pyo3(name = "get_last_price")]
    fn py_get_last_price(&self, ticker: String) -> Option<f64> {
        self.get_last_price(ticker)
    }
}

struct EmptyResearchBackend {
    start_date: NaiveDate,
    end_date: NaiveDate,
    securities: HashMap<String, Symbol>,
}

impl Default for EmptyResearchBackend {
    fn default() -> Self {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        Self {
            start_date: epoch,
            end_date: epoch,
            securities: HashMap::new(),
        }
    }
}

impl EmptyResearchBackend {
    fn symbol_for_ticker(&mut self, ticker: &str) -> Symbol {
        self.securities
            .entry(ticker.to_ascii_uppercase())
            .or_insert_with(|| Symbol::create_equity(ticker, &Market::usa()))
            .clone()
    }
}

impl ResearchBackend for EmptyResearchBackend {
    fn set_start_date(&mut self, year: i32, month: u32, day: u32) {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
            self.start_date = date;
        }
    }

    fn set_end_date(&mut self, year: i32, month: u32, day: u32) {
        if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
            self.end_date = date;
        }
    }

    fn add_equity(&mut self, ticker: &str) -> Symbol {
        self.symbol_for_ticker(ticker)
    }

    fn add_option(&mut self, ticker: &str) -> Symbol {
        let market = Market::usa();
        Symbol::create_option(
            Symbol::create_equity(ticker, &market),
            &market,
            NaiveDate::from_ymd_opt(1970, 1, 16).unwrap(),
            rust_decimal::Decimal::ZERO,
            OptionRight::Call,
            OptionStyle::American,
        )
    }

    fn add_future(&mut self, ticker: &str) -> Symbol {
        Symbol::create_future(
            ticker,
            &Market::usa(),
            NaiveDate::from_ymd_opt(1970, 1, 16).unwrap(),
        )
    }

    fn symbol_for(&self, ticker: &str) -> Symbol {
        self.securities
            .get(&ticker.to_ascii_uppercase())
            .cloned()
            .unwrap_or_else(|| Symbol::create_equity(ticker, &Market::usa()))
    }

    fn history_count(
        &self,
        _symbol: &Symbol,
        _bar_count: usize,
        _resolution: Resolution,
    ) -> Vec<TradeBar> {
        Vec::new()
    }

    fn history_range(
        &self,
        _symbol: &Symbol,
        _resolution: Resolution,
        _start: chrono::NaiveDate,
        _end: chrono::NaiveDate,
    ) -> Vec<TradeBar> {
        Vec::new()
    }

    fn indicator(
        &self,
        _name: &str,
        _symbol: &Symbol,
        _period: usize,
        _bar_count: usize,
        _resolution: Resolution,
    ) -> IndicatorResult {
        IndicatorResult::default()
    }

    fn option_chain(&self, _ticker: &str) -> Vec<HashMap<String, String>> {
        Vec::new()
    }

    fn last_price(&self, _symbol: &Symbol) -> Option<f64> {
        None
    }

    fn start_date(&self) -> chrono::NaiveDate {
        self.start_date
    }

    fn end_date(&self) -> chrono::NaiveDate {
        self.end_date
    }

    fn security_keys(&self) -> Vec<String> {
        self.securities.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use rlean_core::{DateTime, Market, OptionRight, OptionStyle, TimeSpan};
    use rlean_data_tables::TradeBarData;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn bar(symbol: Symbol, day: i64, close: i64) -> TradeBar {
        TradeBar::new(
            symbol,
            DateTime::from_secs(day * 86_400),
            TimeSpan::from_days(1),
            TradeBarData::new(
                dec!(1),
                dec!(2),
                dec!(0.5),
                Decimal::from(close),
                dec!(1000),
            ),
        )
    }

    struct FakeBackend {
        symbol: Symbol,
        start: NaiveDate,
        end: NaiveDate,
    }

    impl FakeBackend {
        fn new() -> Self {
            Self {
                symbol: Symbol::create_equity("SPY", &Market::usa()),
                start: date(2024, 1, 1),
                end: date(2024, 1, 31),
            }
        }
    }

    impl ResearchBackend for FakeBackend {
        fn set_start_date(&mut self, year: i32, month: u32, day: u32) {
            self.start = date(year, month, day);
        }

        fn set_end_date(&mut self, year: i32, month: u32, day: u32) {
            self.end = date(year, month, day);
        }

        fn add_equity(&mut self, ticker: &str) -> Symbol {
            self.symbol = Symbol::create_equity(ticker, &Market::usa());
            self.symbol.clone()
        }

        fn add_option(&mut self, ticker: &str) -> Symbol {
            let underlying = Symbol::create_equity(ticker, &Market::usa());
            Symbol::create_option(
                underlying,
                &Market::usa(),
                date(2024, 1, 19),
                Decimal::from(100),
                OptionRight::Call,
                OptionStyle::American,
            )
        }

        fn add_future(&mut self, ticker: &str) -> Symbol {
            Symbol::create_future(ticker, &Market::usa(), date(2024, 3, 15))
        }

        fn symbol_for(&self, _ticker: &str) -> Symbol {
            self.symbol.clone()
        }

        fn history_count(
            &self,
            symbol: &Symbol,
            bar_count: usize,
            _resolution: Resolution,
        ) -> Vec<TradeBar> {
            (0..bar_count)
                .map(|i| bar(symbol.clone(), i as i64 + 1, i as i64 + 10))
                .collect()
        }

        fn history_range(
            &self,
            symbol: &Symbol,
            _resolution: Resolution,
            _start: NaiveDate,
            _end: NaiveDate,
        ) -> Vec<TradeBar> {
            vec![bar(symbol.clone(), 1, 10), bar(symbol.clone(), 2, 20)]
        }

        fn indicator(
            &self,
            _name: &str,
            _symbol: &Symbol,
            _period: usize,
            bar_count: usize,
            _resolution: Resolution,
        ) -> IndicatorResult {
            IndicatorResult {
                time: (0..bar_count).map(|i| format!("t{i}")).collect(),
                value: vec![1.0; bar_count],
                signal: vec![2.0; bar_count],
                histogram: vec![3.0; bar_count],
                upper: vec![4.0; bar_count],
                lower: vec![5.0; bar_count],
            }
        }

        fn option_chain(&self, ticker: &str) -> Vec<HashMap<String, String>> {
            vec![HashMap::from([(
                "underlying".to_string(),
                ticker.to_string(),
            )])]
        }

        fn last_price(&self, _symbol: &Symbol) -> Option<f64> {
            Some(123.45)
        }

        fn start_date(&self) -> NaiveDate {
            self.start
        }

        fn end_date(&self) -> NaiveDate {
            self.end
        }

        fn security_keys(&self) -> Vec<String> {
            vec![self.symbol.value.to_string()]
        }
    }

    #[test]
    fn indicator_frame_exposes_lean_extra_series_names() {
        let result = IndicatorResult {
            time: vec!["2024-01-01".to_string()],
            value: vec![1.0],
            signal: vec![2.0],
            histogram: vec![3.0],
            upper: vec![4.0],
            lower: vec![5.0],
        };

        let macd = IndicatorFrameView::new("MACD", result.clone());
        let extras: Vec<_> = macd
            .extra_series()
            .map(|(name, values)| (name, values[0]))
            .collect();
        assert_eq!(extras, vec![("signal", 2.0), ("histogram", 3.0)]);

        let bb = IndicatorFrameView::new("BollingerBands", result);
        let extras: Vec<_> = bb
            .extra_series()
            .map(|(name, values)| (name, values[0]))
            .collect();
        assert_eq!(extras, vec![("upper", 4.0), ("lower", 5.0)]);
    }

    #[test]
    fn research_book_forwards_quantbook_state_and_views() {
        let book = ResearchBook::new(Box::new(FakeBackend::new()));
        book.set_start_date(2023, 12, 1);
        book.set_end_date(2023, 12, 31);

        assert_eq!(book.start_date(), date(2023, 12, 1));
        assert_eq!(book.end_date(), date(2023, 12, 31));

        let symbol = book.add_equity("spy");
        assert_eq!(symbol.value.as_ref(), "SPY");
        assert_eq!(book.security_keys(), vec!["SPY".to_string()]);

        let history = book.history_for_symbol(
            ResearchSymbol::Ticker("SPY".to_string()),
            2,
            Resolution::Daily,
        );
        assert_eq!(history.close, vec![10.0, 11.0]);
        assert_eq!(history.columns()[3], ("close", history.close.as_slice()));

        let indicator = book.indicator_frame_for_symbol(
            "MACD",
            ResearchSymbol::Symbol(symbol.clone()),
            12,
            2,
            Resolution::Daily,
        );
        assert_eq!(indicator.value(), &[1.0, 1.0]);
        assert_eq!(
            book.get_last_price_for_symbol(ResearchSymbol::Symbol(symbol)),
            Some(123.45)
        );
        assert_eq!(
            book.option_chain_rows("SPY")[0].get("underlying"),
            Some(&"SPY".to_string())
        );
    }
}
