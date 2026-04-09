use crate::indicator::{Indicator, IndicatorResult};
use crate::window::RollingWindow;
use lean_core::{DateTime, Price};
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug, Clone)]
pub struct IchimokuResult {
    pub tenkan: Option<Decimal>,
    pub kijun: Option<Decimal>,
    pub senkou_a: Option<Decimal>,
    pub senkou_b: Option<Decimal>,
    pub chikou: Option<Decimal>,
}

/// Ichimoku Kinko Hyo.
pub struct Ichimoku {
    name: String,
    tenkan_period: usize,
    kijun_period: usize,
    senkou_b_period: usize,
    displacement: usize,
    // Rolling windows for highs/lows
    tenkan_highs: RollingWindow<Decimal>,
    tenkan_lows: RollingWindow<Decimal>,
    kijun_highs: RollingWindow<Decimal>,
    kijun_lows: RollingWindow<Decimal>,
    senkou_b_highs: RollingWindow<Decimal>,
    senkou_b_lows: RollingWindow<Decimal>,
    // Delay buffers for senkou
    senkou_a_buf: RollingWindow<Decimal>,
    senkou_b_buf: RollingWindow<Decimal>,
    // Close buffer for chikou
    chikou_buf: RollingWindow<Decimal>,
    samples: usize,
    current: IndicatorResult,
    pub last_result: IchimokuResult,
}

impl Ichimoku {
    pub fn new(tenkan: usize, kijun: usize, senkou_b: usize, displacement: usize) -> Self {
        Ichimoku {
            name: format!("ICHIMOKU({},{},{},{})", tenkan, kijun, senkou_b, displacement),
            tenkan_period: tenkan,
            kijun_period: kijun,
            senkou_b_period: senkou_b,
            displacement,
            tenkan_highs: RollingWindow::new(tenkan),
            tenkan_lows: RollingWindow::new(tenkan),
            kijun_highs: RollingWindow::new(kijun),
            kijun_lows: RollingWindow::new(kijun),
            senkou_b_highs: RollingWindow::new(senkou_b),
            senkou_b_lows: RollingWindow::new(senkou_b),
            senkou_a_buf: RollingWindow::new(displacement),
            senkou_b_buf: RollingWindow::new(displacement),
            chikou_buf: RollingWindow::new(displacement),
            samples: 0,
            current: IndicatorResult::not_ready(),
            last_result: IchimokuResult {
                tenkan: None, kijun: None, senkou_a: None, senkou_b: None, chikou: None
            },
        }
    }

    pub fn default() -> Self {
        Self::new(9, 26, 52, 26)
    }

    fn window_midpoint(highs: &RollingWindow<Decimal>, lows: &RollingWindow<Decimal>) -> Option<Decimal> {
        if !highs.is_full() { return None; }
        let max_h = highs.iter().copied().fold(Decimal::MIN, Decimal::max);
        let min_l = lows.iter().copied().fold(Decimal::MAX, Decimal::min);
        Some((max_h + min_l) / dec!(2))
    }

    pub fn update_ichimoku(&mut self, bar: &TradeBar) -> IchimokuResult {
        self.samples += 1;

        self.tenkan_highs.push(bar.high);
        self.tenkan_lows.push(bar.low);
        self.kijun_highs.push(bar.high);
        self.kijun_lows.push(bar.low);
        self.senkou_b_highs.push(bar.high);
        self.senkou_b_lows.push(bar.low);
        self.chikou_buf.push(bar.close);

        let tenkan = Self::window_midpoint(&self.tenkan_highs, &self.tenkan_lows);
        let kijun = Self::window_midpoint(&self.kijun_highs, &self.kijun_lows);
        let senkou_b_raw = Self::window_midpoint(&self.senkou_b_highs, &self.senkou_b_lows);

        let senkou_a_raw = match (tenkan, kijun) {
            (Some(t), Some(k)) => Some((t + k) / dec!(2)),
            _ => None,
        };

        // Buffer senkou lines for displacement
        if let Some(sa) = senkou_a_raw {
            self.senkou_a_buf.push(sa);
        }
        if let Some(sb) = senkou_b_raw {
            self.senkou_b_buf.push(sb);
        }

        let senkou_a = if self.senkou_a_buf.is_full() {
            self.senkou_a_buf.oldest().copied()
        } else { None };

        let senkou_b = if self.senkou_b_buf.is_full() {
            self.senkou_b_buf.oldest().copied()
        } else { None };

        // chikou: close displaced back by kijun period
        let chikou = if self.chikou_buf.is_full() {
            self.chikou_buf.oldest().copied()
        } else { None };

        self.last_result = IchimokuResult { tenkan, kijun, senkou_a, senkou_b, chikou };

        if tenkan.is_some() && kijun.is_some() {
            self.current = IndicatorResult::ready(tenkan.unwrap_or(dec!(0)), bar.time);
        }

        self.last_result.clone()
    }
}

impl Indicator for Ichimoku {
    fn name(&self) -> &str { &self.name }
    fn is_ready(&self) -> bool { self.tenkan_highs.is_full() && self.kijun_highs.is_full() }
    fn current(&self) -> IndicatorResult { self.current.clone() }
    fn samples(&self) -> usize { self.samples }
    fn warm_up_period(&self) -> usize {
        let a = self.tenkan_period + self.displacement;
        let b = self.kijun_period + self.displacement;
        let c = self.senkou_b_period + self.displacement;
        a.max(b).max(c)
    }

    fn reset(&mut self) {
        self.tenkan_highs.clear();
        self.tenkan_lows.clear();
        self.kijun_highs.clear();
        self.kijun_lows.clear();
        self.senkou_b_highs.clear();
        self.senkou_b_lows.clear();
        self.senkou_a_buf.clear();
        self.senkou_b_buf.clear();
        self.chikou_buf.clear();
        self.samples = 0;
        self.current = IndicatorResult::not_ready();
    }

    fn update_price(&mut self, _time: DateTime, _value: Price) -> IndicatorResult {
        self.current.clone()
    }

    fn update_bar(&mut self, bar: &TradeBar) -> IndicatorResult {
        self.update_ichimoku(bar);
        self.current.clone()
    }
}
