use crate::live_data_feed::{ILiveDataFeed, LiveDataEvent};
use lean_core::Symbol;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// A simple polling-based live data feed that emits heartbeats at a configurable interval.
/// Concrete integrations should override the polling logic by extending this or wrapping the sender.
pub struct PollingLiveDataFeed {
    pub symbols: Vec<Symbol>,
    pub poll_interval: Duration,
    sender: mpsc::Sender<LiveDataEvent>,
    receiver: Option<mpsc::Receiver<LiveDataEvent>>,
    running: Arc<AtomicBool>,
}

impl PollingLiveDataFeed {
    pub fn new(poll_interval: Duration) -> Self {
        let (tx, rx) = mpsc::channel(1000);
        Self {
            symbols: Vec::new(),
            poll_interval,
            sender: tx,
            receiver: Some(rx),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Expose the sender so callers can inject data events directly.
    pub fn sender(&self) -> mpsc::Sender<LiveDataEvent> {
        self.sender.clone()
    }
}

impl ILiveDataFeed for PollingLiveDataFeed {
    fn subscribe(
        &mut self,
        symbol: Symbol,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            if !self.symbols.contains(&symbol) {
                info!("PollingLiveDataFeed: subscribing to {}", symbol.id.ticker);
                self.symbols.push(symbol);
            }
            Ok(())
        })
    }

    fn unsubscribe(
        &mut self,
        symbol: Symbol,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            info!("PollingLiveDataFeed: unsubscribing from {}", symbol.id.ticker);
            self.symbols.retain(|s| s != &symbol);
            Ok(())
        })
    }

    fn event_receiver(&mut self) -> Option<mpsc::Receiver<LiveDataEvent>> {
        self.receiver.take()
    }

    fn start(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            if self.running.load(Ordering::SeqCst) {
                warn!("PollingLiveDataFeed: already running");
                return Ok(());
            }
            self.running.store(true, Ordering::SeqCst);

            let sender = self.sender.clone();
            let running = Arc::clone(&self.running);
            let poll_interval = self.poll_interval;

            tokio::spawn(async move {
                info!("PollingLiveDataFeed: polling loop started (interval={poll_interval:?})");
                while running.load(Ordering::SeqCst) {
                    // Send a heartbeat tick. Concrete implementations would fetch
                    // real bars here and send LiveDataEvent::Bar(...) instead.
                    if sender.send(LiveDataEvent::HeartBeat).await.is_err() {
                        warn!("PollingLiveDataFeed: receiver dropped, stopping poll loop");
                        break;
                    }
                    tokio::time::sleep(poll_interval).await;
                }
                info!("PollingLiveDataFeed: polling loop stopped");
            });

            Ok(())
        })
    }

    fn stop(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            info!("PollingLiveDataFeed: stopping");
            self.running.store(false, Ordering::SeqCst);
            Ok(())
        })
    }

    fn name(&self) -> &str {
        "PollingLiveDataFeed"
    }
}
