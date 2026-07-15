//! Engine-owned custom-universe selection runner.
//!
//! Language bindings register a symbol-selection callback; this module owns
//! trigger cadence, membership diffing, and selection orchestration.

use rlean_algorithm::algorithm::SecurityChanges;
use rlean_algorithm::lifecycle::UniverseSelection;
use rlean_core::{Resolution, Symbol};
use rlean_data_tables::CustomDataPoint;
use rlean_sdk::universe::ScheduledUniverseDescriptor;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::universe_selection::{UniverseDiff, UniverseSelectionState};

pub type CustomUniverseSelectFn = Arc<dyn Fn(&[CustomDataPoint]) -> Vec<Symbol> + Send + Sync>;

pub type CustomUniverseSelectorRegistry = Arc<Mutex<Vec<CustomUniverseSelectorSlot>>>;

pub struct CustomUniverseSelectorSlot {
    pub ticker: String,
    pub resolution: Resolution,
    pub state: UniverseSelectionState,
    pub select: CustomUniverseSelectFn,
}

pub fn register_custom_universe_selector(
    registry: &CustomUniverseSelectorRegistry,
    ticker: String,
    resolution: Resolution,
    descriptor: ScheduledUniverseDescriptor,
    select: CustomUniverseSelectFn,
) {
    registry.lock().unwrap().push(CustomUniverseSelectorSlot {
        ticker,
        resolution,
        state: UniverseSelectionState::new(descriptor),
        select,
    });
}

pub fn has_custom_universe_selectors(registry: &CustomUniverseSelectorRegistry) -> bool {
    !registry.lock().unwrap().is_empty()
}

pub fn custom_universe_resolution(registry: &CustomUniverseSelectorRegistry) -> Option<Resolution> {
    registry
        .lock()
        .unwrap()
        .first()
        .map(|selector| selector.resolution)
}

pub fn run_custom_universe_selections(
    registry: &CustomUniverseSelectorRegistry,
    utc_ns: i64,
    _resolution: Resolution,
    custom_data: &HashMap<String, Vec<CustomDataPoint>>,
) -> Vec<UniverseSelection> {
    let mut selectors = registry.lock().unwrap();
    let mut selections = Vec::new();

    for selector in selectors.iter_mut() {
        if !selector
            .state
            .should_trigger_custom_data(utc_ns, selector.resolution)
        {
            continue;
        }
        let Some(points) = custom_data.get(&selector.ticker) else {
            continue;
        };
        if points.is_empty() {
            continue;
        }

        let symbols = (selector.select)(points);
        selector
            .state
            .mark_custom_data_triggered(utc_ns, selector.resolution);
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
