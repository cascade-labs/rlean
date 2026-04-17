use lean_core::TimeSpan;
use lean_data::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::consolidator::IConsolidator;

/// Volume-based bar consolidator. Emits a bar when accumulated volume reaches `volume_per_bar`.
/// Mirrors the volume-bar logic in C# VolumeRenkoConsolidator but produces plain TradeBars.
pub struct VolumeConsolidator {
    volume_per_bar: Decimal,
    working: Option<TradeBar>,
    accumulated_volume: Decimal,
}

impl VolumeConsolidator {
    pub fn new(volume_per_bar: Decimal) -> Self {
        assert!(volume_per_bar > dec!(0), "volume_per_bar must be > 0");
        Self {
            volume_per_bar,
            working: None,
            accumulated_volume: dec!(0),
        }
    }
}

impl IConsolidator for VolumeConsolidator {
    fn update(&mut self, bar: &TradeBar) -> Option<TradeBar> {
        match &mut self.working {
            None => {
                self.working = Some(bar.clone());
                self.accumulated_volume = bar.volume;
            }
            Some(w) => {
                if bar.high > w.high {
                    w.high = bar.high;
                }
                if bar.low < w.low {
                    w.low = bar.low;
                }
                w.close = bar.close;
                w.volume += bar.volume;
                w.end_time = bar.end_time;
                w.period = TimeSpan::from_nanos(w.end_time.0 - w.time.0);
                self.accumulated_volume += bar.volume;
            }
        }

        if self.accumulated_volume >= self.volume_per_bar {
            self.accumulated_volume = dec!(0);
            self.working.take()
        } else {
            None
        }
    }

    fn reset(&mut self) {
        self.working = None;
        self.accumulated_volume = dec!(0);
    }

    fn name(&self) -> &str {
        "VolumeConsolidator"
    }
}
