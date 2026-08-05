use anyhow::Result;
use chrono::NaiveDate;
use rlean_core::DatedRiskFreeInterestRateModel;
use std::collections::BTreeMap;

const FIRST_INTEREST_RATE_DATE: (i32, u32, u32) = (1998, 1, 1);

fn default_risk_free_rate() -> rust_decimal::Decimal {
    rust_decimal::Decimal::new(1, 2)
}

pub fn load_risk_free_interest_rate_model(
    _end: NaiveDate,
) -> Result<DatedRiskFreeInterestRateModel> {
    dated_model_or_lean_default(BTreeMap::new())
}

fn dated_model_or_lean_default(
    mut rates: BTreeMap<NaiveDate, rust_decimal::Decimal>,
) -> Result<DatedRiskFreeInterestRateModel> {
    if rates.is_empty() {
        let first_date = NaiveDate::from_ymd_opt(
            FIRST_INTEREST_RATE_DATE.0,
            FIRST_INTEREST_RATE_DATE.1,
            FIRST_INTEREST_RATE_DATE.2,
        )
        .expect("valid LEAN interest-rate start");
        tracing::error!(
            default_rate = %default_risk_free_rate(),
            "no risk-free interest rates were loaded; using LEAN's default rate"
        );
        rates.insert(first_date, default_risk_free_rate());
    }
    DatedRiskFreeInterestRateModel::new(rates).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use rlean_core::{DateTime, RiskFreeInterestRateModel};

    #[test]
    fn empty_series_uses_lean_default_rate() {
        let model = dated_model_or_lean_default(BTreeMap::new()).unwrap();

        assert_eq!(
            model.get_interest_rate(DateTime::from(
                Utc.with_ymd_and_hms(2026, 7, 20, 0, 0, 0).single().unwrap()
            )),
            default_risk_free_rate()
        );
    }
}
