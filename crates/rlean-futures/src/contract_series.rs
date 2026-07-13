use crate::expiry::{compute_expiry, ExpiryRule};
use chrono::{Datelike, NaiveDate};

/// Month codes used in futures tickers (Jan=F, Feb=G, ... Dec=Z)
pub const MONTH_CODES: [char; 12] = ['F', 'G', 'H', 'J', 'K', 'M', 'N', 'Q', 'U', 'V', 'X', 'Z'];

pub fn month_code(month: u32) -> char {
    MONTH_CODES[(month - 1) as usize]
}

pub fn month_from_code(code: char) -> Option<u32> {
    MONTH_CODES
        .iter()
        .position(|&c| c == code)
        .map(|i| i as u32 + 1)
}

/// A futures contract in a series.
#[derive(Debug, Clone)]
pub struct FuturesContract {
    pub underlying: String, // e.g. "ES"
    pub expiry: NaiveDate,
    pub ticker: String, // e.g. "ES H25"
    pub is_active: bool,
    pub open_interest: Option<u64>,
}

impl FuturesContract {
    pub fn new(underlying: &str, year: i32, month: u32, rule: ExpiryRule) -> Self {
        let expiry = compute_expiry(rule, year, month);
        let ticker = format!("{} {}{}", underlying, month_code(month), year % 100);
        Self {
            underlying: underlying.to_string(),
            expiry,
            ticker,
            is_active: true,
            open_interest: None,
        }
    }
}

/// A series of quarterly futures contracts.
pub struct FuturesContractSeries {
    pub underlying: String,
    pub expiry_rule: ExpiryRule,
    pub active_months: Vec<u32>, // e.g. [3, 6, 9, 12] for quarterly
}

impl FuturesContractSeries {
    /// Standard quarterly series (Mar, Jun, Sep, Dec)
    pub fn quarterly(underlying: &str) -> Self {
        Self {
            underlying: underlying.to_string(),
            expiry_rule: ExpiryRule::ThirdFriday,
            active_months: vec![3, 6, 9, 12],
        }
    }

    /// Monthly series
    pub fn monthly(underlying: &str, rule: ExpiryRule) -> Self {
        Self {
            underlying: underlying.to_string(),
            expiry_rule: rule,
            active_months: (1..=12).collect(),
        }
    }

    /// Get all contracts for the next N months
    pub fn upcoming_contracts(&self, from: NaiveDate, count: usize) -> Vec<FuturesContract> {
        let mut result = Vec::new();
        let mut year = from.year();
        let mut month = from.month();

        while result.len() < count {
            if self.active_months.contains(&month) {
                let contract =
                    FuturesContract::new(&self.underlying, year, month, self.expiry_rule);
                if contract.expiry > from {
                    result.push(contract);
                }
            }
            month += 1;
            if month > 12 {
                month = 1;
                year += 1;
            }
        }
        result
    }

    /// Get the front month contract (nearest expiry)
    pub fn front_month(&self, from: NaiveDate) -> Option<FuturesContract> {
        self.upcoming_contracts(from, 1).into_iter().next()
    }
}
