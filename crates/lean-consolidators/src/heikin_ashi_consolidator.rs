use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::consolidator::IConsolidator;

/// Heikin Ashi bar consolidator.
///
/// Transforms each incoming TradeBar into a smoothed Heikin Ashi bar using:
/// ```text
/// HA_Close = (O + H + L + C) / 4
/// HA_Open  = (prev_HA_Open + prev_HA_Close) / 2   [first bar: (O + C) / 2]
/// HA_High  = max(H, HA_Open, HA_Close)
/// HA_Low   = min(L, HA_Open, HA_Close)
/// ```
///
/// Each input bar produces exactly one output bar (no aggregation), so `update()` always
/// returns `Some(bar)` after the first tick.
pub struct HeikinAshiConsolidator {
    prev_ha_open: Option<Decimal>,
    prev_ha_close: Option<Decimal>,
}

impl HeikinAshiConsolidator {
    pub fn new() -> Self {
        Self {
            prev_ha_open: None,
            prev_ha_close: None,
        }
    }
}

impl Default for HeikinAshiConsolidator {
    fn default() -> Self {
        Self::new()
    }
}

impl IConsolidator for HeikinAshiConsolidator {
    fn update(&mut self, bar: &TradeBar) -> Option<TradeBar> {
        let four = dec!(4);
        let two = dec!(2);

        let ha_close = (bar.open + bar.high + bar.low + bar.close) / four;

        let ha_open = match (self.prev_ha_open, self.prev_ha_close) {
            (Some(po), Some(pc)) => (po + pc) / two,
            _ => (bar.open + bar.close) / two, // first bar seed
        };

        let ha_high = bar.high.max(ha_open).max(ha_close);
        let ha_low = bar.low.min(ha_open).min(ha_close);

        self.prev_ha_open = Some(ha_open);
        self.prev_ha_close = Some(ha_close);

        Some(TradeBar {
            symbol: bar.symbol.clone(),
            time: bar.time,
            end_time: bar.end_time,
            open: ha_open,
            high: ha_high,
            low: ha_low,
            close: ha_close,
            volume: bar.volume,
            period: bar.period,
        })
    }

    fn reset(&mut self) {
        self.prev_ha_open = None;
        self.prev_ha_close = None;
    }

    fn name(&self) -> &str {
        "HeikinAshiConsolidator"
    }
}
