use crate::insight::Insight;
use lean_core::{DateTime, Symbol};
use std::collections::HashMap;

/// Maintains a set of active insights, indexed by symbol.
#[derive(Debug, Clone)]
pub struct InsightCollection {
    /// keyed by symbol.id.sid for O(1) lookup
    insights: HashMap<u64, Vec<Insight>>,
    total_count: usize,
}

impl InsightCollection {
    pub fn new() -> Self {
        Self {
            insights: HashMap::new(),
            total_count: 0,
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

    pub fn remove_expired(&mut self, utc_now: DateTime) -> Vec<Insight> {
        let mut expired = Vec::new();
        for v in self.insights.values_mut() {
            let mut i = 0;
            while i < v.len() {
                if v[i].is_expired(utc_now) {
                    expired.push(v.remove(i));
                } else {
                    i += 1;
                }
            }
        }
        self.insights.retain(|_, v| !v.is_empty());
        expired
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

    pub fn clear_symbols(&mut self, symbols: &[Symbol]) -> Vec<Insight> {
        let mut removed = Vec::new();
        for symbol in symbols {
            if let Some(mut insights) = self.insights.remove(&symbol.id.sid) {
                removed.append(&mut insights);
            }
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
