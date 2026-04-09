use lean_core::OptionRight;
use rust_decimal::Decimal;
use chrono::{Datelike, NaiveDate};
use crate::contract::OptionContract;

/// Fluent filter builder for selecting option contracts from a universe.
/// Mirrors C# `OptionFilterUniverse`.
pub struct OptionFilterUniverse {
    contracts: Vec<OptionContract>,
    underlying_price: Decimal,
}

impl OptionFilterUniverse {
    pub fn new(contracts: Vec<OptionContract>, underlying_price: Decimal) -> Self {
        OptionFilterUniverse { contracts, underlying_price }
    }

    /// Finish filtering and return the selected contracts.
    pub fn into_contracts(self) -> Vec<OptionContract> { self.contracts }

    /// Filter by strike distance from ATM.
    /// minStrike=-1, maxStrike=+1 means 1 strike below and above ATM.
    pub fn strikes(mut self, min_strike: i32, max_strike: i32) -> Self {
        let unique_strikes: Vec<Decimal> = {
            let set: std::collections::BTreeSet<_> = self.contracts.iter()
                .map(|c| c.strike)
                .collect();
            set.into_iter().collect()
        };

        if unique_strikes.is_empty() {
            self.contracts.clear();
            return self;
        }

        // Find ATM index
        let atm_idx = unique_strikes.partition_point(|&s| s < self.underlying_price);
        let atm_idx = if atm_idx == unique_strikes.len() { atm_idx.saturating_sub(1) } else { atm_idx };

        let min_idx = (atm_idx as i32 + min_strike).max(0) as usize;
        let max_idx = ((atm_idx as i32 + max_strike).max(0) as usize)
            .min(unique_strikes.len().saturating_sub(1));

        if min_idx > max_idx {
            self.contracts.clear();
            return self;
        }

        let min_price = unique_strikes[min_idx];
        let max_price = unique_strikes[max_idx];

        self.contracts.retain(|c| c.strike >= min_price && c.strike <= max_price);
        self
    }

    /// Filter by days to expiry.
    pub fn expiration(mut self, min_days: i64, max_days: i64) -> Self {
        let today = chrono::Utc::now().date_naive();
        self.contracts.retain(|c| {
            let days = (c.expiry - today).num_days();
            days >= min_days && days <= max_days
        });
        self
    }

    /// Filter by expiry date range directly.
    pub fn expiration_dates(mut self, min_date: NaiveDate, max_date: NaiveDate) -> Self {
        self.contracts.retain(|c| c.expiry >= min_date && c.expiry <= max_date);
        self
    }

    pub fn calls_only(mut self) -> Self {
        self.contracts.retain(|c| c.right == OptionRight::Call);
        self
    }

    pub fn puts_only(mut self) -> Self {
        self.contracts.retain(|c| c.right == OptionRight::Put);
        self
    }

    /// Filter by delta range (inclusive).
    pub fn delta(mut self, min: Decimal, max: Decimal) -> Self {
        self.contracts.retain(|c| {
            let d = c.data.greeks.delta;
            d >= min && d <= max
        });
        self
    }

    /// Filter by implied volatility range.
    pub fn implied_volatility(mut self, min: Decimal, max: Decimal) -> Self {
        self.contracts.retain(|c| {
            let iv = c.data.implied_volatility;
            iv >= min && iv <= max
        });
        self
    }

    /// Filter by open interest minimum.
    pub fn open_interest(mut self, min: Decimal) -> Self {
        self.contracts.retain(|c| c.data.open_interest >= min);
        self
    }

    /// Include only standard (monthly) contracts — expire 3rd Friday of month.
    pub fn standard_contracts_only(mut self) -> Self {
        self.contracts.retain(|c| is_standard_contract(c.expiry));
        self
    }

    /// Include weekly contracts (non-standard expiries).
    pub fn include_weeklys(self) -> Self { self } // no-op: all contracts included by default

    /// Apply a custom predicate.
    pub fn where_contract<F: Fn(&OptionContract) -> bool>(mut self, f: F) -> Self {
        self.contracts.retain(|c| f(c));
        self
    }
}

/// Returns true if the expiry falls on the standard monthly expiry (3rd Friday).
pub fn is_standard_contract(expiry: NaiveDate) -> bool {
    use chrono::Weekday;
    if expiry.weekday() != Weekday::Fri { return false; }
    let first_day = NaiveDate::from_ymd_opt(expiry.year(), expiry.month(), 1).unwrap();
    let first_friday_offset = (Weekday::Fri.num_days_from_monday() as i32
        - first_day.weekday().num_days_from_monday() as i32)
        .rem_euclid(7);
    let third_friday = first_friday_offset + 14; // 0-indexed day-of-month
    expiry.day() == (third_friday + 1) as u32
}
