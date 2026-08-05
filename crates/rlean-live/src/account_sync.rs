use rlean_brokerages::BrokerageHolding;
use rlean_orders::Order;
use rust_decimal::Decimal;
use std::collections::HashMap;

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

/// Provider-neutral account snapshot used for deployment persistence and
/// restart recovery. Acquisition comes from the selected brokerage interface.
#[derive(Debug, Clone)]
pub struct AccountState {
    pub cash: Decimal,
    pub cash_balances: Vec<(String, Decimal)>,
    pub positions: HashMap<String, Decimal>,
    pub holdings: Vec<BrokerageHolding>,
    pub open_orders: Vec<Order>,
    pub last_sync_time: chrono::DateTime<chrono::Utc>,
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
