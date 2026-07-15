//! SDK-owned data view APIs for language bindings.
//!
//! Primitive projections (decimal->f64, `Value == Close` semantics) live here
//! so language bindings are pure structural glue a generator can emit 1:1.

use rlean_core::{Symbol, TickType};
use rlean_data::{Delisting, DelistingType, Slice, SymbolChangedEvent};
use rlean_data_tables::{Bar, CustomDataPoint, MarginInterestRate, QuoteBar, Tick, TradeBar};
use rust_decimal::prelude::ToPrimitive;
use std::collections::HashMap;

fn decimal_to_f64(value: rust_decimal::Decimal) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

#[derive(Debug, Clone)]
pub struct BarView {
    bar: Bar,
}

impl BarView {
    pub fn new(bar: Bar) -> Self {
        Self { bar }
    }

    pub fn open(&self) -> f64 {
        decimal_to_f64(self.bar.open)
    }

    pub fn high(&self) -> f64 {
        decimal_to_f64(self.bar.high)
    }

    pub fn low(&self) -> f64 {
        decimal_to_f64(self.bar.low)
    }

    pub fn close(&self) -> f64 {
        decimal_to_f64(self.bar.close)
    }
}

/// Convert a UTC nanosecond timestamp to an exchange-local (US/Eastern) naive
/// datetime, matching LEAN's convention of delivering bar times in exchange time.
///
/// This is real projection logic (timezone math), so it lives in the SDK rather
/// than in any individual language binding. Bindings call this and copy the
/// result into their native datetime type with zero additional logic.
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

/// Primitive projection of a `TradeBar` for language bindings.
#[derive(Debug, Clone)]
pub struct TradeBarView {
    bar: TradeBar,
}

impl TradeBarView {
    pub fn new(bar: TradeBar) -> Self {
        Self { bar }
    }

    pub fn symbol(&self) -> &Symbol {
        &self.bar.symbol
    }
    pub fn venue(&self) -> Option<&str> {
        self.bar.venue.as_deref()
    }
    pub fn open(&self) -> f64 {
        decimal_to_f64(self.bar.open)
    }
    pub fn high(&self) -> f64 {
        decimal_to_f64(self.bar.high)
    }
    pub fn low(&self) -> f64 {
        decimal_to_f64(self.bar.low)
    }
    pub fn close(&self) -> f64 {
        decimal_to_f64(self.bar.close)
    }
    pub fn volume(&self) -> f64 {
        decimal_to_f64(self.bar.volume)
    }
    /// `Value` matches C# `BaseData.Value`, which returns `Close` for a TradeBar.
    pub fn value(&self) -> f64 {
        self.close()
    }
    pub fn time_ns(&self) -> i64 {
        self.bar.time.0
    }
    pub fn end_time_ns(&self) -> i64 {
        self.bar.end_time.0
    }
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.time_ns())
    }
    pub fn end_time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.end_time_ns())
    }
}

/// Primitive projection of a `QuoteBar` for language bindings.
#[derive(Debug, Clone)]
pub struct QuoteBarView {
    bar: QuoteBar,
}

impl QuoteBarView {
    pub fn new(bar: QuoteBar) -> Self {
        Self { bar }
    }

    pub fn symbol(&self) -> &Symbol {
        &self.bar.symbol
    }
    pub fn venue(&self) -> Option<&str> {
        self.bar.venue.as_deref()
    }
    pub fn bid(&self) -> Option<BarView> {
        self.bar.bid.clone().map(BarView::new)
    }
    pub fn ask(&self) -> Option<BarView> {
        self.bar.ask.clone().map(BarView::new)
    }
    pub fn bid_open(&self) -> f64 {
        self.bar
            .bid
            .as_ref()
            .map(|b| decimal_to_f64(b.open))
            .unwrap_or(0.0)
    }
    pub fn bid_high(&self) -> f64 {
        self.bar
            .bid
            .as_ref()
            .map(|b| decimal_to_f64(b.high))
            .unwrap_or(0.0)
    }
    pub fn bid_low(&self) -> f64 {
        self.bar
            .bid
            .as_ref()
            .map(|b| decimal_to_f64(b.low))
            .unwrap_or(0.0)
    }
    pub fn bid_close(&self) -> Option<f64> {
        self.bar.bid.as_ref().map(|b| decimal_to_f64(b.close))
    }
    pub fn bid_close_or_zero(&self) -> f64 {
        self.bid_close().unwrap_or(0.0)
    }
    pub fn ask_open(&self) -> f64 {
        self.bar
            .ask
            .as_ref()
            .map(|a| decimal_to_f64(a.open))
            .unwrap_or(0.0)
    }
    pub fn ask_high(&self) -> f64 {
        self.bar
            .ask
            .as_ref()
            .map(|a| decimal_to_f64(a.high))
            .unwrap_or(0.0)
    }
    pub fn ask_low(&self) -> f64 {
        self.bar
            .ask
            .as_ref()
            .map(|a| decimal_to_f64(a.low))
            .unwrap_or(0.0)
    }
    pub fn ask_close(&self) -> Option<f64> {
        self.bar.ask.as_ref().map(|a| decimal_to_f64(a.close))
    }
    pub fn ask_close_or_zero(&self) -> f64 {
        self.ask_close().unwrap_or(0.0)
    }
    pub fn bid_size(&self) -> f64 {
        decimal_to_f64(self.bar.last_bid_size)
    }
    pub fn ask_size(&self) -> f64 {
        decimal_to_f64(self.bar.last_ask_size)
    }
    pub fn open(&self) -> f64 {
        decimal_to_f64(self.bar.mid_open())
    }
    pub fn close(&self) -> f64 {
        decimal_to_f64(self.bar.mid_close())
    }
    pub fn time_ns(&self) -> i64 {
        self.bar.time.0
    }
    pub fn end_time_ns(&self) -> i64 {
        self.bar.end_time.0
    }
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.time_ns())
    }
    pub fn end_time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.end_time_ns())
    }
}

#[derive(Debug, Clone)]
pub struct MarginInterestRateView {
    rate: MarginInterestRate,
}

impl MarginInterestRateView {
    pub fn new(rate: MarginInterestRate) -> Self {
        Self { rate }
    }

    pub fn symbol(&self) -> &Symbol {
        &self.rate.symbol
    }
    pub fn venue(&self) -> Option<&str> {
        self.rate.venue.as_deref()
    }
    pub fn time_ns(&self) -> i64 {
        self.rate.time.0
    }
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.time_ns())
    }
    pub fn interest_rate(&self) -> f64 {
        decimal_to_f64(self.rate.interest_rate)
    }
    pub fn value(&self) -> f64 {
        self.interest_rate()
    }
}

#[derive(Debug, Clone)]
pub struct TickView {
    tick: Tick,
}

impl TickView {
    pub fn new(tick: Tick) -> Self {
        Self { tick }
    }

    pub fn symbol(&self) -> &Symbol {
        &self.tick.symbol
    }
    pub fn venue(&self) -> Option<&str> {
        self.tick.venue.as_deref()
    }
    pub fn time_ns(&self) -> i64 {
        self.tick.time.0
    }
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.time_ns())
    }
    pub fn value(&self) -> f64 {
        decimal_to_f64(self.tick.value)
    }
    pub fn quantity(&self) -> f64 {
        decimal_to_f64(self.tick.quantity)
    }
    pub fn bid_price(&self) -> f64 {
        decimal_to_f64(self.tick.bid_price)
    }
    pub fn ask_price(&self) -> f64 {
        decimal_to_f64(self.tick.ask_price)
    }
    pub fn bid_size(&self) -> f64 {
        decimal_to_f64(self.tick.bid_size)
    }
    pub fn ask_size(&self) -> f64 {
        decimal_to_f64(self.tick.ask_size)
    }
    pub fn exchange(&self) -> Option<&str> {
        self.tick.exchange.as_deref()
    }
    pub fn sale_condition(&self) -> Option<&str> {
        self.tick.sale_condition.as_deref()
    }
    pub fn suspicious(&self) -> bool {
        self.tick.suspicious
    }
    pub fn tick_type(&self) -> String {
        self.tick_type_name().to_string()
    }
    pub fn tick_type_name(&self) -> &'static str {
        match self.tick.tick_type {
            TickType::Trade => "Trade",
            TickType::Quote => "Quote",
            TickType::OpenInterest => "OpenInterest",
        }
    }
    pub fn is_trade(&self) -> bool {
        self.tick.tick_type == TickType::Trade
    }
    pub fn is_quote(&self) -> bool {
        self.tick.tick_type == TickType::Quote
    }
}

#[derive(Debug, Clone)]
pub struct DelistingView {
    delisting: Delisting,
}

impl DelistingView {
    pub fn new(delisting: Delisting) -> Self {
        Self { delisting }
    }

    pub fn symbol(&self) -> &Symbol {
        &self.delisting.symbol
    }
    pub fn time_ns(&self) -> i64 {
        self.delisting.time.0
    }
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.time_ns())
    }
    pub fn price(&self) -> f64 {
        decimal_to_f64(self.delisting.price)
    }
    pub fn delisting_type(&self) -> DelistingType {
        self.delisting.delisting_type
    }
    pub fn delisting_type_name(&self) -> &'static str {
        match self.delisting.delisting_type {
            DelistingType::Warning => "Warning",
            DelistingType::Delisted => "Delisted",
        }
    }
    pub fn is_warning(&self) -> bool {
        self.delisting.delisting_type == DelistingType::Warning
    }
}

#[derive(Debug, Clone)]
pub struct SymbolChangedEventView {
    event: SymbolChangedEvent,
}

impl SymbolChangedEventView {
    pub fn new(event: SymbolChangedEvent) -> Self {
        Self { event }
    }

    pub fn symbol(&self) -> &Symbol {
        &self.event.symbol
    }
    pub fn time_ns(&self) -> i64 {
        self.event.time.0
    }
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.time_ns())
    }
    pub fn old_symbol(&self) -> &str {
        &self.event.old_symbol
    }
    pub fn new_symbol(&self) -> &str {
        &self.event.new_symbol
    }
}

#[derive(Debug, Clone)]
pub struct CustomDataPointView {
    point: CustomDataPoint,
}

impl CustomDataPointView {
    pub fn new(point: CustomDataPoint) -> Self {
        Self { point }
    }

    pub fn value(&self) -> f64 {
        decimal_to_f64(self.point.value)
    }
    pub fn venue(&self) -> Option<&str> {
        self.point.venue.as_deref()
    }
    /// Period start (LEAN `BaseData.Time`) in exchange-local time.
    pub fn time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.point.time.0)
    }
    pub fn end_time_ns(&self) -> i64 {
        self.point.end_time.0
    }
    /// Emission/end time (LEAN `BaseData.EndTime`) in exchange-local time.
    pub fn end_time(&self) -> chrono::NaiveDateTime {
        ns_to_exchange_naive(self.end_time_ns())
    }
    pub fn fields(&self) -> &HashMap<String, serde_json::Value> {
        &self.point.fields
    }
}

#[derive(Debug, Clone, Default)]
pub struct TradeBarsView {
    bars: HashMap<u64, TradeBar>,
}

impl TradeBarsView {
    pub fn new(bars: HashMap<u64, TradeBar>) -> Self {
        Self { bars }
    }

    pub fn get(&self, symbol: &Symbol) -> Option<&TradeBar> {
        self.bars.get(&symbol.id.sid)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&u64, &TradeBar)> {
        self.bars.iter()
    }

    pub fn count(&self) -> usize {
        self.bars.len()
    }
}

#[derive(Debug, Clone, Default)]
pub struct QuoteBarsView {
    bars: HashMap<u64, QuoteBar>,
}

impl QuoteBarsView {
    pub fn new(bars: HashMap<u64, QuoteBar>) -> Self {
        Self { bars }
    }

    pub fn get(&self, symbol: &Symbol) -> Option<&QuoteBar> {
        self.bars.get(&symbol.id.sid)
    }

    pub fn count(&self) -> usize {
        self.bars.len()
    }
}

#[derive(Debug, Clone, Default)]
pub struct TicksView {
    ticks: HashMap<u64, Vec<Tick>>,
}

impl TicksView {
    pub fn new(ticks: HashMap<u64, Vec<Tick>>) -> Self {
        Self { ticks }
    }

    pub fn get(&self, symbol: &Symbol) -> Option<&[Tick]> {
        self.ticks.get(&symbol.id.sid).map(Vec::as_slice)
    }

    pub fn count(&self) -> usize {
        self.ticks.values().map(Vec::len).sum()
    }
}

#[derive(Debug, Clone, Default)]
pub struct CustomDataView {
    points: HashMap<String, Vec<CustomDataPoint>>,
}

impl CustomDataView {
    pub fn new(points: HashMap<String, Vec<CustomDataPoint>>) -> Self {
        let mut normalized = HashMap::with_capacity(points.len());
        for (ticker, rows) in points {
            if !rows.is_empty() {
                normalized.insert(Self::normalize_key(&ticker), rows);
            }
        }
        Self { points: normalized }
    }

    pub fn get(&self, key: &str) -> Option<&[CustomDataPoint]> {
        self.points
            .get(&Self::normalize_key(key))
            .map(Vec::as_slice)
    }

    pub fn latest(&self, key: &str) -> Option<&CustomDataPoint> {
        self.get(key).and_then(|points| points.last())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.points.contains_key(&Self::normalize_key(key))
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Vec<CustomDataPoint>)> {
        self.points.iter()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.points.keys()
    }

    pub fn normalize_key(key: &str) -> String {
        key.to_uppercase()
    }
}

#[derive(Debug, Clone)]
pub struct SliceView {
    slice: Slice,
}

impl SliceView {
    pub fn new(slice: Slice) -> Self {
        Self { slice }
    }

    pub fn inner(&self) -> &Slice {
        &self.slice
    }

    pub fn has_data(&self) -> bool {
        self.slice.has_data || !self.slice.custom_data.is_empty()
    }

    pub fn bars(&self) -> TradeBarsView {
        TradeBarsView::new(self.slice.bars.clone())
    }

    pub fn quote_bars(&self) -> QuoteBarsView {
        QuoteBarsView::new(self.slice.quote_bars.clone())
    }

    pub fn ticks(&self) -> TicksView {
        TicksView::new(self.slice.ticks.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::Market;
    use rlean_data_tables::{Bar, TradeBarData};
    use rlean_data_tables::{CustomDataPoint, QuoteBar, TradeBar};
    use rust_decimal_macros::dec;
    use std::sync::Arc;

    #[test]
    fn tradebar_view_value_equals_close() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let bar = TradeBar::new(
            sym,
            rlean_core::DateTime::EPOCH,
            rlean_core::TimeSpan::from_secs(60),
            TradeBarData::new(dec!(100), dec!(105), dec!(99), dec!(102), dec!(1000)),
        );
        let view = TradeBarView::new(bar);
        assert!((view.value() - view.close()).abs() < 1e-9);
        assert!((view.close() - 102.0).abs() < 1e-9);
        assert!((view.high() - 105.0).abs() < 1e-9);
        assert!((view.low() - 99.0).abs() < 1e-9);
        assert!((view.volume() - 1000.0).abs() < 1e-9);
    }

    #[test]
    fn quotebar_view_uses_quotebar_mid_prices() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let bar = QuoteBar::new(
            sym,
            rlean_core::DateTime::EPOCH,
            rlean_core::TimeSpan::from_secs(60),
            Some(Bar::new(dec!(100), dec!(101), dec!(99), dec!(100.5))),
            Some(Bar::new(dec!(101), dec!(102), dec!(100), dec!(101.5))),
            dec!(10),
            dec!(11),
        );

        let view = QuoteBarView::new(bar);

        assert_eq!(view.bid_open(), 100.0);
        assert_eq!(view.ask_close_or_zero(), 101.5);
        assert_eq!(view.open(), 100.5);
        assert_eq!(view.close(), 101.0);
        assert_eq!(view.bid_size(), 10.0);
        assert_eq!(view.ask_size(), 11.0);
    }

    #[test]
    fn symbol_alias_map_resolves_value_and_permtick() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let sid = sym.id.sid;
        let mut aliases = SymbolAliasMap::new();

        aliases.insert_symbol(sid, &sym);

        assert_eq!(aliases.resolve_ticker(&sym.value), Some(sid));
        assert_eq!(aliases.resolve_ticker(&sym.permtick), Some(sid));
        assert_eq!(aliases.resolve_ticker("QQQ"), None);
    }

    #[test]
    fn custom_data_view_normalizes_keys_and_returns_latest() {
        let date = chrono::NaiveDate::from_ymd_opt(2025, 9, 3).unwrap();
        let stamp = rlean_core::DateTime::from(date.and_hms_opt(16, 0, 0).unwrap());
        let first = CustomDataPoint {
            time: stamp,
            end_time: stamp,
            value: dec!(1),
            symbol: None,
            venue: None,
            fields: Arc::new(HashMap::new()),
        };
        let second = CustomDataPoint {
            time: stamp,
            end_time: stamp,
            value: dec!(2),
            symbol: None,
            venue: None,
            fields: Arc::new(HashMap::new()),
        };
        let view =
            CustomDataView::new(HashMap::from([("sweeps".to_string(), vec![first, second])]));

        assert!(view.contains("SWEEPS"));
        assert!(view.contains("sweeps"));
        assert_eq!(view.get("Sweeps").unwrap().len(), 2);
        assert_eq!(
            CustomDataPointView::new(view.latest("sweeps").unwrap().clone()).value(),
            2.0
        );
        assert_eq!(view.keys().next().unwrap(), "SWEEPS");
    }

    #[test]
    fn ns_to_exchange_naive_converts_utc_to_eastern() {
        // 2025-01-15 18:30:00 UTC == 13:30:00 US/Eastern (EST, UTC-5).
        let utc = chrono::NaiveDate::from_ymd_opt(2025, 1, 15)
            .unwrap()
            .and_hms_opt(18, 30, 0)
            .unwrap();
        let ns = utc.and_utc().timestamp_nanos_opt().unwrap();
        let eastern = ns_to_exchange_naive(ns);
        assert_eq!(
            eastern,
            chrono::NaiveDate::from_ymd_opt(2025, 1, 15)
                .unwrap()
                .and_hms_opt(13, 30, 0)
                .unwrap()
        );
    }

    #[test]
    fn ns_to_exchange_naive_honors_daylight_saving() {
        // 2025-07-15 18:30:00 UTC == 14:30:00 US/Eastern (EDT, UTC-4).
        let utc = chrono::NaiveDate::from_ymd_opt(2025, 7, 15)
            .unwrap()
            .and_hms_opt(18, 30, 0)
            .unwrap();
        let ns = utc.and_utc().timestamp_nanos_opt().unwrap();
        let eastern = ns_to_exchange_naive(ns);
        assert_eq!(
            eastern.time(),
            chrono::NaiveTime::from_hms_opt(14, 30, 0).unwrap()
        );
    }

    #[test]
    fn tradebar_view_time_is_exchange_local() {
        let sym = Symbol::create_equity("SPY", &Market::usa());
        let utc = chrono::NaiveDate::from_ymd_opt(2025, 1, 15)
            .unwrap()
            .and_hms_opt(21, 0, 0)
            .unwrap();
        let time = rlean_core::DateTime::from(utc);
        let bar = TradeBar::new(
            sym,
            time,
            rlean_core::TimeSpan::from_secs(60),
            TradeBarData::new(dec!(100), dec!(105), dec!(99), dec!(102), dec!(1000)),
        );
        let view = TradeBarView::new(bar);
        // 21:00 UTC -> 16:00 Eastern (market close, EST).
        assert_eq!(
            view.time().time(),
            chrono::NaiveTime::from_hms_opt(16, 0, 0).unwrap()
        );
    }

    #[test]
    fn custom_data_point_view_exposes_time_and_end_time_in_exchange_local() {
        // 2025-09-03 20:00 UTC == 16:00 US/Eastern (EDT). `time` and `end_time`
        // are both the real LEAN timestamps — no midnight fallback (#31/#81).
        let date = chrono::NaiveDate::from_ymd_opt(2025, 9, 3).unwrap();
        let time = rlean_core::DateTime::from(date.and_hms_opt(20, 0, 0).unwrap());
        let end_time = time + rlean_core::TimeSpan::from_days(1);
        let point = CustomDataPoint {
            time,
            end_time,
            value: dec!(1),
            symbol: None,
            venue: None,
            fields: Arc::new(HashMap::new()),
        };
        let view = CustomDataPointView::new(point);
        assert_eq!(
            view.time(),
            date.and_hms_opt(16, 0, 0).unwrap(),
            "time is exchange-local period start"
        );
        assert_eq!(
            view.end_time(),
            chrono::NaiveDate::from_ymd_opt(2025, 9, 4)
                .unwrap()
                .and_hms_opt(16, 0, 0)
                .unwrap(),
            "end_time is exchange-local emission gate (time + 1 day)"
        );
    }

    #[test]
    fn slice_view_has_data_includes_custom_data() {
        let date = chrono::NaiveDate::from_ymd_opt(2025, 9, 3).unwrap();
        let mut slice = Slice::new(rlean_core::DateTime::EPOCH);
        let stamp = rlean_core::DateTime::from(date.and_hms_opt(16, 0, 0).unwrap());
        slice.custom_data.insert(
            "macro".to_string(),
            vec![CustomDataPoint {
                time: stamp,
                end_time: stamp,
                value: dec!(1),
                symbol: None,
                venue: None,
                fields: Arc::new(HashMap::new()),
            }],
        );

        assert!(SliceView::new(slice).has_data());
    }
}
