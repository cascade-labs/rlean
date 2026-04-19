use crate::{
    indicator::{Indicator, IndicatorResult},
    window::RollingWindow,
};
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug, Clone)]
pub struct VortexResult {
    pub plus: Decimal,
    pub minus: Decimal,
}

/// Vortex Indicator. VI+ and VI-.
pub struct Vortex {
    name: String,
    period: usize,
    // raw TR (H-L, no prev close) windows
    tr_window: RollingWindow<Decimal>,
    plus_vm_window: RollingWindow<Decimal>,
    minus_vm_window: RollingWindow<Decimal>,
    prev_bar: Option<(Decimal, Decimal, Decimal)>, // high, low, close
    samples: usize,
    current: IndicatorResult,
    pub last_result: VortexResult,
}

impl Vortex {
    pub fn new(period: usize) -> Self {
        Vortex {
            name: format!("VTX({})", period),
            period,
            tr_window: RollingWindow::new(period),
            plus_vm_window: RollingWindow::new(period),
            minus_vm_window: RollingWindow::new(period),
            prev_bar: None,
            samples: 0,
            current: IndicatorResult::not_ready(),
            last_result: VortexResult {
                plus: dec!(0),
                minus: dec!(0),
            },
        }
    }

    fn sum_window(w: &RollingWindow<Decimal>) -> Decimal {
        w.iter().copied().sum()
    }
}

impl Indicator for Vortex {
    fn name(&self) -> &str {
        &self.name
    }
    fn is_ready(&self) -> bool {
        self.samples >= self.period
    }
    fn current(&self) -> IndicatorResult {
        self.current.clone()
    }
    fn samples(&self) -> usize {
        self.samples
    }
    fn warm_up_period(&self) -> usize {
        self.period
    }

    fn reset(&mut self) {
        self.tr_window.clear();
        self.plus_vm_window.clear();
        self.minus_vm_window.clear();
        self.prev_bar = None;
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
        self.last_result = VortexResult {
            plus: dec!(0),
            minus: dec!(0),
        };
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.samples += 1;

        // TR = max(H-L, |H-prevC|, |L-prevC|)
        let tr = if let Some((_, _, prev_c)) = self.prev_bar {
            let hl = bar.high - bar.low;
            let hc = (bar.high - prev_c).abs();
            let lc = (bar.low - prev_c).abs();
            hl.max(hc).max(lc)
        } else {
            bar.high - bar.low
        };

        self.tr_window.push(tr);

        if let Some((prev_h, prev_l, _)) = self.prev_bar {
            let plus_vm = (bar.high - prev_l).abs();
            let minus_vm = (bar.low - prev_h).abs();
            self.plus_vm_window.push(plus_vm);
            self.minus_vm_window.push(minus_vm);
        }

        self.prev_bar = Some((bar.high, bar.low, bar.close));

        if self.is_ready() && self.plus_vm_window.is_full() {
            let tr_sum = Self::sum_window(&self.tr_window);
            if tr_sum != dec!(0) {
                let plus = Self::sum_window(&self.plus_vm_window) / tr_sum;
                let minus = Self::sum_window(&self.minus_vm_window) / tr_sum;
                self.last_result = VortexResult { plus, minus };
                self.current = IndicatorResult::ready((plus + minus) / dec!(2), bar.time);
            }
        }

        self.current.clone()
    }
}
