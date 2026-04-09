use lean_core::Symbol;
use lean_data::TradeBar;
use rust_decimal::Decimal;
use std::future::Future;
use std::pin::Pin;
use tokio::sync::mpsc;

/// A live data subscription event
#[derive(Debug, Clone)]
pub enum LiveDataEvent {
    Bar(TradeBar),
    Quote {
        symbol: Symbol,
        bid: Decimal,
        ask: Decimal,
    },
    HeartBeat,
    Disconnected,
    Reconnected,
}

/// Trait for live data feeds. Mirrors C# IDataFeed.
/// Uses boxed futures instead of async_trait to avoid the external dependency.
pub trait ILiveDataFeed: Send + Sync {
    /// Subscribe to real-time data for a symbol
    fn subscribe(
        &mut self,
        symbol: Symbol,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>>;

    /// Unsubscribe from a symbol
    fn unsubscribe(
        &mut self,
        symbol: Symbol,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>>;

    /// Returns a receiver channel for live data events.
    /// Can only be called once — takes ownership of the receiver.
    fn event_receiver(&mut self) -> Option<mpsc::Receiver<LiveDataEvent>>;

    /// Start the feed (connect, begin streaming)
    fn start(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>>;

    /// Stop the feed
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>>;

    fn name(&self) -> &str {
        "LiveDataFeed"
    }
}
