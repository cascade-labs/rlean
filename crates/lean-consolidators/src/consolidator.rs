use lean_data::TradeBar;

/// Called when a consolidated bar is ready.
pub type ConsolidatedHandler = Box<dyn FnMut(TradeBar) + Send + Sync>;

/// Base trait for all consolidators. Mirrors C# IDataConsolidator.
pub trait IConsolidator: Send + Sync {
    /// Feed a new bar into the consolidator.
    /// Returns Some(bar) when a new consolidated bar is complete.
    fn update(&mut self, bar: &TradeBar) -> Option<TradeBar>;

    /// Reset all state.
    fn reset(&mut self);

    /// Human-readable name
    fn name(&self) -> &str {
        "Consolidator"
    }
}
