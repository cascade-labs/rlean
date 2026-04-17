use crate::{Delisting, Dividend, QuoteBar, Split, Tick, TradeBar};
use lean_core::{DateTime, Symbol};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A time-slice of market data across all subscribed symbols.
/// Mirrors LEAN's `Slice` — the object delivered to `OnData()`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Slice {
    pub time: DateTime,
    pub bars: HashMap<u64, TradeBar>, // keyed by symbol.id.sid
    pub quote_bars: HashMap<u64, QuoteBar>,
    pub ticks: HashMap<u64, Vec<Tick>>,
    pub dividends: HashMap<u64, Dividend>,
    pub splits: HashMap<u64, Split>,
    pub delistings: HashMap<u64, Delisting>,
    pub has_data: bool,
}

impl Slice {
    pub fn new(time: DateTime) -> Self {
        Slice {
            time,
            bars: std::collections::HashMap::new(),
            quote_bars: std::collections::HashMap::new(),
            ticks: std::collections::HashMap::new(),
            dividends: std::collections::HashMap::new(),
            splits: std::collections::HashMap::new(),
            delistings: std::collections::HashMap::new(),
            has_data: false,
        }
    }

    pub fn add_bar(&mut self, bar: TradeBar) {
        self.bars.insert(bar.symbol.id.sid, bar);
        self.has_data = true;
    }

    pub fn add_quote_bar(&mut self, bar: QuoteBar) {
        self.quote_bars.insert(bar.symbol.id.sid, bar);
        self.has_data = true;
    }

    pub fn add_tick(&mut self, tick: Tick) {
        self.ticks.entry(tick.symbol.id.sid).or_default().push(tick);
        self.has_data = true;
    }

    pub fn add_dividend(&mut self, div: Dividend) {
        self.dividends.insert(div.symbol.id.sid, div);
        self.has_data = true;
    }

    pub fn add_split(&mut self, split: Split) {
        self.splits.insert(split.symbol.id.sid, split);
        self.has_data = true;
    }

    pub fn add_delisting(&mut self, d: Delisting) {
        self.delistings.insert(d.symbol.id.sid, d);
        self.has_data = true;
    }

    pub fn get_bar(&self, symbol: &Symbol) -> Option<&TradeBar> {
        self.bars.get(&symbol.id.sid)
    }

    pub fn get_quote_bar(&self, symbol: &Symbol) -> Option<&QuoteBar> {
        self.quote_bars.get(&symbol.id.sid)
    }

    pub fn get_ticks(&self, symbol: &Symbol) -> Option<&Vec<Tick>> {
        self.ticks.get(&symbol.id.sid)
    }
}
