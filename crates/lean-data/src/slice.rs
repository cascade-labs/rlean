use crate::symbol_changed::SymbolChangedEvent;
use crate::{
    CustomDataPoint, Delisting, Dividend, MarginInterestRate, OrderBook, PerpetualContext,
    QuoteBar, Split, Tick, TradeBar,
};
use lean_core::{DateTime, Symbol};
use crate::OptionChain;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

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
    pub symbol_changed_events: HashMap<u64, SymbolChangedEvent>,
    pub margin_interest_rates: HashMap<u64, MarginInterestRate>,
    pub perpetual_contexts: HashMap<u64, PerpetualContext>,
    pub order_books: HashMap<u64, OrderBook>,
    pub option_chains: HashMap<String, Arc<OptionChain>>,
    pub custom_data_by_sid: HashMap<u64, Vec<CustomDataPoint>>,
    pub custom_data_symbols: HashMap<u64, Symbol>,
    pub custom_data: HashMap<String, Vec<CustomDataPoint>>,
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
            symbol_changed_events: std::collections::HashMap::new(),
            margin_interest_rates: std::collections::HashMap::new(),
            perpetual_contexts: std::collections::HashMap::new(),
            order_books: std::collections::HashMap::new(),
            option_chains: std::collections::HashMap::new(),
            custom_data_by_sid: std::collections::HashMap::new(),
            custom_data_symbols: std::collections::HashMap::new(),
            custom_data: std::collections::HashMap::new(),
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

    pub fn add_symbol_changed(&mut self, ev: SymbolChangedEvent) {
        self.symbol_changed_events.insert(ev.symbol.id.sid, ev);
        self.has_data = true;
    }

    pub fn add_margin_interest_rate(&mut self, rate: MarginInterestRate) {
        self.margin_interest_rates.insert(rate.symbol.id.sid, rate);
        self.has_data = true;
    }

    pub fn add_perpetual_context(&mut self, context: PerpetualContext) {
        self.perpetual_contexts
            .insert(context.symbol.id.sid, context);
        self.has_data = true;
    }

    pub fn add_order_book(&mut self, book: OrderBook) {
        self.order_books.insert(book.symbol.id.sid, book);
        self.has_data = true;
    }

    pub fn add_option_chain(&mut self, canonical_permtick: String, chain: Arc<OptionChain>) {
        self.option_chains.insert(canonical_permtick, chain);
        self.has_data = true;
    }

    pub fn add_custom_data(&mut self, source_type: String, ticker: String, point: CustomDataPoint) {
        let _ = source_type;
        let key = ticker.to_ascii_uppercase();
        self.custom_data.entry(key).or_default().push(point);
        self.has_data = true;
    }

    pub fn add_custom_data_for_symbol(
        &mut self,
        symbol: Symbol,
        ticker: String,
        point: CustomDataPoint,
    ) {
        self.custom_data_symbols
            .entry(symbol.id.sid)
            .or_insert_with(|| symbol.clone());
        self.custom_data_by_sid
            .entry(symbol.id.sid)
            .or_default()
            .push(point.clone());
        self.custom_data
            .entry(ticker.to_ascii_uppercase())
            .or_default()
            .push(point);
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

    pub fn get_margin_interest_rate(&self, symbol: &Symbol) -> Option<&MarginInterestRate> {
        self.margin_interest_rates.get(&symbol.id.sid)
    }

    pub fn get_perpetual_context(&self, symbol: &Symbol) -> Option<&PerpetualContext> {
        self.perpetual_contexts.get(&symbol.id.sid)
    }

    pub fn get_order_book(&self, symbol: &Symbol) -> Option<&OrderBook> {
        self.order_books.get(&symbol.id.sid)
    }

    pub fn get_custom_data(&self, symbol: &Symbol) -> Option<&Vec<CustomDataPoint>> {
        self.custom_data_by_sid.get(&symbol.id.sid)
    }
}
