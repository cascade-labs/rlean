use std::collections::BTreeMap;

use chrono::NaiveDate;
use rust_decimal::Decimal;

use crate::DateTime;

/// LEAN-compatible dated annual risk-free interest-rate model.
pub trait RiskFreeInterestRateModel: Send + Sync {
    fn get_interest_rate(&self, date: DateTime) -> Decimal;

    fn get_average_interest_rate(&self, dates: &[DateTime]) -> Decimal {
        if dates.is_empty() {
            return Decimal::ZERO;
        }
        dates
            .iter()
            .map(|date| self.get_interest_rate(*date))
            .sum::<Decimal>()
            / Decimal::from(dates.len())
    }
}

#[derive(Debug, Clone)]
pub struct ConstantRiskFreeInterestRateModel {
    rate: Decimal,
}

impl ConstantRiskFreeInterestRateModel {
    pub fn new(rate: Decimal) -> Self {
        Self { rate }
    }
}

impl RiskFreeInterestRateModel for ConstantRiskFreeInterestRateModel {
    fn get_interest_rate(&self, _date: DateTime) -> Decimal {
        self.rate
    }
}

#[derive(Debug, Clone)]
pub struct DatedRiskFreeInterestRateModel {
    rates: BTreeMap<NaiveDate, Decimal>,
}

impl DatedRiskFreeInterestRateModel {
    pub fn new(rates: BTreeMap<NaiveDate, Decimal>) -> Result<Self, &'static str> {
        if rates.is_empty() {
            return Err("risk-free interest-rate series is empty");
        }
        Ok(Self { rates })
    }

    pub fn rates(&self) -> &BTreeMap<NaiveDate, Decimal> {
        &self.rates
    }
}

impl RiskFreeInterestRateModel for DatedRiskFreeInterestRateModel {
    fn get_interest_rate(&self, date: DateTime) -> Decimal {
        let date = date.date_utc();
        self.rates
            .range(..=date)
            .next_back()
            .or_else(|| self.rates.first_key_value())
            .map(|(_, rate)| *rate)
            .expect("dated risk-free model is non-empty by construction")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use rust_decimal_macros::dec;

    fn date(year: i32, month: u32, day: u32) -> DateTime {
        DateTime::from(
            Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
                .single()
                .unwrap(),
        )
    }

    #[test]
    fn dated_model_forward_fills_rate_changes() {
        let model = DatedRiskFreeInterestRateModel::new(BTreeMap::from([
            (date(2020, 1, 1).date_utc(), dec!(0.01)),
            (date(2022, 6, 1).date_utc(), dec!(0.03)),
        ]))
        .unwrap();
        assert_eq!(model.get_interest_rate(date(2019, 1, 1)), dec!(0.01));
        assert_eq!(model.get_interest_rate(date(2021, 1, 1)), dec!(0.01));
        assert_eq!(model.get_interest_rate(date(2023, 1, 1)), dec!(0.03));
    }
}
