use lean_core::DateTime;
use lean_data::{QuoteBar, Slice, Tick, TradeBar};

/// A single data point emitted by a subscription stream.
#[derive(Debug, Clone)]
pub enum SubscriptionDataPoint {
    TradeBar(TradeBar),
    QuoteBar(QuoteBar),
    Tick(Tick),
}

impl SubscriptionDataPoint {
    /// Frontier time used for synchronizing slices across subscriptions.
    /// Bars use `end_time`; ticks use `time`.
    pub fn frontier_time(&self) -> DateTime {
        match self {
            SubscriptionDataPoint::TradeBar(bar) => bar.end_time,
            SubscriptionDataPoint::QuoteBar(bar) => bar.end_time,
            SubscriptionDataPoint::Tick(tick) => tick.time,
        }
    }

    pub fn add_to_slice(&self, slice: &mut Slice) {
        match self {
            SubscriptionDataPoint::TradeBar(bar) => slice.add_bar(bar.clone()),
            SubscriptionDataPoint::QuoteBar(bar) => slice.add_quote_bar(bar.clone()),
            SubscriptionDataPoint::Tick(tick) => slice.add_tick(tick.clone()),
        }
    }
}
