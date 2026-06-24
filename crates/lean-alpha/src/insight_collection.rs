use crate::insight::Insight;
use lean_core::{DateTime, Symbol};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Max number of closed (scored) insights retained for `get_insights` history.
/// Bounds memory while covering well beyond any realistic source-IC lookback.
const MAX_CLOSED_HISTORY: usize = 50_000;

/// Maintains a set of active insights, indexed by symbol.
#[derive(Debug, Clone)]
pub struct InsightCollection {
    /// keyed by symbol.id.sid; BTreeMap (not HashMap) so every iteration order is
    /// deterministic (sorted by sid) — HashMap's per-process RandomState reordered
    /// insight delivery to the PCM / framework scoring, causing run-to-run numeric
    /// drift in backtests.
    insights: BTreeMap<u64, Vec<Insight>>,
    /// Closed insights retained with their final scores (FIFO-pruned history).
    closed: Vec<Insight>,
    total_count: usize,
}

/// Stable serializable representation of an [`InsightCollection`].
///
/// The collection keeps its runtime index private so active-insight ordering
/// stays deterministic, while live runners can still persist and hydrate the
/// framework's alpha state across restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InsightCollectionSnapshot {
    pub active: Vec<Insight>,
    pub closed: Vec<Insight>,
    pub total_count: usize,
}

impl InsightCollection {
    pub fn new() -> Self {
        Self {
            insights: BTreeMap::new(),
            closed: Vec::new(),
            total_count: 0,
        }
    }

    /// Update direction/magnitude scores of all active insights from current prices.
    /// `prices` is keyed by `symbol.value` (ticker), matching the framework pipeline.
    pub fn score_active(&mut self, prices: &HashMap<String, Decimal>, utc_now: DateTime) {
        for v in self.insights.values_mut() {
            for insight in v.iter_mut() {
                if insight.is_active(utc_now) {
                    if let Some(price) = prices.get(&insight.symbol.value) {
                        if insight.reference_value.is_none() {
                            insight.reference_value = Some(*price);
                        }
                        insight.update_score(*price);
                    }
                }
            }
        }
    }

    /// All insights (active + retained closed history), for `GetInsights`.
    pub fn get_insights(&self) -> Vec<Insight> {
        let mut out: Vec<Insight> = self.insights.values().flatten().cloned().collect();
        out.extend(self.closed.iter().cloned());
        out
    }

    pub fn closed_insights(&self) -> Vec<Insight> {
        self.closed.clone()
    }

    pub fn snapshot(&self) -> InsightCollectionSnapshot {
        InsightCollectionSnapshot {
            active: self.insights.values().flatten().cloned().collect(),
            closed: self.closed.clone(),
            total_count: self.total_count,
        }
    }

    pub fn from_snapshot(snapshot: InsightCollectionSnapshot) -> Self {
        let mut insights: BTreeMap<u64, Vec<Insight>> = BTreeMap::new();
        for insight in snapshot.active {
            insights
                .entry(insight.symbol.id.sid)
                .or_default()
                .push(insight);
        }
        let mut closed = snapshot.closed;
        if closed.len() > MAX_CLOSED_HISTORY {
            let overflow = closed.len() - MAX_CLOSED_HISTORY;
            closed.drain(0..overflow);
        }
        Self {
            insights,
            closed,
            total_count: snapshot.total_count,
        }
    }

    pub fn add(&mut self, insight: Insight) {
        self.total_count += 1;
        self.insights
            .entry(insight.symbol.id.sid)
            .or_default()
            .push(insight);
    }

    pub fn add_range(&mut self, insights: Vec<Insight>) {
        for i in insights {
            self.add(i);
        }
    }

    pub fn get_active(&self, utc_now: DateTime) -> Vec<&Insight> {
        self.insights
            .values()
            .flatten()
            .filter(|i| i.is_active(utc_now))
            .collect()
    }

    pub fn active_insights(&self, utc_now: DateTime) -> Vec<Insight> {
        self.get_active(utc_now).into_iter().cloned().collect()
    }

    pub fn latest_active_per_symbol(&self, utc_now: DateTime) -> Vec<Insight> {
        self.insights
            .values()
            .filter_map(|symbol_insights| {
                symbol_insights
                    .iter()
                    .filter(|i| i.is_active(utc_now))
                    .max_by_key(|i| i.generated_time_utc.0)
                    .cloned()
            })
            .collect()
    }

    pub fn active(&self, utc_now: DateTime) -> Vec<Insight> {
        self.insights
            .values()
            .flat_map(|symbol_insights| {
                symbol_insights
                    .iter()
                    .filter(move |i| i.is_active(utc_now))
                    .cloned()
            })
            .collect()
    }

    pub fn remove_expired(&mut self, utc_now: DateTime) -> Vec<Insight> {
        let mut expired = Vec::new();
        for v in self.insights.values_mut() {
            let mut i = 0;
            while i < v.len() {
                if v[i].is_expired(utc_now) {
                    let mut closed = v.remove(i);
                    closed.finalize_score();
                    self.closed.push(closed.clone());
                    expired.push(closed);
                } else {
                    i += 1;
                }
            }
        }
        self.insights.retain(|_, v| !v.is_empty());
        // Bound retained history (FIFO).
        if self.closed.len() > MAX_CLOSED_HISTORY {
            let overflow = self.closed.len() - MAX_CLOSED_HISTORY;
            self.closed.drain(0..overflow);
        }
        expired
    }

    /// Close (finalize + retain in scored history) and remove ONLY the active insights for
    /// `symbol` emitted by `source_model`. Used for Flat insights: one alpha's Flat must not
    /// destroy another alpha's still-active insight on the same symbol, and the closed insight
    /// must be scored (mirrors `remove_expired`'s finalize+retain) so per-alpha IC is a property
    /// of that alpha alone — independent of which other alphas (or PCM) are running.
    pub fn close_source_symbol(
        &mut self,
        symbol: &Symbol,
        source_model: &str,
        utc_now: DateTime,
    ) -> Vec<Insight> {
        let mut closed_out = Vec::new();
        if let Some(v) = self.insights.get_mut(&symbol.id.sid) {
            let mut i = 0;
            while i < v.len() {
                if v[i].source_model == source_model {
                    let mut closed = v.remove(i);
                    if closed.close_time_utc > utc_now {
                        closed.close_time_utc = utc_now;
                    }
                    closed.finalize_score();
                    self.closed.push(closed.clone());
                    closed_out.push(closed);
                } else {
                    i += 1;
                }
            }
        }
        self.insights.retain(|_, v| !v.is_empty());
        if self.closed.len() > MAX_CLOSED_HISTORY {
            let overflow = self.closed.len() - MAX_CLOSED_HISTORY;
            self.closed.drain(0..overflow);
        }
        closed_out
    }

    pub fn has_active(&self, symbol: &Symbol, utc_now: DateTime) -> bool {
        self.insights
            .get(&symbol.id.sid)
            .map(|v| v.iter().any(|i| i.is_active(utc_now)))
            .unwrap_or(false)
    }

    pub fn expire(&mut self, symbols: &[Symbol], utc_now: DateTime) -> Vec<Insight> {
        let mut expired = Vec::new();
        for symbol in symbols {
            if let Some(symbol_insights) = self.insights.get_mut(&symbol.id.sid) {
                for insight in symbol_insights.iter_mut() {
                    if insight.close_time_utc > utc_now {
                        insight.close_time_utc = utc_now;
                    }
                    expired.push(insight.clone());
                }
            }
        }
        expired
    }

    /// Remove all insights for the given symbols (e.g. universe removal, risk cancellation),
    /// finalizing each and retaining it in the scored-history `closed` set so it is still
    /// available for analytics — same finalize+retain contract as `remove_expired`, so insights
    /// cut short by a security leaving the universe are not silently dropped from per-alpha IC.
    /// The caller is responsible for passing the returned insights to the analytics tracker.
    pub fn clear_symbols(&mut self, symbols: &[Symbol]) -> Vec<Insight> {
        let mut removed = Vec::new();
        for symbol in symbols {
            if let Some(mut insights) = self.insights.remove(&symbol.id.sid) {
                for insight in insights.iter_mut() {
                    insight.finalize_score();
                    self.closed.push(insight.clone());
                }
                removed.append(&mut insights);
            }
        }
        if self.closed.len() > MAX_CLOSED_HISTORY {
            let overflow = self.closed.len() - MAX_CLOSED_HISTORY;
            self.closed.drain(0..overflow);
        }
        removed
    }

    pub fn for_symbol(&self, symbol: &Symbol) -> Vec<&Insight> {
        self.insights
            .get(&symbol.id.sid)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    }

    pub fn latest_for_symbol(&self, symbol: &Symbol) -> Option<&Insight> {
        self.insights.get(&symbol.id.sid)?.last()
    }

    pub fn len(&self) -> usize {
        self.insights.values().map(|v| v.len()).sum()
    }

    pub fn total_count(&self) -> usize {
        self.total_count
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&mut self) {
        self.insights.clear();
    }
}

impl Default for InsightCollection {
    fn default() -> Self {
        Self::new()
    }
}
