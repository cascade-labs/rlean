use crate::{contract_series::FuturesContractSeries, expiry::ExpiryRule};

pub fn es() -> FuturesContractSeries {
    // S&P 500 E-mini
    FuturesContractSeries::quarterly("ES")
}

pub fn nq() -> FuturesContractSeries {
    // Nasdaq E-mini
    FuturesContractSeries::quarterly("NQ")
}

pub fn cl() -> FuturesContractSeries {
    // Crude Oil (monthly, 3rd-last business day)
    FuturesContractSeries {
        underlying: "CL".to_string(),
        expiry_rule: ExpiryRule::NthFromEnd(3),
        active_months: (1..=12).collect(),
    }
}

pub fn gc() -> FuturesContractSeries {
    // Gold (bi-monthly)
    FuturesContractSeries {
        underlying: "GC".to_string(),
        expiry_rule: ExpiryRule::ThirdFriday,
        active_months: vec![2, 4, 6, 8, 10, 12],
    }
}

pub fn zb() -> FuturesContractSeries {
    // 30-yr Treasury Bond (quarterly)
    FuturesContractSeries::quarterly("ZB")
}
