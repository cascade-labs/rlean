use lean_brokerages::Brokerage;
use lean_orders::order::Order;
use rust_decimal::Decimal;
use std::collections::HashMap;
use tracing::info;

#[derive(Debug, Clone)]
pub struct AccountState {
    /// Total USD cash balance.
    pub cash: Decimal,
    /// All currency balances keyed by currency code (e.g. "USD").
    pub cash_balances: Vec<(String, Decimal)>,
    /// Ticker → quantity for all held positions.
    pub positions: HashMap<String, Decimal>,
    pub open_orders: Vec<Order>,
    pub last_sync_time: chrono::DateTime<chrono::Utc>,
}

/// Periodically syncs account state from a brokerage.
pub struct AccountSynchronizer {
    pub sync_interval_secs: u64,
}

impl AccountSynchronizer {
    pub fn new(sync_interval_secs: u64) -> Self {
        Self { sync_interval_secs }
    }

    /// Fetch current account state from the brokerage (synchronous).
    /// In an async context use `tokio::task::block_in_place(|| self.sync_blocking(brokerage))`.
    pub fn sync_blocking(&self, brokerage: &dyn Brokerage) -> anyhow::Result<AccountState> {
        info!("AccountSynchronizer: syncing account state");

        let cash_balances = brokerage.get_cash_balance();

        // Sum USD balance as a simple aggregate cash figure.
        let cash: Decimal = cash_balances
            .iter()
            .filter(|(currency, _)| currency.eq_ignore_ascii_case("USD"))
            .map(|(_, amt)| *amt)
            .sum();

        let holdings = brokerage.get_account_holdings();
        let open_orders = brokerage.get_open_orders();

        let positions: HashMap<String, Decimal> = holdings
            .into_iter()
            .map(|(sym, qty)| (sym.id.ticker.clone(), qty))
            .collect();

        Ok(AccountState {
            cash,
            cash_balances,
            positions,
            open_orders,
            last_sync_time: chrono::Utc::now(),
        })
    }

    /// Async helper — runs `sync_blocking` on the tokio blocking thread pool.
    /// Requires the brokerage to be wrapped in an `Arc<Mutex<_>>` or similar
    /// so ownership can be moved into the blocking task.
    pub async fn sync_with_blocking<F>(&self, fetch: F) -> anyhow::Result<AccountState>
    where
        F: FnOnce() -> anyhow::Result<AccountState> + Send + 'static,
    {
        tokio::task::spawn_blocking(fetch).await?
    }
}
