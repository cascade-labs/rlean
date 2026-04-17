use lean_core::{DateTime, Resolution, Symbol};

/// Request for historical data.
#[derive(Debug, Clone)]
pub struct HistoryRequest {
    pub symbol: Symbol,
    pub resolution: Resolution,
    pub start: DateTime,
    pub end: DateTime,
}

impl HistoryRequest {
    pub fn new(symbol: Symbol, resolution: Resolution, start: DateTime, end: DateTime) -> Self {
        HistoryRequest {
            symbol,
            resolution,
            start,
            end,
        }
    }
}
