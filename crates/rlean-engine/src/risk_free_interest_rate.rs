use anyhow::{Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use rlean_core::{DatedRiskFreeInterestRateModel, NanosecondTimestamp};
use rlean_data_providers::{HistoricalDataProvider, RiskFreeInterestRateUnavailable, TimeRange};
use std::collections::BTreeMap;
use std::sync::Arc;

const FIRST_INTEREST_RATE_DATE: (i32, u32, u32) = (1998, 1, 1);

fn default_risk_free_rate() -> rust_decimal::Decimal {
    rust_decimal::Decimal::new(1, 2)
}

/// Reads the canonical `risk_free_interest_rates` table through the cache-first
/// historical provider, mirroring LEAN's `InterestRateProvider`: the whole
/// published series from 1998 up to the run's own frontier, forward filled by
/// `DatedRiskFreeInterestRateModel`.
pub async fn load_risk_free_interest_rate_model(
    provider: &Arc<dyn HistoricalDataProvider>,
    end: NaiveDate,
) -> Result<DatedRiskFreeInterestRateModel> {
    let start = NaiveDate::from_ymd_opt(
        FIRST_INTEREST_RATE_DATE.0,
        FIRST_INTEREST_RATE_DATE.1,
        FIRST_INTEREST_RATE_DATE.2,
    )
    .expect("valid LEAN interest-rate start");
    // A run that ends before the series begins still needs its first rate, so
    // the requested window never collapses below a single day.
    let end_exclusive = end
        .max(start)
        .succ_opt()
        .context("risk-free interest-rate end date has no following day")?;
    let range = TimeRange::new(midnight_utc(start)?, midnight_utc(end_exclusive)?)?;
    let rates = match provider.get_risk_free_interest_rates(range).await {
        Ok(Some(rows)) => rows
            .into_iter()
            .map(|row| (row.time.date_utc(), row.annual_rate))
            .collect(),
        Ok(None) => {
            tracing::warn!(
                "no configured historical provider publishes risk-free interest rates, and none \
                 are cached"
            );
            BTreeMap::new()
        }
        // An unreachable publisher must not unwind a multi-hour run. A
        // malformed response still does: that is a correctness fault, and a
        // wrong rate silently corrupts every risk-adjusted statistic.
        Err(error) if error.is::<RiskFreeInterestRateUnavailable>() => {
            tracing::warn!(
                error = format!("{error:#}"),
                "risk-free interest rates are unavailable"
            );
            BTreeMap::new()
        }
        Err(error) => return Err(error),
    };
    dated_model_or_lean_default(rates)
}

fn midnight_utc(date: NaiveDate) -> Result<NanosecondTimestamp> {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .with_context(|| format!("{date} has no UTC midnight"))?;
    Ok(NanosecondTimestamp::from(Utc.from_utc_datetime(&midnight)))
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
        tracing::warn!(
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
    use anyhow::bail;
    use async_trait::async_trait;
    use chrono::TimeZone;
    use parking_lot::Mutex;
    use rlean_core::{DateTime, RiskFreeInterestRateModel};
    use rlean_data_providers::{HistoricalData, HistoryRequest};
    use rlean_data_tables::RiskFreeInterestRate;
    use rust_decimal_macros::dec;

    /// Stands in for the cache-first provider. Only the risk-free rate
    /// boundary is exercised; market-data history is not part of this path.
    struct RateProvider {
        rates: Result<Option<Vec<RiskFreeInterestRate>>, fn() -> anyhow::Error>,
        requested: Mutex<Option<TimeRange>>,
    }

    impl RateProvider {
        fn new(rates: Result<Option<Vec<RiskFreeInterestRate>>, fn() -> anyhow::Error>) -> Self {
            Self {
                rates,
                requested: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl HistoricalDataProvider for RateProvider {
        fn name(&self) -> &str {
            "rates"
        }

        fn supports(&self, _request: &HistoryRequest) -> bool {
            false
        }

        async fn get_history(&self, _request: &HistoryRequest) -> Result<HistoricalData> {
            bail!("the risk-free rate loader must not request market data")
        }

        async fn get_risk_free_interest_rates(
            &self,
            range: TimeRange,
        ) -> Result<Option<Vec<RiskFreeInterestRate>>> {
            *self.requested.lock() = Some(range);
            match &self.rates {
                Ok(rates) => Ok(rates.clone()),
                Err(error) => Err(error()),
            }
        }
    }

    fn date(year: i32, month: u32, day: u32) -> DateTime {
        DateTime::from(
            Utc.with_ymd_and_hms(year, month, day, 0, 0, 0)
                .single()
                .unwrap(),
        )
    }

    fn rate(
        year: i32,
        month: u32,
        day: u32,
        annual_rate: rust_decimal::Decimal,
    ) -> RiskFreeInterestRate {
        RiskFreeInterestRate::new(date(year, month, day), annual_rate).with_venue("fred")
    }

    fn provider(
        rates: Result<Option<Vec<RiskFreeInterestRate>>, fn() -> anyhow::Error>,
    ) -> (Arc<dyn HistoricalDataProvider>, Arc<RateProvider>) {
        let provider = Arc::new(RateProvider::new(rates));
        (provider.clone(), provider)
    }

    #[test]
    fn empty_series_uses_lean_default_rate() {
        let model = dated_model_or_lean_default(BTreeMap::new()).unwrap();

        assert_eq!(
            model.get_interest_rate(date(2026, 7, 20)),
            default_risk_free_rate()
        );
    }

    #[tokio::test]
    async fn published_rates_are_loaded_and_forward_filled() {
        let (provider, _) = provider(Ok(Some(vec![
            rate(2003, 1, 9, dec!(0.0225)),
            rate(2023, 7, 27, dec!(0.055)),
        ])));

        let model = load_risk_free_interest_rate_model(
            &provider,
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(model.get_interest_rate(date(2003, 1, 9)), dec!(0.0225));
        assert_eq!(model.get_interest_rate(date(2010, 5, 4)), dec!(0.0225));
        assert_eq!(model.get_interest_rate(date(2024, 1, 31)), dec!(0.055));
    }

    #[tokio::test]
    async fn an_empty_canonical_table_falls_back_to_the_lean_default() {
        let (provider, _) = provider(Ok(Some(Vec::new())));

        let model = load_risk_free_interest_rate_model(
            &provider,
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            model.get_interest_rate(date(2024, 1, 31)),
            default_risk_free_rate()
        );
    }

    /// A deployment with no FRED credential publishes no rates at all. That is
    /// a legitimate configuration, not a failed run.
    #[tokio::test]
    async fn an_unconfigured_publisher_falls_back_without_an_error() {
        let (provider, _) = provider(Ok(None));

        let model = load_risk_free_interest_rate_model(
            &provider,
            NaiveDate::from_ymd_opt(2024, 1, 31).unwrap(),
        )
        .await
        .unwrap();

        assert_eq!(
            model.get_interest_rate(date(2024, 1, 31)),
            default_risk_free_rate()
        );
    }

    #[tokio::test]
    async fn an_unreachable_publisher_degrades_but_a_malformed_one_fails() {
        let (unavailable, _) = provider(Err(|| {
            RiskFreeInterestRateUnavailable::new("request FRED observations: connection refused")
        }));
        let (malformed, _) = provider(Err(|| anyhow::anyhow!("decode FRED observations")));
        let end = NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();

        let model = load_risk_free_interest_rate_model(&unavailable, end)
            .await
            .unwrap();
        assert_eq!(
            model.get_interest_rate(date(2024, 1, 31)),
            default_risk_free_rate()
        );

        assert!(load_risk_free_interest_rate_model(&malformed, end)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn the_requested_window_is_bounded_by_the_run_end_date() {
        let (provider, recorder) = provider(Ok(Some(Vec::new())));

        load_risk_free_interest_rate_model(&provider, NaiveDate::from_ymd_opt(2024, 3, 1).unwrap())
            .await
            .unwrap();

        let requested = recorder
            .requested
            .lock()
            .expect("the loader asks for a range");
        assert_eq!(requested.start, date(1998, 1, 1));
        assert_eq!(requested.end, date(2024, 3, 2));
    }
}
