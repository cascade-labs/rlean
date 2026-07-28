use rlean_core::{DateTime, MarketHoursDatabase, NanosecondTimestamp, SecurityType, TickType};
use rlean_data::{LiveDataItem, Slice, SubscriptionDataConfig, SubscriptionDataKind};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FillForwardKey {
    sid: u64,
    tick_type: TickType,
}

/// Synchronizes provider events against a monotonic live-time frontier.
///
/// Provider timestamps describe when data occurred. They do not control the
/// algorithm clock: late data is delivered at the current frontier and future
/// data waits until the frontier reaches it, matching LEAN's live synchronizer.
#[derive(Debug, Default)]
pub struct LiveSliceAssembler {
    pending: Vec<LiveDataItem>,
    last_frontier: Option<DateTime>,
    fill_forward_subscriptions: HashMap<u64, SubscriptionDataConfig>,
    last_market_data: HashMap<FillForwardKey, LiveDataItem>,
}

impl LiveSliceAssembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds provider data without advancing algorithm time.
    pub fn enqueue(&mut self, item: LiveDataItem) {
        self.pending.push(item);
    }

    /// Synchronizes the active market subscriptions used for live fill-forward.
    ///
    /// C# LEAN applies `FillForwardEnumerator` to each subscription before the
    /// time-slice synchronizer. Live provider streams are asynchronous, so the
    /// equivalent here is to retain the last completed bar for every active
    /// fill-forward subscription and add its current clone to a slice driven by
    /// another subscription.
    pub fn set_subscriptions<'a>(
        &mut self,
        subscriptions: impl IntoIterator<Item = &'a SubscriptionDataConfig>,
    ) {
        let subscriptions: Vec<_> = subscriptions
            .into_iter()
            .filter(|config| {
                config.data_kind == SubscriptionDataKind::Market
                    && config.fill_data_forward
                    && !config.resolution.is_tick()
                    && matches!(config.tick_type, TickType::Trade | TickType::Quote)
            })
            .collect();
        let desired_ids: HashSet<_> = subscriptions
            .iter()
            .map(|config| config.unique_id())
            .collect();
        if desired_ids.len() == self.fill_forward_subscriptions.len()
            && desired_ids
                .iter()
                .all(|id| self.fill_forward_subscriptions.contains_key(id))
        {
            return;
        }

        let next: HashMap<_, _> = subscriptions
            .into_iter()
            .map(|config| (config.unique_id(), config.clone()))
            .collect();

        self.last_market_data
            .retain(|key, _| next.values().any(|config| fill_forward_key(config) == *key));
        self.fill_forward_subscriptions = next;
    }

    /// Advances live time and emits all data whose availability time is at or
    /// behind that frontier. A caller-supplied clock regression is clamped to
    /// the last frontier, so this type can never emit a backward `Slice`.
    pub fn advance(&mut self, frontier: DateTime) -> Option<Slice> {
        let frontier = self
            .last_frontier
            .map_or(frontier, |last| frontier.max(last));
        self.last_frontier = Some(frontier);

        let mut due = Vec::new();
        let mut future = Vec::new();
        for item in self.pending.drain(..) {
            if item.end_time() <= frontier {
                due.push(item);
            } else {
                future.push(item);
            }
        }
        self.pending = future;

        if due.is_empty() {
            return None;
        }

        let mut slice = Slice::new(frontier);
        for item in due {
            self.remember_market_data(&item);
            item.add_to_slice(&mut slice);
        }
        if !slice.has_data {
            return None;
        }

        self.add_fill_forward_data(&mut slice, frontier);
        Some(slice)
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    fn remember_market_data(&mut self, item: &LiveDataItem) {
        let key = match item {
            LiveDataItem::TradeBar(bar) => FillForwardKey {
                sid: bar.symbol.id.sid,
                tick_type: TickType::Trade,
            },
            LiveDataItem::QuoteBar(bar) => FillForwardKey {
                sid: bar.symbol.id.sid,
                tick_type: TickType::Quote,
            },
            _ => return,
        };
        if !self
            .fill_forward_subscriptions
            .values()
            .any(|config| fill_forward_key(config) == key)
        {
            return;
        }

        let replace = self
            .last_market_data
            .get(&key)
            .is_none_or(|previous| item.end_time() >= previous.end_time());
        if replace {
            self.last_market_data.insert(key, item.clone());
        }
    }

    fn add_fill_forward_data(&self, slice: &mut Slice, frontier: DateTime) {
        for config in self.fill_forward_subscriptions.values() {
            let sid = config.symbol.id.sid;
            let already_present = match config.tick_type {
                TickType::Trade => slice.bars.contains_key(&sid),
                TickType::Quote => slice.quote_bars.contains_key(&sid),
                TickType::OpenInterest => true,
            };
            if already_present {
                continue;
            }

            let key = fill_forward_key(config);
            let Some(previous) = self.last_market_data.get(&key) else {
                continue;
            };
            let Some(fill) = fill_forward_item(config, previous, frontier) else {
                continue;
            };
            fill.add_to_slice(slice);
        }
    }
}

fn fill_forward_key(config: &SubscriptionDataConfig) -> FillForwardKey {
    FillForwardKey {
        sid: config.symbol.id.sid,
        tick_type: config.tick_type,
    }
}

fn fill_forward_item(
    config: &SubscriptionDataConfig,
    previous: &LiveDataItem,
    frontier: DateTime,
) -> Option<LiveDataItem> {
    let period = config.resolution.to_time_span()?;
    let previous_frontier = previous.end_time();
    if frontier <= previous_frontier {
        return None;
    }
    let steps = (frontier.0 - previous_frontier.0) / period.nanos;
    if steps == 0 {
        return None;
    }
    let fill_frontier = NanosecondTimestamp(previous_frontier.0 + steps * period.nanos);
    if config.symbol.security_type() == SecurityType::Equity
        && !MarketHoursDatabase::global()
            .exchange_hours(&config.symbol)
            .is_open_at(fill_frontier - period)
    {
        return None;
    }

    match previous {
        LiveDataItem::TradeBar(bar) => {
            let mut fill = bar.clone();
            fill.time = fill_frontier - period;
            fill.end_time = fill_frontier;
            fill.open = fill.close;
            fill.high = fill.close;
            fill.low = fill.close;
            fill.volume = Decimal::ZERO;
            fill.period = period;
            Some(LiveDataItem::TradeBar(fill))
        }
        LiveDataItem::QuoteBar(bar) => {
            let mut fill = bar.clone();
            fill.time = fill_frontier - period;
            fill.end_time = fill_frontier;
            fill.period = period;
            Some(LiveDataItem::QuoteBar(fill))
        }
        _ => None,
    }
}
