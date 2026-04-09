use crate::account_sync::{AccountState, AccountSynchronizer};
use crate::live_data_feed::{ILiveDataFeed, LiveDataEvent};
use tracing::{info, warn};

/// Configuration for a live trading session.
pub struct LiveTradingConfig {
    pub algorithm_name: String,
    pub poll_interval_secs: u64,
    pub account_sync_interval_secs: u64,
    pub paper_trading: bool,
}

/// The live trading engine orchestrates the data feed, account sync, and order loop.
pub struct LiveEngine {
    pub config: LiveTradingConfig,
}

impl LiveEngine {
    pub fn new(config: LiveTradingConfig) -> Self {
        Self { config }
    }

    /// Run a live trading session.
    ///
    /// Callers provide:
    /// - `feed`: a boxed live data feed (already started or to be started here)
    /// - `initial_account`: the starting account snapshot
    ///
    /// The engine runs until the data feed signals `Disconnected` or the channel closes.
    pub async fn run(
        &self,
        feed: &mut dyn ILiveDataFeed,
        initial_account: Option<AccountState>,
    ) -> anyhow::Result<()> {
        info!(
            "Live trading engine starting: {} (paper={})",
            self.config.algorithm_name, self.config.paper_trading
        );

        if let Some(acct) = &initial_account {
            info!(
                "Initial account snapshot — cash: {}, positions: {:?}",
                acct.cash,
                acct.positions.keys().collect::<Vec<_>>()
            );
        }

        // Start the feed.
        feed.start().await?;
        info!("Live data feed started");

        // Take the event channel from the feed.
        let mut rx = feed
            .event_receiver()
            .ok_or_else(|| anyhow::anyhow!("event_receiver already taken"))?;

        let sync_interval = self.config.account_sync_interval_secs;
        let _account_sync = AccountSynchronizer::new(sync_interval);

        // Main event loop.
        loop {
            match rx.recv().await {
                Some(LiveDataEvent::Bar(bar)) => {
                    info!(
                        "Live bar received: {} close={} time={}",
                        bar.symbol.id.ticker, bar.close, bar.time
                    );
                    // TODO: call algorithm.on_data(bar) → process signals → route orders
                }
                Some(LiveDataEvent::Quote { symbol, bid, ask }) => {
                    info!("Quote: {} bid={} ask={}", symbol.id.ticker, bid, ask);
                }
                Some(LiveDataEvent::HeartBeat) => {
                    // Routine heartbeat — no action needed.
                }
                Some(LiveDataEvent::Disconnected) => {
                    warn!("Live data feed disconnected");
                    break;
                }
                Some(LiveDataEvent::Reconnected) => {
                    info!("Live data feed reconnected");
                }
                None => {
                    info!("Live data feed channel closed, stopping engine");
                    break;
                }
            }
        }

        feed.stop().await?;
        info!("Live trading engine stopped: {}", self.config.algorithm_name);
        Ok(())
    }
}
