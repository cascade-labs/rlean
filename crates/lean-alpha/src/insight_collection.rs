use crate::insight::Insight;
use lean_core::{DateTime, Symbol};
use std::collections::HashMap;

/// Maintains a set of active insights, indexed by symbol.
pub struct InsightCollection {
    /// keyed by symbol.id.sid for O(1) lookup
    insights: HashMap<u64, Vec<Insight>>,
}

impl InsightCollection {
    pub fn new() -> Self {
        Self {
            insights: HashMap::new(),
        }
    }

    pub fn add(&mut self, insight: Insight) {
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

    pub fn remove_expired(&mut self, utc_now: DateTime) {
        for v in self.insights.values_mut() {
            v.retain(|i| i.is_active(utc_now));
        }
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
