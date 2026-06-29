//! SDK market-data surface.
//!
//! Engine code owns the current `Slice` and swaps it each timestep. These SDK
//! types are handles into that engine-owned frame, so Python wrappers can keep
//! stable objects while accessors read current Rust data.

use crate::options::OptionChainView;
use lean_core::{Symbol, TickType};
use lean_data::{CustomDataPoint, QuoteBar, Slice};
use lean_options::OptionChain;
use lean_sdk_annotations::{sdk_bind, sdk_getter, sdk_method};
use rust_decimal::prelude::ToPrimitive;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

fn decimal_to_f64(value: rust_decimal::Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

fn json_value_to_field_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(value) => value.to_string(),
        serde_json::Value::Number(value) => value.to_string(),
        serde_json::Value::String(value) => value.clone(),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => value.to_string(),
    }
}

pub fn ns_to_exchange_naive(ns: i64) -> chrono::NaiveDateTime {
    use chrono::{DateTime as ChronoDateTime, Utc};
    use chrono_tz::US::Eastern;

    let secs = ns.div_euclid(1_000_000_000);
    let nsub = ns.rem_euclid(1_000_000_000) as u32;
    let dt: ChronoDateTime<Utc> = chrono::DateTime::from_timestamp(secs, nsub).unwrap_or_default();
    dt.with_timezone(&Eastern).naive_local()
}

pub fn symbol_aliases(symbol: &Symbol) -> [String; 2] {
    [symbol.value.to_string(), symbol.permtick.to_string()]
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SymbolAliasMap {
    ticker_to_sid: HashMap<String, u64>,
}

impl SymbolAliasMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(symbol_count: usize) -> Self {
        Self {
            ticker_to_sid: HashMap::with_capacity(symbol_count.saturating_mul(2)),
        }
    }

    pub fn insert_symbol(&mut self, sid: u64, symbol: &Symbol) {
        for alias in symbol_aliases(symbol) {
            self.ticker_to_sid.insert(alias, sid);
        }
    }

    pub fn remove_inactive(&mut self, active_sids: &std::collections::HashSet<u64>) {
        self.ticker_to_sid
            .retain(|_, sid| active_sids.contains(sid));
    }

    pub fn resolve_ticker(&self, ticker: &str) -> Option<u64> {
        self.ticker_to_sid.get(ticker).copied()
    }

    pub fn contains_ticker(&self, ticker: &str) -> bool {
        self.ticker_to_sid.contains_key(ticker)
    }

    pub fn get(&self, ticker: &str) -> Option<&u64> {
        self.ticker_to_sid.get(ticker)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.ticker_to_sid.keys()
    }

    pub fn reserve(&mut self, additional: usize) {
        self.ticker_to_sid.reserve(additional);
    }
}

/// Shared engine-owned current slice. `algorithm_manager` is responsible for
/// calling `set_current` before dispatching callbacks.
#[derive(Debug, Clone, Default)]
pub struct SharedSliceFrame {
    current: Arc<RwLock<Option<Arc<Slice>>>>,
}

impl SharedSliceFrame {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_current(&self, slice: Arc<Slice>) {
        *self.current.write().expect("slice frame poisoned") = Some(slice);
    }

    pub fn clear(&self) {
        *self.current.write().expect("slice frame poisoned") = None;
    }

    pub fn current(&self) -> Option<Arc<Slice>> {
        self.current.read().expect("slice frame poisoned").clone()
    }

    pub fn option_chain(&self, key: &str) -> Option<Arc<OptionChain>> {
        self.current()
            .and_then(|slice| slice.option_chains.get(key).cloned())
    }

    pub fn option_chain_count(&self) -> usize {
        self.current()
            .map(|slice| slice.option_chains.len())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone)]
#[sdk_bind(py_name = "Slice")]
pub struct SliceView {
    frame: SharedSliceFrame,
}

impl SliceView {
    pub fn new(frame: SharedSliceFrame) -> Self {
        Self { frame }
    }

    pub fn frame(&self) -> &SharedSliceFrame {
        &self.frame
    }

    #[sdk_getter]
    pub fn has_data(&self) -> bool {
        self.frame
            .current()
            .map(|slice| slice.has_data || !slice.custom_data.is_empty())
            .unwrap_or(false)
    }

    #[sdk_getter]
    pub fn bars(&self) -> TradeBarsView {
        TradeBarsView::new(self.frame.clone())
    }

    #[sdk_getter]
    pub fn quote_bars(&self) -> QuoteBarsView {
        QuoteBarsView::new(self.frame.clone())
    }

    #[sdk_getter]
    pub fn ticks(&self) -> TicksView {
        TicksView::new(self.frame.clone())
    }

    #[sdk_getter]
    pub fn custom(&self) -> CustomDataView {
        CustomDataView::new(self.frame.clone())
    }

    #[sdk_getter]
    pub fn option_chains(&self) -> OptionChainsView {
        OptionChainsView::new(self.frame.clone())
    }
}

#[derive(Debug, Clone, Default)]
#[sdk_bind(py_name = "OptionChains")]
pub struct OptionChainsView {
    frame: SharedSliceFrame,
}

impl OptionChainsView {
    pub fn new(frame: SharedSliceFrame) -> Self {
        Self { frame }
    }

    #[sdk_method]
    pub fn get(&self, canonical: &Symbol) -> Option<OptionChainView> {
        self.frame
            .option_chain(&canonical.permtick)
            .map(|chain| OptionChainView::from_chain(&chain))
    }

    #[sdk_method]
    pub fn __contains__(&self, canonical: &Symbol) -> bool {
        self.frame.option_chain(&canonical.permtick).is_some()
    }

    #[sdk_method]
    pub fn __getitem__(&self, canonical: &Symbol) -> Option<OptionChainView> {
        self.get(canonical)
    }

    #[sdk_getter]
    pub fn count(&self) -> usize {
        self.frame.option_chain_count()
    }
}

#[derive(Debug, Clone, Default)]
#[sdk_bind(py_name = "TradeBars")]
pub struct TradeBarsView {
    frame: SharedSliceFrame,
}

impl TradeBarsView {
    pub fn new(frame: SharedSliceFrame) -> Self {
        Self { frame }
    }

    #[sdk_method]
    pub fn get(&self, symbol: &Symbol) -> Option<TradeBarView> {
        let slice = self.frame.current()?;
        slice
            .bars
            .contains_key(&symbol.id.sid)
            .then(|| TradeBarView::new(slice, symbol.id.sid))
    }

    #[sdk_method]
    pub fn __contains__(&self, symbol: &Symbol) -> bool {
        self.frame
            .current()
            .map(|slice| slice.bars.contains_key(&symbol.id.sid))
            .unwrap_or(false)
    }

    #[sdk_method]
    pub fn __getitem__(&self, symbol: &Symbol) -> Option<TradeBarView> {
        self.get(symbol)
    }

    #[sdk_getter]
    pub fn count(&self) -> usize {
        self.frame
            .current()
            .map(|slice| slice.bars.len())
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
#[sdk_bind(py_name = "TradeBar")]
pub struct TradeBarView {
    slice: Arc<Slice>,
    sid: u64,
}

impl TradeBarView {
    pub fn new(slice: Arc<Slice>, sid: u64) -> Self {
        Self { slice, sid }
    }

    fn bar(&self) -> &lean_data::TradeBar {
        self.slice
            .bars
            .get(&self.sid)
            .expect("TradeBarView sid missing from slice")
    }

    #[sdk_getter]
    pub fn symbol(&self) -> &Symbol {
        &self.bar().symbol
    }
    #[sdk_getter(alias = "Open")]
    pub fn open(&self) -> f64 {
        decimal_to_f64(self.bar().open)
    }
    #[sdk_getter(alias = "High")]
    pub fn high(&self) -> f64 {
        decimal_to_f64(self.bar().high)
    }
    #[sdk_getter(alias = "Low")]
    pub fn low(&self) -> f64 {
        decimal_to_f64(self.bar().low)
    }
    #[sdk_getter(alias = "Close")]
    pub fn close(&self) -> f64 {
        decimal_to_f64(self.bar().close)
    }
    #[sdk_getter]
    pub fn volume(&self) -> f64 {
        decimal_to_f64(self.bar().volume)
    }
    #[sdk_getter]
    pub fn value(&self) -> f64 {
        self.close()
    }
    #[sdk_getter(alias = "Time")]
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.bar().time.0)
    }
    #[sdk_getter(alias = "EndTime")]
    pub fn end_time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.bar().end_time.0)
    }
}

#[derive(Debug, Clone)]
#[sdk_bind(py_name = "Bar")]
pub struct BarView {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

impl BarView {
    #[sdk_getter(alias = "Open")]
    pub fn open(&self) -> f64 {
        self.open
    }
    #[sdk_getter(alias = "High")]
    pub fn high(&self) -> f64 {
        self.high
    }
    #[sdk_getter(alias = "Low")]
    pub fn low(&self) -> f64 {
        self.low
    }
    #[sdk_getter(alias = "Close")]
    pub fn close(&self) -> f64 {
        self.close
    }
}

#[derive(Debug, Clone, Default)]
#[sdk_bind(py_name = "QuoteBars")]
pub struct QuoteBarsView {
    frame: SharedSliceFrame,
}

impl QuoteBarsView {
    pub fn new(frame: SharedSliceFrame) -> Self {
        Self { frame }
    }

    #[sdk_method]
    pub fn get(&self, symbol: &Symbol) -> Option<QuoteBarView> {
        let slice = self.frame.current()?;
        slice
            .quote_bars
            .contains_key(&symbol.id.sid)
            .then(|| QuoteBarView::new(slice, symbol.id.sid))
    }

    #[sdk_method]
    pub fn __contains__(&self, symbol: &Symbol) -> bool {
        self.frame
            .current()
            .map(|slice| slice.quote_bars.contains_key(&symbol.id.sid))
            .unwrap_or(false)
    }

    #[sdk_method]
    pub fn __getitem__(&self, symbol: &Symbol) -> Option<QuoteBarView> {
        self.get(symbol)
    }

    #[sdk_getter]
    pub fn count(&self) -> usize {
        self.frame
            .current()
            .map(|slice| slice.quote_bars.len())
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
#[sdk_bind(py_name = "QuoteBar")]
pub struct QuoteBarView {
    slice: Arc<Slice>,
    sid: u64,
}

impl QuoteBarView {
    pub fn new(slice: Arc<Slice>, sid: u64) -> Self {
        Self { slice, sid }
    }

    fn bar(&self) -> &QuoteBar {
        self.slice
            .quote_bars
            .get(&self.sid)
            .expect("QuoteBarView sid missing from slice")
    }

    fn bid_bar(&self) -> Option<BarView> {
        self.bar().bid.as_ref().map(|b| BarView {
            open: decimal_to_f64(b.open),
            high: decimal_to_f64(b.high),
            low: decimal_to_f64(b.low),
            close: decimal_to_f64(b.close),
        })
    }

    fn ask_bar(&self) -> Option<BarView> {
        self.bar().ask.as_ref().map(|b| BarView {
            open: decimal_to_f64(b.open),
            high: decimal_to_f64(b.high),
            low: decimal_to_f64(b.low),
            close: decimal_to_f64(b.close),
        })
    }

    #[sdk_getter]
    pub fn symbol(&self) -> &Symbol {
        &self.bar().symbol
    }
    #[sdk_getter]
    pub fn bid(&self) -> Option<BarView> {
        self.bid_bar()
    }
    #[sdk_getter]
    pub fn ask(&self) -> Option<BarView> {
        self.ask_bar()
    }
    #[sdk_getter]
    pub fn open(&self) -> f64 {
        decimal_to_f64(self.bar().mid_open())
    }
    #[sdk_getter(alias = "Close")]
    pub fn close(&self) -> f64 {
        decimal_to_f64(self.bar().mid_close())
    }
    #[sdk_getter]
    pub fn bid_size(&self) -> f64 {
        decimal_to_f64(self.bar().last_bid_size)
    }
    #[sdk_getter]
    pub fn ask_size(&self) -> f64 {
        decimal_to_f64(self.bar().last_ask_size)
    }
    #[sdk_getter]
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.bar().time.0)
    }
    #[sdk_getter]
    pub fn end_time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.bar().end_time.0)
    }
}

#[derive(Debug, Clone, Default)]
#[sdk_bind(py_name = "Ticks")]
pub struct TicksView {
    frame: SharedSliceFrame,
}

impl TicksView {
    pub fn new(frame: SharedSliceFrame) -> Self {
        Self { frame }
    }

    #[sdk_method]
    pub fn get(&self, symbol: &Symbol) -> Vec<TickView> {
        let Some(slice) = self.frame.current() else {
            return Vec::new();
        };
        let sid = symbol.id.sid;
        let len = slice.ticks.get(&sid).map(Vec::len).unwrap_or(0);
        (0..len)
            .map(|index| TickView::new(slice.clone(), sid, index))
            .collect()
    }

    #[sdk_getter]
    pub fn count(&self) -> usize {
        self.frame
            .current()
            .map(|slice| slice.ticks.len())
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
#[sdk_bind(py_name = "Tick")]
pub struct TickView {
    slice: Arc<Slice>,
    sid: u64,
    index: usize,
}

impl TickView {
    pub fn new(slice: Arc<Slice>, sid: u64, index: usize) -> Self {
        Self { slice, sid, index }
    }

    fn tick(&self) -> &lean_data::Tick {
        self.slice
            .ticks
            .get(&self.sid)
            .and_then(|ticks| ticks.get(self.index))
            .expect("TickView index missing from slice")
    }

    #[sdk_getter]
    pub fn symbol(&self) -> &Symbol {
        &self.tick().symbol
    }
    #[sdk_getter]
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.tick().time.0)
    }
    #[sdk_getter]
    pub fn value(&self) -> f64 {
        decimal_to_f64(self.tick().value)
    }
    #[sdk_getter]
    pub fn quantity(&self) -> f64 {
        decimal_to_f64(self.tick().quantity)
    }
    #[sdk_getter]
    pub fn bid_price(&self) -> f64 {
        decimal_to_f64(self.tick().bid_price)
    }
    #[sdk_getter]
    pub fn ask_price(&self) -> f64 {
        decimal_to_f64(self.tick().ask_price)
    }
    #[sdk_getter]
    pub fn tick_type(&self) -> String {
        match self.tick().tick_type {
            TickType::Trade => "Trade",
            TickType::Quote => "Quote",
            TickType::OpenInterest => "OpenInterest",
        }
        .to_string()
    }
    #[sdk_method]
    pub fn is_trade(&self) -> bool {
        self.tick().tick_type == TickType::Trade
    }
    #[sdk_method]
    pub fn is_quote(&self) -> bool {
        self.tick().tick_type == TickType::Quote
    }
}

#[derive(Debug, Clone, Default)]
#[sdk_bind(py_name = "CustomData")]
pub struct CustomDataView {
    frame: SharedSliceFrame,
}

impl CustomDataView {
    pub fn new(frame: SharedSliceFrame) -> Self {
        Self { frame }
    }

    fn normalize_key(key: &str) -> String {
        key.to_ascii_uppercase()
    }

    fn latest_point(&self, key: &str) -> Option<CustomDataPoint> {
        let slice = self.frame.current()?;
        slice
            .custom_data
            .get(&Self::normalize_key(key))
            .and_then(|points| points.last())
            .cloned()
    }

    #[sdk_method]
    pub fn get(&self, key: &str) -> Option<CustomDataPointView> {
        self.latest_point(key).map(CustomDataPointView::new)
    }

    #[sdk_method]
    pub fn get_all(&self, key: &str) -> Vec<CustomDataPointView> {
        self.frame
            .current()
            .and_then(|slice| slice.custom_data.get(&Self::normalize_key(key)).cloned())
            .unwrap_or_default()
            .into_iter()
            .map(CustomDataPointView::new)
            .collect()
    }

    #[sdk_method]
    pub fn __getitem__(&self, key: &str) -> Option<CustomDataPointView> {
        self.get(key)
    }

    #[sdk_method]
    pub fn __contains__(&self, key: &str) -> bool {
        self.frame
            .current()
            .map(|slice| slice.custom_data.contains_key(&Self::normalize_key(key)))
            .unwrap_or(false)
    }

    #[sdk_getter]
    pub fn count(&self) -> usize {
        self.frame
            .current()
            .map(|slice| slice.custom_data.len())
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone)]
#[sdk_bind(py_name = "CustomDataPoint")]
pub struct CustomDataPointView {
    point: CustomDataPoint,
}

impl CustomDataPointView {
    pub fn new(point: CustomDataPoint) -> Self {
        Self { point }
    }

    #[sdk_getter]
    pub fn value(&self) -> f64 {
        decimal_to_f64(self.point.value)
    }

    #[sdk_getter(py_name = "Value")]
    pub fn value_pascal(&self) -> f64 {
        self.value()
    }

    #[sdk_getter]
    pub fn time(&self) -> chrono::NaiveDate {
        self.point.time
    }

    #[sdk_getter(py_name = "Time")]
    pub fn time_pascal(&self) -> chrono::NaiveDate {
        self.time()
    }

    #[sdk_getter]
    pub fn end_time(&self) -> chrono::NaiveDateTime {
        self.point
            .end_time
            .map(|time| ns_to_exchange_naive(time.0))
            .unwrap_or_else(|| self.point.time.and_hms_opt(0, 0, 0).unwrap_or_default())
    }

    #[sdk_getter(py_name = "EndTime")]
    pub fn end_time_pascal(&self) -> chrono::NaiveDateTime {
        self.end_time()
    }

    #[sdk_getter]
    pub fn fields(&self) -> HashMap<String, String> {
        self.point
            .fields
            .iter()
            .map(|(key, value)| (key.clone(), json_value_to_field_string(value)))
            .collect()
    }

    #[sdk_getter(py_name = "Fields")]
    pub fn fields_pascal(&self) -> HashMap<String, String> {
        self.fields()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lean_core::{Market, SymbolOptionsExt, TimeSpan};
    use lean_data::{TradeBar, TradeBarData};
    use lean_options::OptionChain;
    use rust_decimal_macros::dec;
    use std::collections::HashSet;

    #[test]
    fn slice_view_empty_frame_matches_lean_empty_slice_shape() {
        let frame = SharedSliceFrame::new();
        let view = SliceView::new(frame.clone());
        let symbol = Symbol::create_equity("SPY", &Market::usa());

        assert!(!view.has_data());
        assert_eq!(view.bars().count(), 0);
        assert_eq!(view.quote_bars().count(), 0);
        assert_eq!(view.ticks().count(), 0);
        assert!(view.bars().get(&symbol).is_none());
        assert!(view.quote_bars().get(&symbol).is_none());
        assert!(view.ticks().get(&symbol).is_empty());

        frame.clear();
        assert!(frame.current().is_none());
    }

    #[test]
    fn bar_collections_support_membership_and_indexing() {
        let frame = SharedSliceFrame::new();
        let view = SliceView::new(frame.clone());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut slice = Slice::new(lean_core::DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(16, 0, 0)
                .unwrap(),
        ));
        slice.add_bar(lean_data::TradeBar::new(
            symbol.clone(),
            slice.time,
            lean_core::TimeSpan::ONE_DAY,
            lean_data::TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(1), dec!(100)),
        ));
        frame.set_current(Arc::new(slice));

        assert!(view.bars().__contains__(&symbol));
        assert!(view.bars().__getitem__(&symbol).is_some());
        assert!(!view.quote_bars().__contains__(&symbol));
        assert!(view.quote_bars().__getitem__(&symbol).is_none());
    }

    #[test]
    fn slice_view_exposes_custom_data_latest_and_membership() {
        let frame = SharedSliceFrame::new();
        let view = SliceView::new(frame.clone());
        let day = chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap();
        let mut slice = Slice::new(lean_core::DateTime::from(
            day.and_hms_opt(16, 0, 0).unwrap(),
        ));
        slice.custom_data.insert(
            "VIX".to_string(),
            vec![
                CustomDataPoint {
                    time: day,
                    end_time: None,
                    value: dec!(13.5),
                    fields: HashMap::new(),
                },
                CustomDataPoint {
                    time: day,
                    end_time: None,
                    value: dec!(14.25),
                    fields: HashMap::from([
                        ("usymbol".to_string(), serde_json::json!("SPY")),
                        ("indicative_borrow".to_string(), serde_json::json!(1.25)),
                        ("missing".to_string(), serde_json::Value::Null),
                    ]),
                },
            ],
        );
        frame.set_current(Arc::new(slice));

        let custom = view.custom();
        assert!(custom.__contains__("vix"));
        assert_eq!(custom.count(), 1);
        assert_eq!(custom.get("VIX").unwrap().value(), 14.25);
        assert_eq!(custom.get("VIX").unwrap().value_pascal(), 14.25);
        assert_eq!(custom.get("VIX").unwrap().fields()["usymbol"], "SPY");
        assert_eq!(
            custom.get("VIX").unwrap().fields_pascal()["indicative_borrow"],
            "1.25"
        );
        assert_eq!(custom.get("VIX").unwrap().fields()["missing"], "");
        assert_eq!(custom.__getitem__("VIX").unwrap().value(), 14.25);
        assert_eq!(custom.get_all("VIX").len(), 2);
    }

    #[test]
    fn slice_view_exposes_option_chain_lookup() {
        let frame = SharedSliceFrame::new();
        let view = SliceView::new(frame.clone());
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let canonical = Symbol::create_canonical_option(&underlying, &Market::usa());
        let mut slice = Slice::new(lean_core::DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(16, 0, 0)
                .unwrap(),
        ));
        let chain = OptionChain::new(canonical.clone(), dec!(411));

        slice.add_option_chain(canonical.permtick.to_string(), Arc::new(chain));
        frame.set_current(Arc::new(slice));

        let chains = view.option_chains();
        assert_eq!(chains.count(), 1);
        assert!(chains.__contains__(&canonical));
        assert!(chains.get(&canonical).is_some());
        assert_eq!(
            chains.__getitem__(&canonical).unwrap().underlying().price(),
            411.0
        );

        frame.clear();
        assert_eq!(view.option_chains().count(), 0);
    }

    #[test]
    fn trade_bar_view_keeps_original_slice_after_frame_advances() {
        let frame = SharedSliceFrame::new();
        let view = SliceView::new(frame.clone());
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut first_slice = Slice::new(lean_core::DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(9, 31, 0)
                .unwrap(),
        ));
        first_slice.add_bar(TradeBar::new(
            symbol.clone(),
            first_slice.time,
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(1), dec!(1), dec!(1), dec!(101), dec!(100)),
        ));
        frame.set_current(Arc::new(first_slice));
        let stored_bar = view.bars().__getitem__(&symbol).unwrap();

        let mut second_slice = Slice::new(lean_core::DateTime::from(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(9, 32, 0)
                .unwrap(),
        ));
        second_slice.add_bar(TradeBar::new(
            symbol.clone(),
            second_slice.time,
            TimeSpan::ONE_MINUTE,
            TradeBarData::new(dec!(2), dec!(2), dec!(2), dec!(202), dec!(100)),
        ));
        frame.set_current(Arc::new(second_slice));

        assert_eq!(stored_bar.close(), 101.0);
        assert_eq!(view.bars().__getitem__(&symbol).unwrap().close(), 202.0);
    }

    #[test]
    fn symbol_alias_map_registers_value_and_permtick_then_prunes_inactive() {
        let symbol = Symbol::create_equity("spy", &Market::usa());
        let sid = symbol.id.sid;
        let mut aliases = SymbolAliasMap::new();

        aliases.insert_symbol(sid, &symbol);

        assert_eq!(aliases.resolve_ticker("SPY"), Some(sid));
        assert_eq!(aliases.resolve_ticker(symbol.permtick.as_ref()), Some(sid));
        assert!(aliases.contains_ticker("SPY"));

        aliases.remove_inactive(&HashSet::new());
        assert_eq!(aliases.resolve_ticker("SPY"), None);
    }
}
