use lean_core::DateTime;
use lean_core::Symbol;
use lean_data::{CustomDataPoint, QuoteBar, Slice, Tick, TradeBar};
use lean_options::OptionChain;
use std::sync::Arc;

/// A single data point emitted by a subscription stream.
#[derive(Debug, Clone)]
pub enum SubscriptionDataPoint {
    TradeBar(TradeBar),
    QuoteBar(QuoteBar),
    Tick(Tick),
    CustomData {
        symbol: Symbol,
        ticker: String,
        point: CustomDataPoint,
    },
    OptionChain {
        canonical_permtick: String,
        chain: Arc<OptionChain>,
        frontier_time: DateTime,
    },
}

impl SubscriptionDataPoint {
    /// Frontier time used for synchronizing slices across subscriptions.
    /// Bars use `end_time`; ticks use `time`.
    pub fn frontier_time(&self) -> DateTime {
        match self {
            SubscriptionDataPoint::TradeBar(bar) => bar.end_time,
            SubscriptionDataPoint::QuoteBar(bar) => bar.end_time,
            SubscriptionDataPoint::Tick(tick) => tick.time,
            SubscriptionDataPoint::OptionChain { frontier_time, .. } => *frontier_time,
            SubscriptionDataPoint::CustomData { point, .. } => {
                point.end_time.unwrap_or_else(|| {
                    point
                        .time
                        .and_hms_opt(0, 0, 0)
                        .expect("midnight is valid")
                        .into()
                })
            }
        }
    }

    pub fn add_to_slice(&self, slice: &mut Slice) {
        match self {
            SubscriptionDataPoint::TradeBar(bar) => slice.add_bar(bar.clone()),
            SubscriptionDataPoint::QuoteBar(bar) => slice.add_quote_bar(bar.clone()),
            SubscriptionDataPoint::Tick(tick) => slice.add_tick(tick.clone()),
            SubscriptionDataPoint::CustomData {
                symbol,
                ticker,
                point,
            } => slice.add_custom_data_for_symbol(symbol.clone(), ticker.clone(), point.clone()),
            SubscriptionDataPoint::OptionChain {
                canonical_permtick,
                chain,
                ..
            } => slice.add_option_chain(canonical_permtick.clone(), chain.clone()),
        }
    }
}
