use crate::base_data::{BaseData, BaseDataType};
use lean_core::{DateTime, Price, Symbol, TimeSpan};
use serde::{Deserialize, Serialize};

/// Per-symbol context data for perpetual futures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PerpetualContext {
    pub symbol: Symbol,
    pub time: DateTime,
    pub end_time: DateTime,
    pub funding: Price,
    pub open_interest: Price,
    pub prev_day_px: Price,
    pub day_ntl_vlm: Price,
    pub premium: Price,
    pub oracle_px: Price,
    pub mark_px: Price,
    pub mid_px: Price,
    pub impact_bid_px: Price,
    pub impact_ask_px: Price,
    pub period: TimeSpan,
}

impl PerpetualContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        symbol: Symbol,
        time: DateTime,
        period: TimeSpan,
        funding: Price,
        open_interest: Price,
        prev_day_px: Price,
        day_ntl_vlm: Price,
        premium: Price,
        oracle_px: Price,
        mark_px: Price,
        mid_px: Price,
        impact_bid_px: Price,
        impact_ask_px: Price,
    ) -> Self {
        Self {
            symbol,
            time,
            end_time: time + period,
            funding,
            open_interest,
            prev_day_px,
            day_ntl_vlm,
            premium,
            oracle_px,
            mark_px,
            mid_px,
            impact_bid_px,
            impact_ask_px,
            period,
        }
    }
}

impl BaseData for PerpetualContext {
    fn data_type(&self) -> BaseDataType {
        BaseDataType::PerpetualContext
    }

    fn symbol(&self) -> &Symbol {
        &self.symbol
    }

    fn time(&self) -> DateTime {
        self.time
    }

    fn end_time(&self) -> DateTime {
        self.end_time
    }

    fn price(&self) -> Price {
        self.mark_px
    }

    fn clone_box(&self) -> Box<dyn BaseData> {
        Box::new(self.clone())
    }
}
