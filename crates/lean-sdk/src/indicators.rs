//! SDK indicator API surface.
//!
//! Rust users get the native `lean-indicators` types directly. Python-facing
//! SDK classes are thin annotated wrappers around those same native types so
//! bindgen can expose them without reimplementing indicator logic.

pub use lean_indicators::williams_r::WilliamsR as WilliamsPercentR;
pub use lean_indicators::*;

use lean_core::{DateTime, NanosecondTimestamp, Price};
use lean_sdk_annotations::{sdk_bind, sdk_getter, sdk_method, sdk_new};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use rust_decimal::Decimal;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

pub const DEFAULT_INDICATOR_TIME: DateTime = DateTime::EPOCH;

fn price_from_f64(value: f64) -> Price {
    Decimal::from_f64(value).unwrap_or_default()
}

fn f64_from_price(value: Price) -> f64 {
    value.to_f64().unwrap_or(0.0)
}

fn timestamp_from_exchange_naive(time: chrono::NaiveDateTime) -> NanosecondTimestamp {
    use chrono::{TimeZone, Utc};
    use chrono_tz::US::Eastern;

    let local = Eastern.from_local_datetime(&time);
    let utc = local
        .single()
        .or_else(|| local.earliest())
        .or_else(|| local.latest())
        .map(|time| time.with_timezone(&Utc))
        .unwrap_or_else(|| Utc.from_utc_datetime(&time));
    NanosecondTimestamp::from(utc)
}

fn current_point<I: lean_indicators::indicator::Indicator>(
    indicator: &I,
) -> IndicatorDataPointView {
    let current = indicator.current();
    IndicatorDataPointView {
        value: f64_from_price(current.value),
        time: current.time.0,
    }
}


pub trait RegisteredIndicator: Send + Sync {
    fn update_value(&self, time: DateTime, value: Price) -> bool;

    fn update_bar(&self, bar: &lean_data::TradeBar) -> bool {
        self.update_value(bar.end_time, bar.close)
    }
}

fn naive_from_timestamp(time: DateTime) -> chrono::NaiveDateTime {
    chrono::DateTime::from_timestamp_nanos(time.0).naive_utc()
}

macro_rules! impl_registered_price_indicator {
    ($type_name:ty) => {
        impl RegisteredIndicator for $type_name {
            fn update_value(&self, time: DateTime, value: Price) -> bool {
                self.inner
                    .lock()
                    .expect("indicator lock poisoned")
                    .update_price(time, value)
                    .is_ready()
            }
        }

        impl lean_algorithm::lifecycle::RegisteredIndicatorBridge for $type_name {
            fn update_bar(&self, bar: &lean_data::TradeBar) -> bool {
                RegisteredIndicator::update_bar(self, bar)
            }
        }
    };
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[sdk_bind(py_name = "IndicatorDataPoint")]
pub struct IndicatorDataPointView {
    value: f64,
    time: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[sdk_bind(py_name = "IndicatorResult")]
pub struct IndicatorResultView {
    value: f64,
    is_ready: bool,
}

impl IndicatorResultView {
    pub fn from_result(result: lean_indicators::IndicatorResult) -> Self {
        Self {
            value: f64_from_price(result.value),
            is_ready: result.is_ready(),
        }
    }

    #[sdk_getter(alias = "Value")]
    pub fn value(&self) -> f64 {
        self.value
    }

    #[sdk_getter]
    pub fn is_ready(&self) -> bool {
        self.is_ready
    }
}

impl IndicatorDataPointView {
    #[sdk_getter]
    pub fn value(&self) -> f64 {
        self.value
    }

    #[sdk_getter]
    pub fn time(&self) -> i64 {
        self.time
    }
}

#[sdk_bind(py_name = "SimpleMovingAverage")]
pub struct SimpleMovingAverage {
    inner: Arc<Mutex<lean_indicators::Sma>>,
}

impl SimpleMovingAverage {
    #[sdk_new]
    pub fn new(period: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(lean_indicators::Sma::new(period))),
        }
    }

    #[sdk_method]
    pub fn update(&mut self, time: chrono::NaiveDateTime, value: f64) -> bool {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .update_price(timestamp_from_exchange_naive(time), price_from_f64(value))
            .is_ready()
    }

    #[sdk_getter(alias = "IsReady")]
    pub fn is_ready(&self) -> bool {
        self.inner.lock().expect("indicator lock poisoned").is_ready()
    }

    #[sdk_getter(alias = "Current")]
    pub fn current(&self) -> IndicatorDataPointView {
        current_point(&*self.inner.lock().expect("indicator lock poisoned"))
    }

    #[sdk_getter]
    pub fn samples(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").samples()
    }

    #[sdk_getter]
    pub fn warm_up_period(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").warm_up_period()
    }

    #[sdk_method]
    pub fn reset(&mut self) {
        self.inner.lock().expect("indicator lock poisoned").reset();
    }

    pub fn registered_handle(&self) -> Arc<dyn lean_algorithm::lifecycle::RegisteredIndicatorBridge> {
        Arc::new(self.clone())
    }
}

impl Clone for SimpleMovingAverage {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl_registered_price_indicator!(SimpleMovingAverage);

#[sdk_bind(py_name = "ExponentialMovingAverage")]
pub struct ExponentialMovingAverage {
    inner: Arc<Mutex<lean_indicators::Ema>>,
}

impl ExponentialMovingAverage {
    #[sdk_new]
    pub fn new(period: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(lean_indicators::Ema::new(period))),
        }
    }

    #[sdk_method]
    pub fn update(&mut self, time: chrono::NaiveDateTime, value: f64) -> bool {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .update_price(timestamp_from_exchange_naive(time), price_from_f64(value))
            .is_ready()
    }

    #[sdk_getter(alias = "IsReady")]
    pub fn is_ready(&self) -> bool {
        self.inner.lock().expect("indicator lock poisoned").is_ready()
    }

    #[sdk_getter(alias = "Current")]
    pub fn current(&self) -> IndicatorDataPointView {
        current_point(&*self.inner.lock().expect("indicator lock poisoned"))
    }

    #[sdk_getter]
    pub fn samples(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").samples()
    }

    #[sdk_getter]
    pub fn warm_up_period(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").warm_up_period()
    }

    #[sdk_method]
    pub fn reset(&mut self) {
        self.inner.lock().expect("indicator lock poisoned").reset();
    }

    pub fn registered_handle(&self) -> Arc<dyn lean_algorithm::lifecycle::RegisteredIndicatorBridge> {
        Arc::new(self.clone())
    }
}

impl Clone for ExponentialMovingAverage {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl_registered_price_indicator!(ExponentialMovingAverage);

#[sdk_bind(py_name = "RelativeStrengthIndex")]
pub struct RelativeStrengthIndex {
    inner: Arc<Mutex<lean_indicators::Rsi>>,
}

impl RelativeStrengthIndex {
    #[sdk_new]
    pub fn new(period: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(lean_indicators::Rsi::new(period))),
        }
    }

    #[sdk_method]
    pub fn update(&mut self, time: chrono::NaiveDateTime, value: f64) -> bool {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .update_price(timestamp_from_exchange_naive(time), price_from_f64(value))
            .is_ready()
    }

    #[sdk_getter(alias = "IsReady")]
    pub fn is_ready(&self) -> bool {
        self.inner.lock().expect("indicator lock poisoned").is_ready()
    }

    #[sdk_getter(alias = "Current")]
    pub fn current(&self) -> IndicatorDataPointView {
        current_point(&*self.inner.lock().expect("indicator lock poisoned"))
    }

    #[sdk_getter]
    pub fn samples(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").samples()
    }

    #[sdk_getter]
    pub fn warm_up_period(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").warm_up_period()
    }

    #[sdk_method]
    pub fn reset(&mut self) {
        self.inner.lock().expect("indicator lock poisoned").reset();
    }

    pub fn registered_handle(&self) -> Arc<dyn lean_algorithm::lifecycle::RegisteredIndicatorBridge> {
        Arc::new(self.clone())
    }
}

impl Clone for RelativeStrengthIndex {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl_registered_price_indicator!(RelativeStrengthIndex);

#[sdk_bind(py_name = "MomentumPercent")]
pub struct MomentumPercentIndicator {
    inner: Arc<Mutex<MomentumPercentState>>,
}

struct MomentumPercentState {
    period: usize,
    values: VecDeque<(chrono::NaiveDateTime, f64)>,
}

impl MomentumPercentIndicator {
    #[sdk_new]
    pub fn new(period: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(MomentumPercentState {
                period,
                values: VecDeque::with_capacity(period.saturating_add(1)),
            })),
        }
    }

    #[sdk_method]
    pub fn update(&mut self, time: chrono::NaiveDateTime, value: f64) -> bool {
        let mut inner = self.inner.lock().expect("indicator lock poisoned");
        inner.values.push_back((time, value));
        while inner.values.len() > inner.period.saturating_add(1) {
            inner.values.pop_front();
        }
        inner.period > 0 && inner.values.len() > inner.period
    }

    #[sdk_getter(alias = "IsReady")]
    pub fn is_ready(&self) -> bool {
        let inner = self.inner.lock().expect("indicator lock poisoned");
        inner.period > 0 && inner.values.len() > inner.period
    }

    #[sdk_getter(alias = "Current")]
    pub fn current(&self) -> IndicatorDataPointView {
        let inner = self.inner.lock().expect("indicator lock poisoned");
        let Some((time, latest)) = inner.values.back().copied() else {
            return IndicatorDataPointView {
                value: 0.0,
                time: DEFAULT_INDICATOR_TIME.0,
            };
        };
        let value = inner
            .values
            .front()
            .map(|(_, previous)| {
                if *previous == 0.0 {
                    0.0
                } else {
                    (latest / previous) - 1.0
                }
            })
            .unwrap_or(0.0);
        IndicatorDataPointView {
            value,
            time: timestamp_from_exchange_naive(time).0,
        }
    }

    #[sdk_getter]
    pub fn samples(&self) -> usize {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .values
            .len()
    }

    #[sdk_getter]
    pub fn warm_up_period(&self) -> usize {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .period
            .saturating_add(1)
    }

    #[sdk_method]
    pub fn reset(&mut self) {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .values
            .clear();
    }

    pub fn registered_handle(&self) -> Arc<dyn lean_algorithm::lifecycle::RegisteredIndicatorBridge> {
        Arc::new(self.clone())
    }
}

impl Clone for MomentumPercentIndicator {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl RegisteredIndicator for MomentumPercentIndicator {
    fn update_value(&self, time: DateTime, value: Price) -> bool {
        let mut inner = self.inner.lock().expect("indicator lock poisoned");
        inner
            .values
            .push_back((naive_from_timestamp(time), f64_from_price(value)));
        while inner.values.len() > inner.period.saturating_add(1) {
            inner.values.pop_front();
        }
        inner.period > 0 && inner.values.len() > inner.period
    }
}

impl lean_algorithm::lifecycle::RegisteredIndicatorBridge for MomentumPercentIndicator {
    fn update_bar(&self, bar: &lean_data::TradeBar) -> bool {
        RegisteredIndicator::update_bar(self, bar)
    }
}

#[sdk_bind(py_name = "StandardDeviation")]
pub struct StandardDeviationIndicator {
    inner: Arc<Mutex<StandardDeviationState>>,
}

struct StandardDeviationState {
    period: usize,
    values: VecDeque<(chrono::NaiveDateTime, f64)>,
}

impl StandardDeviationIndicator {
    #[sdk_new]
    pub fn new(period: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(StandardDeviationState {
                period,
                values: VecDeque::with_capacity(period),
            })),
        }
    }

    #[sdk_method]
    pub fn update(&mut self, time: chrono::NaiveDateTime, value: f64) -> bool {
        let mut inner = self.inner.lock().expect("indicator lock poisoned");
        inner.values.push_back((time, value));
        while inner.values.len() > inner.period {
            inner.values.pop_front();
        }
        inner.period > 0 && inner.values.len() >= inner.period
    }

    #[sdk_getter(alias = "IsReady")]
    pub fn is_ready(&self) -> bool {
        let inner = self.inner.lock().expect("indicator lock poisoned");
        inner.period > 0 && inner.values.len() >= inner.period
    }

    #[sdk_getter(alias = "Current")]
    pub fn current(&self) -> IndicatorDataPointView {
        let inner = self.inner.lock().expect("indicator lock poisoned");
        let Some((time, _)) = inner.values.back().copied() else {
            return IndicatorDataPointView {
                value: 0.0,
                time: DEFAULT_INDICATOR_TIME.0,
            };
        };
        let count = inner.values.len() as f64;
        let mean = inner.values.iter().map(|(_, value)| *value).sum::<f64>() / count;
        let variance = inner
            .values
            .iter()
            .map(|(_, value)| {
                let diff = value - mean;
                diff * diff
            })
            .sum::<f64>()
            / count;
        IndicatorDataPointView {
            value: variance.sqrt(),
            time: timestamp_from_exchange_naive(time).0,
        }
    }

    #[sdk_getter]
    pub fn samples(&self) -> usize {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .values
            .len()
    }

    #[sdk_getter]
    pub fn warm_up_period(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").period
    }

    #[sdk_method]
    pub fn reset(&mut self) {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .values
            .clear();
    }

    pub fn registered_handle(&self) -> Arc<dyn lean_algorithm::lifecycle::RegisteredIndicatorBridge> {
        Arc::new(self.clone())
    }
}

impl Clone for StandardDeviationIndicator {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl RegisteredIndicator for StandardDeviationIndicator {
    fn update_value(&self, time: DateTime, value: Price) -> bool {
        let mut inner = self.inner.lock().expect("indicator lock poisoned");
        inner
            .values
            .push_back((naive_from_timestamp(time), f64_from_price(value)));
        while inner.values.len() > inner.period {
            inner.values.pop_front();
        }
        inner.period > 0 && inner.values.len() >= inner.period
    }
}

impl lean_algorithm::lifecycle::RegisteredIndicatorBridge for StandardDeviationIndicator {
    fn update_bar(&self, bar: &lean_data::TradeBar) -> bool {
        RegisteredIndicator::update_bar(self, bar)
    }
}

#[sdk_bind(py_name = "BollingerBands")]
pub struct BollingerBandsIndicator {
    inner: Arc<Mutex<lean_indicators::BollingerBands>>,
}

impl BollingerBandsIndicator {
    #[sdk_new]
    pub fn new(period: usize, k: f64) -> Self {
        Self {
            inner: Arc::new(Mutex::new(lean_indicators::BollingerBands::new(period, price_from_f64(k)))),
        }
    }

    #[sdk_method]
    pub fn update(&mut self, time: chrono::NaiveDateTime, value: f64) -> bool {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .update_price(timestamp_from_exchange_naive(time), price_from_f64(value))
            .is_ready()
    }

    #[sdk_getter]
    pub fn is_ready(&self) -> bool {
        self.inner.lock().expect("indicator lock poisoned").is_ready()
    }

    #[sdk_getter]
    pub fn current(&self) -> IndicatorDataPointView {
        current_point(&*self.inner.lock().expect("indicator lock poisoned"))
    }

    #[sdk_getter]
    pub fn samples(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").samples()
    }

    #[sdk_getter]
    pub fn warm_up_period(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").warm_up_period()
    }

    #[sdk_method]
    pub fn reset(&mut self) {
        self.inner.lock().expect("indicator lock poisoned").reset();
    }

    pub fn registered_handle(&self) -> Arc<dyn lean_algorithm::lifecycle::RegisteredIndicatorBridge> {
        Arc::new(self.clone())
    }
}

impl Clone for BollingerBandsIndicator {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl_registered_price_indicator!(BollingerBandsIndicator);

#[sdk_bind(py_name = "MovingAverageConvergenceDivergence")]
pub struct MacdIndicator {
    inner: Arc<Mutex<lean_indicators::Macd>>,
}

#[sdk_bind(py_name = "AverageTrueRange")]
pub struct AverageTrueRange {
    inner: Arc<Mutex<lean_indicators::Atr>>,
}

impl AverageTrueRange {
    #[sdk_new]
    pub fn new(period: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(lean_indicators::Atr::new(period))),
        }
    }

    #[sdk_getter]
    pub fn is_ready(&self) -> bool {
        self.inner.lock().expect("indicator lock poisoned").is_ready()
    }

    #[sdk_getter]
    pub fn current(&self) -> IndicatorDataPointView {
        current_point(&*self.inner.lock().expect("indicator lock poisoned"))
    }

    #[sdk_getter]
    pub fn samples(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").samples()
    }

    #[sdk_getter]
    pub fn warm_up_period(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").warm_up_period()
    }

    #[sdk_method]
    pub fn reset(&mut self) {
        self.inner.lock().expect("indicator lock poisoned").reset();
    }

    pub fn registered_handle(&self) -> Arc<dyn lean_algorithm::lifecycle::RegisteredIndicatorBridge> {
        Arc::new(self.clone())
    }
}

impl Clone for AverageTrueRange {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl RegisteredIndicator for AverageTrueRange {
    fn update_value(&self, _time: DateTime, _value: Price) -> bool {
        // ATR needs full trade-bar inputs. It is registered for LEAN API
        // compatibility, but price-only auto-updates are intentionally skipped.
        self.is_ready()
    }

    fn update_bar(&self, bar: &lean_data::TradeBar) -> bool {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .update_bar(bar)
            .is_ready()
    }
}

impl lean_algorithm::lifecycle::RegisteredIndicatorBridge for AverageTrueRange {
    fn update_bar(&self, bar: &lean_data::TradeBar) -> bool {
        RegisteredIndicator::update_bar(self, bar)
    }
}

impl MacdIndicator {
    #[sdk_new]
    pub fn new(fast_period: usize, slow_period: usize, signal_period: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(lean_indicators::Macd::new(fast_period, slow_period, signal_period))),
        }
    }

    #[sdk_method]
    pub fn update(&mut self, time: chrono::NaiveDateTime, value: f64) -> bool {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .update_price(timestamp_from_exchange_naive(time), price_from_f64(value))
            .is_ready()
    }

    #[sdk_getter]
    pub fn is_ready(&self) -> bool {
        self.inner.lock().expect("indicator lock poisoned").is_ready()
    }

    #[sdk_getter]
    pub fn current(&self) -> IndicatorDataPointView {
        current_point(&*self.inner.lock().expect("indicator lock poisoned"))
    }

    #[sdk_getter]
    pub fn samples(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").samples()
    }

    #[sdk_getter]
    pub fn warm_up_period(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").warm_up_period()
    }

    #[sdk_method]
    pub fn reset(&mut self) {
        self.inner.lock().expect("indicator lock poisoned").reset();
    }

    pub fn registered_handle(&self) -> Arc<dyn lean_algorithm::lifecycle::RegisteredIndicatorBridge> {
        Arc::new(self.clone())
    }
}

impl Clone for MacdIndicator {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl_registered_price_indicator!(MacdIndicator);

#[derive(Clone)]
#[sdk_bind(py_name = "Identity")]
pub struct IdentityIndicator {
    inner: Arc<Mutex<IdentityState>>,
}

struct IdentityState {
    current: IndicatorDataPointView,
    samples: usize,
}

impl IdentityIndicator {
    #[sdk_new]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(IdentityState {
                current: IndicatorDataPointView {
                    value: 0.0,
                    time: DEFAULT_INDICATOR_TIME.0,
                },
                samples: 0,
            })),
        }
    }

    #[sdk_method]
    pub fn update(&mut self, time: chrono::NaiveDateTime, value: f64) -> bool {
        let mut inner = self.inner.lock().expect("indicator lock poisoned");
        inner.current = IndicatorDataPointView {
            value,
            time: timestamp_from_exchange_naive(time).0,
        };
        inner.samples += 1;
        true
    }

    #[sdk_getter]
    pub fn is_ready(&self) -> bool {
        self.inner.lock().expect("indicator lock poisoned").samples > 0
    }

    #[sdk_getter]
    pub fn current(&self) -> IndicatorDataPointView {
        self.inner
            .lock()
            .expect("indicator lock poisoned")
            .current
    }

    #[sdk_getter]
    pub fn samples(&self) -> usize {
        self.inner.lock().expect("indicator lock poisoned").samples
    }

    #[sdk_getter]
    pub fn warm_up_period(&self) -> usize {
        0
    }

    #[sdk_method]
    pub fn reset(&mut self) {
        let mut inner = self.inner.lock().expect("indicator lock poisoned");
        inner.current = IndicatorDataPointView {
            value: 0.0,
            time: DEFAULT_INDICATOR_TIME.0,
        };
        inner.samples = 0;
    }

    pub fn registered_handle(&self) -> Arc<dyn lean_algorithm::lifecycle::RegisteredIndicatorBridge> {
        Arc::new(self.clone())
    }
}

impl RegisteredIndicator for IdentityIndicator {
    fn update_value(&self, time: DateTime, value: Price) -> bool {
        let mut inner = self.inner.lock().expect("indicator lock poisoned");
        inner.current = IndicatorDataPointView {
            value: f64_from_price(value),
            time: time.0,
        };
        inner.samples += 1;
        true
    }
}

impl lean_algorithm::lifecycle::RegisteredIndicatorBridge for IdentityIndicator {
    fn update_bar(&self, bar: &lean_data::TradeBar) -> bool {
        RegisteredIndicator::update_bar(self, bar)
    }
}

impl Default for IdentityIndicator {
    fn default() -> Self {
        Self::new()
    }
}
