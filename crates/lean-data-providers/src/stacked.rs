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
        for provider in &self.providers {
            match provider.get_history(request) {
                Ok(data) if !data.is_empty() => return Ok(data),
                Ok(_) => continue,
                Err(ref e) if is_not_implemented(e) => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(vec![])
    }
}
