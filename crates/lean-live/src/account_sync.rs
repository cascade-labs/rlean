use lean_brokerages::{Brokerage, BrokerageHolding};
use lean_orders::order::Order;
use rust_decimal::Decimal;
use std::collections::HashMap;
use tracing::info;

/// Currencies treated as settlement cash (USD and USD-pegged stablecoins).
pub fn is_cash_currency(currency: &str) -> bool {
    matches!(
        currency.to_ascii_uppercase().as_str(),
        "USD" | "USDC" | "USDT" | "USDD" | "DAI" | "BUSD"
    )
}

/// Derive the account's settlement-cash figure from per-currency balances.
///
/// Sums all cash-currency balances; if none are present, falls back to a
/// single reported balance, otherwise sums everything as a best effort.
pub fn settlement_cash(cash_balances: &[(String, Decimal)]) -> Decimal {
    if cash_balances.iter().any(|(c, _)| is_cash_currency(c)) {
        cash_balances
            .iter()
            .filter(|(c, _)| is_cash_currency(c))
            .map(|(_, amt)| *amt)
            .sum()
    } else if let [(_, amt)] = cash_balances {
        *amt
    } else {
        cash_balances.iter().map(|(_, amt)| *amt).sum()
    }
}

#[derive(Debug, Clone)]
pub struct AccountState {
    /// Total USD cash balance.
    pub cash: Decimal,
    /// All currency balances keyed by currency code (e.g. "USD").
    pub cash_balances: Vec<(String, Decimal)>,
    /// Ticker → quantity for all held positions.
    pub positions: HashMap<String, Decimal>,
    /// Detailed symbol holdings, including average price when the brokerage exposes it.
    pub holdings: Vec<BrokerageHolding>,
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

        // Aggregate the settlement-cash figure. USD and USD-pegged stablecoins
        // (USDC/USDT/etc.) are all treated as the account's cash balance so
        // non-USD brokerages like Hyperliquid (USDC) reconcile correctly. When
        // no cash-currency balance is present but a single currency is reported,
        // fall back to that balance.
        let cash: Decimal = settlement_cash(&cash_balances);

        let holdings = brokerage.get_account_detailed_holdings();
        let open_orders = brokerage.get_open_orders();

        let positions: HashMap<String, Decimal> = holdings
            .iter()
            .map(|holding| (holding.symbol.id.ticker.clone(), holding.quantity))
            .collect();

        Ok(AccountState {
            cash,
            cash_balances,
            positions,
            holdings,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn balances(items: &[(&str, Decimal)]) -> Vec<(String, Decimal)> {
        items.iter().map(|(c, a)| (c.to_string(), *a)).collect()
    }

    #[test]
    fn settlement_cash_sums_usd_and_stablecoins() {
        let b = balances(&[("USD", dec!(100)), ("USDC", dec!(50)), ("BTC", dec!(2))]);
        assert_eq!(settlement_cash(&b), dec!(150));
    }

    #[test]
    fn settlement_cash_handles_usdc_only_brokerage() {
        // Hyperliquid-style account: cash held entirely in USDC.
        let b = balances(&[("USDC", dec!(2500))]);
        assert_eq!(settlement_cash(&b), dec!(2500));
    }

    #[test]
    fn settlement_cash_falls_back_to_single_currency() {
        let b = balances(&[("EUR", dec!(900))]);
        assert_eq!(settlement_cash(&b), dec!(900));
    }

    #[test]
    fn settlement_cash_empty_is_zero() {
        assert_eq!(settlement_cash(&[]), Decimal::ZERO);
    }
}
