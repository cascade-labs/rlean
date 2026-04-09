use crate::{indicator::{Indicator, IndicatorResult}, window::RollingWindow};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// Chaikin Money Flow. sum(MFV) / sum(volume) over n.
pub struct ChaikinMoneyFlow {
    name: String,
    period: usize,
    mfv_window: RollingWindow<Decimal>,
    vol_window: RollingWindow<Decimal>,
    mfv_sum: Decimal,
    vol_sum: Decimal,
    samples: usize,
    current: IndicatorResult,
}

impl ChaikinMoneyFlow {
    pub fn new(period: usize) -> Self {
        ChaikinMoneyFlow {
            name: format!("CMF({})", period),
            period,
            mfv_window: RollingWindow::new(period),
            vol_window: RollingWindow::new(period),
            mfv_sum: dec!(0),
            vol_sum: dec!(0),
            samples: 0,
            current: IndicatorResult::not_ready(),
        }
    }
}

impl Indicator for ChaikinMoneyFlow {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.mfv_window.is_full() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize { self.period }

    fn reset(&mut self) {
        self.mfv_window.clear();
        self.vol_window.clear();
        self.mfv_sum = dec!(0);
        self.vol_sum = dec!(0);
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;
        let range = bar.high - bar.low;
        let mfm = if range > dec!(0) {
            ((bar.close - bar.low) - (bar.high - bar.close)) / range
        } else {
            dec!(0)
        };
        let mfv = mfm * bar.volume;

        if self.mfv_window.is_full() {
            if let Some(old_mfv) = self.mfv_window.oldest() {
                self.mfv_sum -= *old_mfv;
            }
            if let Some(old_vol) = self.vol_window.oldest() {
                self.vol_sum -= *old_vol;
            }
        }

        self.mfv_window.push(mfv);
        self.vol_window.push(bar.volume);
        self.mfv_sum += mfv;
        self.vol_sum += bar.volume;

        if self.is_ready() && self.vol_sum != dec!(0) {
            self.current = IndicatorResult::ready(self.mfv_sum / self.vol_sum, bar.time);
        }

        self.current.clone()
    }
}
