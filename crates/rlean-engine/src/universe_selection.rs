// Engine-owned universe-selection state machine.
//
// Holds the membership-diff logic (added/removed with minimum-time-in-universe
// gating) and trigger-cadence helpers that were previously inlined in the
// Python universe binding. Language bindings only invoke the user selector and
// pass the resulting symbol set into [`UniverseMembership::diff`].

use rlean_core::{Resolution, Symbol};
use rlean_sdk::universe::{DateRule, ScheduledUniverseDescriptor, TimeRule};
use std::collections::HashMap;

#[derive(Clone)]
struct UniverseMember {
    symbol: Symbol,
    added_ns: i64,
}

/// Diff result: symbols entering and leaving the universe this selection.
pub struct UniverseDiff {
    pub added: Vec<Symbol>,
    pub removed: Vec<Symbol>,
}

/// Engine-owned runtime state for a scheduled/custom universe selector.
pub struct UniverseSelectionState {
    descriptor: ScheduledUniverseDescriptor,
    membership: UniverseMembership,
    last_custom_trigger: Option<i64>,
}

impl UniverseSelectionState {
    pub fn new(descriptor: ScheduledUniverseDescriptor) -> Self {
        Self {
            descriptor,
            membership: UniverseMembership::new(),
            last_custom_trigger: None,
        }
    }

    pub fn descriptor(&self) -> &ScheduledUniverseDescriptor {
        &self.descriptor
    }

    pub fn descriptor_mut(&mut self) -> &mut ScheduledUniverseDescriptor {
        &mut self.descriptor
    }

    pub fn should_trigger(&self, utc_ns: i64, resolution: Resolution) -> bool {
        if self.descriptor.is_custom_data() {
            return false;
        }
        should_trigger_scheduled(&self.descriptor, utc_ns, resolution)
    }

    pub fn should_trigger_custom_data(&self, utc_ns: i64, resolution: Resolution) -> bool {
        self.descriptor.is_custom_data() && self.should_trigger_data(utc_ns, resolution)
    }

    pub fn mark_custom_data_triggered(&mut self, utc_ns: i64, resolution: Resolution) {
        self.mark_data_triggered(utc_ns, resolution);
    }

    /// Data-driven selector cadence shared by custom and fundamental universe
    /// feeds. The producer's availability frontier is the timing authority.
    pub fn should_trigger_data(&self, utc_ns: i64, resolution: Resolution) -> bool {
        self.descriptor.trigger_resolution == resolution
            && self.last_custom_trigger != Some(custom_trigger_key(utc_ns, resolution))
    }

    pub fn mark_data_triggered(&mut self, utc_ns: i64, resolution: Resolution) {
        self.last_custom_trigger = Some(custom_trigger_key(utc_ns, resolution));
    }

    pub fn diff(&mut self, symbols: Vec<Symbol>, utc_ns: i64) -> UniverseDiff {
        self.membership.diff(
            symbols,
            utc_ns,
            self.descriptor.settings.minimum_time_in_universe_secs,
        )
    }
}

/// Tracks the current universe membership and computes add/remove diffs against
/// each new selection, honouring `minimum_time_in_universe` for removals.
#[derive(Default)]
pub struct UniverseMembership {
    selected: HashMap<u64, UniverseMember>,
}

impl UniverseMembership {
    pub fn new() -> Self {
        Self {
            selected: HashMap::new(),
        }
    }

    /// Compute the membership diff for `symbols` selected at `utc_ns` and update
    /// internal state. `minimum_secs` is the minimum time a symbol must remain
    /// in the universe before it can be removed.
    pub fn diff(&mut self, symbols: Vec<Symbol>, utc_ns: i64, minimum_secs: f64) -> UniverseDiff {
        let mut next = HashMap::new();
        for symbol in symbols {
            next.insert(symbol.id.sid, symbol);
        }

        let mut added = Vec::new();
        for (sid, symbol) in &next {
            if !self.selected.contains_key(sid) {
                added.push(symbol.clone());
            }
        }

        let mut removed = Vec::new();
        self.selected.retain(|sid, member| {
            if next.contains_key(sid) {
                true
            } else if can_remove_member(member.added_ns, utc_ns, minimum_secs) {
                removed.push(member.symbol.clone());
                false
            } else {
                true
            }
        });

        for symbol in added.iter().cloned() {
            self.selected.insert(
                symbol.id.sid,
                UniverseMember {
                    symbol,
                    added_ns: utc_ns,
                },
            );
        }

        UniverseDiff { added, removed }
    }
}

/// Whether a universe member added at `added_ns` may be removed at `utc_ns`,
/// given the configured `minimum_secs` minimum-time-in-universe.
pub fn can_remove_member(added_ns: i64, utc_ns: i64, minimum_secs: f64) -> bool {
    if minimum_secs <= 0.0 {
        return true;
    }
    let elapsed_secs = (utc_ns - added_ns) as f64 / 1_000_000_000.0;
    elapsed_secs + rounding_epsilon(minimum_secs) >= minimum_secs
}

fn rounding_epsilon(minimum_secs: f64) -> f64 {
    if minimum_secs >= 86_400.0 {
        43_199.0
    } else if minimum_secs >= 3_600.0 {
        1_800.0
    } else if minimum_secs >= 60.0 {
        30.0
    } else {
        0.5
    }
}

fn ns_to_utc_datetime(ns: i64) -> chrono::DateTime<chrono::Utc> {
    let secs = ns / 1_000_000_000;
    let nanos = (ns % 1_000_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos).unwrap_or_default()
}

pub fn should_trigger_scheduled(
    descriptor: &ScheduledUniverseDescriptor,
    utc_ns: i64,
    resolution: Resolution,
) -> bool {
    let dt = ns_to_utc_datetime(utc_ns);
    match descriptor.date_rule {
        DateRule::EveryDay => {}
    }
    match descriptor.time_rule {
        TimeRule::EveryResolution => resolution == descriptor.trigger_resolution,
        TimeRule::At { hour, minute } => dt.hour() == hour && dt.minute() == minute,
        TimeRule::AfterMarketOpen { minutes_after_open } => {
            let open = market_open_time(minutes_after_open);
            dt.time().hour() == open.hour() && dt.time().minute() == open.minute()
        }
    }
}

pub fn trigger_times(
    descriptor: &ScheduledUniverseDescriptor,
    start_ns: i64,
    end_ns: i64,
) -> Vec<i64> {
    let mut out = Vec::new();
    let mut date = ns_to_utc_datetime(start_ns).date_naive();
    let end_dt = ns_to_utc_datetime(end_ns);
    while date <= end_dt.date_naive() {
        let Some(trigger) = trigger_time_for_date(date, descriptor.time_rule) else {
            date += chrono::Duration::days(1);
            continue;
        };
        let ns = trigger.and_utc().timestamp_nanos_opt().unwrap_or(0);
        if ns > start_ns && ns < end_ns {
            out.push(ns);
        }
        date += chrono::Duration::days(1);
    }
    out
}

fn trigger_time_for_date(date: chrono::NaiveDate, rule: TimeRule) -> Option<chrono::NaiveDateTime> {
    match rule {
        TimeRule::At { hour, minute } => date.and_hms_opt(hour, minute, 0),
        TimeRule::AfterMarketOpen { minutes_after_open } => Some(chrono::NaiveDateTime::new(
            date,
            market_open_time(minutes_after_open),
        )),
        TimeRule::EveryResolution => date.and_hms_opt(0, 0, 0),
    }
}

fn market_open_time(minutes_after_open: i64) -> chrono::NaiveTime {
    chrono::NaiveTime::from_hms_opt(14, 30, 0).unwrap()
        + chrono::Duration::minutes(minutes_after_open)
}

/// Resolution-bucketed trigger key used to fire a data-driven custom universe at
/// most once per resolution period.
pub fn custom_trigger_key(ns: i64, resolution: Resolution) -> i64 {
    match resolution {
        Resolution::Daily => ns_to_utc_datetime(ns)
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap_or(ns),
        _ => ns,
    }
}

trait DateTimeParts {
    fn hour(&self) -> u32;
    fn minute(&self) -> u32;
}

impl DateTimeParts for chrono::DateTime<chrono::Utc> {
    fn hour(&self) -> u32 {
        chrono::Timelike::hour(self)
    }

    fn minute(&self) -> u32 {
        chrono::Timelike::minute(self)
    }
}

trait NaiveTimeParts {
    fn hour(&self) -> u32;
    fn minute(&self) -> u32;
}

impl NaiveTimeParts for chrono::NaiveTime {
    fn hour(&self) -> u32 {
        chrono::Timelike::hour(self)
    }

    fn minute(&self) -> u32 {
        chrono::Timelike::minute(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::{Market, SecurityType};
    use rlean_sdk::universe::UniverseSettings;

    fn dt_ns(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .unwrap()
            .and_hms_opt(hour, minute, 0)
            .unwrap()
            .and_utc()
            .timestamp_nanos_opt()
            .unwrap()
    }

    #[test]
    fn trigger_times_excludes_start_and_end_boundaries() {
        let descriptor = ScheduledUniverseDescriptor::scheduled(
            DateRule::EveryDay,
            TimeRule::At {
                hour: 12,
                minute: 0,
            },
            UniverseSettings::default(),
        );

        let triggers = trigger_times(
            &descriptor,
            dt_ns(2000, 1, 5, 15, 0),
            dt_ns(2000, 1, 10, 0, 0),
        );

        assert_eq!(
            triggers,
            vec![
                dt_ns(2000, 1, 6, 12, 0),
                dt_ns(2000, 1, 7, 12, 0),
                dt_ns(2000, 1, 8, 12, 0),
                dt_ns(2000, 1, 9, 12, 0),
            ]
        );
    }

    #[test]
    fn custom_data_state_triggers_once_per_daily_bucket() {
        let descriptor = ScheduledUniverseDescriptor::custom_data(
            "fixture".to_string(),
            "snapshot".to_string(),
            Resolution::Daily,
            UniverseSettings::default(),
            SecurityType::Equity,
            Market::usa(),
        );
        let mut state = UniverseSelectionState::new(descriptor);
        let morning = dt_ns(2026, 1, 1, 9, 0);
        let afternoon = dt_ns(2026, 1, 1, 15, 0);
        let next_day = dt_ns(2026, 1, 2, 9, 0);

        assert!(state.should_trigger_custom_data(morning, Resolution::Daily));
        state.mark_custom_data_triggered(morning, Resolution::Daily);
        assert!(!state.should_trigger_custom_data(afternoon, Resolution::Daily));
        assert!(state.should_trigger_custom_data(next_day, Resolution::Daily));
    }

    #[test]
    fn minimum_time_in_universe_uses_lean_rounding_thresholds() {
        let added = dt_ns(2018, 1, 1, 0, 0);

        assert!(!can_remove_member(added, added + 29_000_000_000, 30.0));
        assert!(can_remove_member(added, added + 29_500_000_000, 30.0));

        assert!(!can_remove_member(
            added,
            added + 29 * 60 * 1_000_000_000,
            30.0 * 60.0
        ));
        assert!(can_remove_member(
            added,
            added + (29 * 60 + 30) * 1_000_000_000,
            30.0 * 60.0
        ));

        assert!(!can_remove_member(
            added,
            added + 12 * 60 * 60 * 1_000_000_000,
            86_400.0
        ));
        assert!(can_remove_member(
            added,
            added + (12 * 60 * 60 + 1) * 1_000_000_000,
            86_400.0
        ));
    }
}
