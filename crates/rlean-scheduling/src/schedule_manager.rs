use crate::scheduled_event::{ScheduledCallback, ScheduledEvent};
use crate::{DateRule, TimeRule};
use parking_lot::Mutex;
use rlean_core::{DateTime, MarketHoursDatabase, TimeSpan};

pub struct DueScheduledEvent {
    pub name: String,
    pub trigger_time: DateTime,
    pub callback: ScheduledCallback,
}

pub struct ScheduleManager {
    events: Mutex<Vec<ScheduledEvent>>,
    frontier: Mutex<DateTime>,
}

impl ScheduleManager {
    pub fn new() -> Self {
        ScheduleManager {
            events: Mutex::new(Vec::new()),
            frontier: Mutex::new(DateTime::EPOCH),
        }
    }

    pub fn add(
        &self,
        name: impl Into<String>,
        date_rule: DateRule,
        time_rule: TimeRule,
        callback: impl FnMut() -> Result<(), String> + Send + 'static,
    ) {
        let frontier = *self.frontier.lock();
        self.events.lock().push(ScheduledEvent::new(
            name, date_rule, time_rule, callback, frontier,
        ));
    }

    /// Fast-forward registered events without invoking callbacks. Live setup
    /// uses this to avoid replaying an event whose scheduled time passed before
    /// the deployment came online; backtests prime at their start frontier.
    pub fn skip_until(&self, utc_time: DateTime) {
        *self.frontier.lock() = utc_time;
        let mut events = self.events.lock();
        for event in events.iter_mut() {
            event.last_evaluated = utc_time;
        }
    }

    /// Return all callbacks due through `utc_time`, in deterministic trigger
    /// and registration order. Callbacks are invoked by the engine after this
    /// lock is released so a callback may safely register another event.
    pub fn due_events(
        &self,
        utc_time: DateTime,
        market_hours_database: &MarketHoursDatabase,
    ) -> Vec<DueScheduledEvent> {
        let mut due = Vec::new();
        let mut events = self.events.lock();
        for (sequence, event) in events.iter_mut().enumerate() {
            if !event.enabled || utc_time <= event.last_evaluated {
                continue;
            }
            for trigger_time in trigger_times(
                &event.date_rule,
                &event.time_rule,
                event.last_evaluated,
                utc_time,
                market_hours_database,
            ) {
                due.push((
                    trigger_time,
                    sequence,
                    DueScheduledEvent {
                        name: event.name.clone(),
                        trigger_time,
                        callback: event.callback.clone(),
                    },
                ));
            }
            event.last_evaluated = utc_time;
        }
        drop(events);
        *self.frontier.lock() = utc_time;
        due.sort_by_key(|(time, sequence, _)| (*time, *sequence));
        due.into_iter().map(|(_, _, event)| event).collect()
    }

    pub fn remove(&self, name: &str) {
        self.events.lock().retain(|e| e.name != name);
    }
}

fn trigger_times(
    date_rule: &DateRule,
    time_rule: &TimeRule,
    after: DateTime,
    through: DateTime,
    market_hours_database: &MarketHoursDatabase,
) -> Vec<DateTime> {
    let mut result = Vec::new();
    let Some(mut date) = after.date_utc().pred_opt() else {
        return result;
    };
    let Some(last_date) = through.date_utc().succ_opt() else {
        return result;
    };
    while date <= last_date {
        if date_rule.applies_on(date) {
            if let Some(trigger) = trigger_time(time_rule, date, market_hours_database) {
                if trigger > after && trigger <= through {
                    result.push(trigger);
                }
            }
        }
        let Some(next) = date.succ_opt() else {
            break;
        };
        date = next;
    }
    result
}

fn trigger_time(
    time_rule: &TimeRule,
    date: chrono::NaiveDate,
    market_hours_database: &MarketHoursDatabase,
) -> Option<DateTime> {
    match time_rule {
        TimeRule::At { hour, minute } => date.and_hms_opt(*hour, *minute, 0).map(DateTime::from),
        TimeRule::BeforeMarketClose {
            symbol,
            minutes_before_close,
            extended_market_close: _,
        } => market_hours_database
            .exchange_hours(symbol)
            .session_bounds(date)
            .map(|(_, close)| close - TimeSpan::from_mins(*minutes_before_close)),
        TimeRule::AfterMarketOpen { minutes_after_open } => {
            // The symbol-less overload is retained for scheduled-universe
            // compatibility. Algorithm events should use a symbol-specific
            // market rule such as BeforeMarketClose.
            date.and_hms_opt(9, 30, 0)
                .map(DateTime::from)
                .map(|open| open + TimeSpan::from_mins(*minutes_after_open))
        }
        TimeRule::Every(interval) if interval.nanos > 0 => {
            let midnight = DateTime::from(date.and_hms_opt(0, 0, 0)?);
            let elapsed = (midnight - DateTime::EPOCH).nanos;
            let slots = elapsed.div_euclid(interval.nanos);
            Some(DateTime::EPOCH + TimeSpan::from_nanos(slots * interval.nanos))
        }
        TimeRule::Every(_) | TimeRule::EveryResolution => None,
    }
}

impl Default for ScheduleManager {
    fn default() -> Self {
        ScheduleManager::new()
    }
}
