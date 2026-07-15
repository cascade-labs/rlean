//! Engine-owned runner for LEAN-style point-in-time fundamental universes.
//!
//! A fundamental selector consumes one whole daily cross-section.  It shares
//! the membership state machine with custom universes, but has no per-ticker
//! custom-data routing and never exposes a partial Arrow batch to a strategy.

use rlean_algorithm::algorithm::SecurityChanges;
use rlean_algorithm::lifecycle::UniverseSelection;
use rlean_core::{Resolution, Symbol};
use rlean_data::FundamentalData;
use rlean_sdk::universe::ScheduledUniverseDescriptor;
use std::sync::{Arc, Mutex};

use crate::universe_selection::{UniverseDiff, UniverseSelectionState};

pub type FundamentalUniverseSelectFn = Arc<dyn Fn(&[FundamentalData]) -> Vec<Symbol> + Send + Sync>;
pub type FundamentalUniverseSelectorRegistry = Arc<Mutex<Vec<FundamentalUniverseSelectorSlot>>>;

pub struct FundamentalUniverseSelectorSlot {
    pub resolution: Resolution,
    pub state: UniverseSelectionState,
    pub select: FundamentalUniverseSelectFn,
}

pub fn register_fundamental_universe_selector(
    registry: &FundamentalUniverseSelectorRegistry,
    resolution: Resolution,
    descriptor: ScheduledUniverseDescriptor,
    select: FundamentalUniverseSelectFn,
) {
    registry
        .lock()
        .unwrap()
        .push(FundamentalUniverseSelectorSlot {
            resolution,
            state: UniverseSelectionState::new(descriptor),
            select,
        });
}

pub fn has_fundamental_universe_selectors(registry: &FundamentalUniverseSelectorRegistry) -> bool {
    !registry.lock().unwrap().is_empty()
}

pub fn fundamental_universe_resolution(
    registry: &FundamentalUniverseSelectorRegistry,
) -> Option<Resolution> {
    registry
        .lock()
        .unwrap()
        .first()
        .map(|selector| selector.resolution)
}

pub fn run_fundamental_universe_selections(
    registry: &FundamentalUniverseSelectorRegistry,
    utc_ns: i64,
    _resolution: Resolution,
    fundamentals: &[FundamentalData],
) -> Vec<UniverseSelection> {
    if fundamentals.is_empty() {
        return Vec::new();
    }

    let mut selectors = registry.lock().unwrap();
    let mut selections = Vec::new();
    for selector in selectors.iter_mut() {
        // The sidecar's `end_time` is the point-in-time availability barrier.
        if !selector
            .state
            .should_trigger_data(utc_ns, selector.resolution)
        {
            continue;
        }
        let symbols = (selector.select)(fundamentals);
        selector
            .state
            .mark_data_triggered(utc_ns, selector.resolution);
        let UniverseDiff { added, removed } = selector.state.diff(symbols, utc_ns);
        let changes = SecurityChanges { added, removed };
        if changes.has_changes() {
            selections.push(UniverseSelection {
                changes,
                resolution: selector.state.descriptor().settings.resolution,
            });
        }
    }
    selections
}
