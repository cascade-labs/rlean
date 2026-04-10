/// Stacked (priority-ordered) history provider.
///
/// Tries each provider in order.  The first provider that returns a non-empty
/// `Ok` result wins.  A provider that returns `Ok(vec![])` or an
/// `anyhow::Error` whose message starts with "NotImplemented:" is treated as
/// "I don't have this data — try the next one".  Any other error short-circuits
/// and is returned immediately.
use std::sync::Arc;

use lean_data::TradeBar;

use crate::{HistoryRequest, IHistoryProvider};

/// Returns `true` when `err` indicates that the provider does not implement
/// the requested data type (as opposed to a transient network or parse error).
pub fn is_not_implemented(err: &anyhow::Error) -> bool {
    err.to_string().starts_with("NotImplemented:")
}

/// Wraps multiple `IHistoryProvider` implementations and tries them in
/// priority order.
pub struct StackedHistoryProvider {
    providers: Vec<Arc<dyn IHistoryProvider>>,
}

impl StackedHistoryProvider {
    /// Create a new stacked provider.  `providers` must be non-empty and are
    /// tried left-to-right (index 0 = highest priority).
    pub fn new(providers: Vec<Arc<dyn IHistoryProvider>>) -> Self {
        assert!(!providers.is_empty(), "StackedHistoryProvider requires at least one provider");
        StackedHistoryProvider { providers }
    }
}

impl IHistoryProvider for StackedHistoryProvider {
    fn get_history(
        &self,
        request: &HistoryRequest,
    ) -> anyhow::Result<Vec<TradeBar>> {
        // Run ALL providers regardless of earlier successes.
        //
        // Rationale: providers may have valuable side-effects beyond returning
        // bars (e.g. the massive plugin generates LEAN factor files when it
        // fetches daily equity data).  Stopping at the first successful result
        // would skip those side-effects when a higher-priority provider (e.g.
        // thetadata) already has the data.
        //
        // Semantics: first non-empty Ok wins for the returned bars; subsequent
        // providers still run.  Errors from non-primary providers (i.e. after
        // we already have bars) are logged as warnings, not propagated.
        let mut bars: Vec<TradeBar> = Vec::new();

        for provider in &self.providers {
            match provider.get_history(request) {
                Ok(data) if !data.is_empty() => {
                    if bars.is_empty() {
                        bars = data;
                    }
                    // continue — let remaining providers run for side-effects
                }
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => {
                    if bars.is_empty() {
                        // No data yet — this is a real failure.
                        return Err(e);
                    }
                    // Already have bars — log and continue.
                    tracing::warn!("Provider error (data already available from earlier provider): {e}");
                }
            }
        }

        Ok(bars)
    }
}
