use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{NaiveDate, TimeZone, Utc};
use reqwest::{Client, StatusCode, Url};
use rlean_core::{DateTime, NanosecondTimestamp};
use rlean_data_tables::RiskFreeInterestRate;
use rust_decimal::Decimal;
use serde::Deserialize;

use crate::{
    HistoricalData, HistoricalDataProvider, HistoryRequest, RiskFreeInterestRateUnavailable,
    TimeRange,
};

const DEFAULT_BASE_URL: &str = "https://api.stlouisfed.org/fred";
/// `PCREDIT8` is the Federal Reserve discount-window primary credit rate, the
/// series C# LEAN's `InterestRateProvider` reads: the file it ships as
/// `alternative/interest-rate/usa/interest-rate.csv` carries FRED's download
/// header `DATE,PCREDIT8`. It records rate-change events rather than one row
/// per day, which is exactly what `DatedRiskFreeInterestRateModel` forward
/// fills. A deployment that prefers a T-bill proxy overrides `series_id`.
const DEFAULT_SERIES_ID: &str = "PCREDIT8";
const MAX_RETRIES: u32 = 5;
/// FRED encodes a date with no observation as a lone period.
const MISSING_OBSERVATION: &str = ".";

#[derive(Debug, Clone)]
pub struct FredConfig {
    pub api_key: String,
    pub base_url: String,
    /// FRED series id to read. Defaults to the primary credit rate; switching
    /// to a T-bill series such as `DTB3` or `DGS3MO` is a configuration
    /// change, not a code change.
    pub series_id: String,
    pub timeout: Duration,
}

impl FredConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            series_id: DEFAULT_SERIES_ID.to_string(),
            timeout: Duration::from_secs(30),
        }
    }
}

/// FRED publishes macroeconomic series, not market data. It participates in
/// the provider list purely as a risk-free interest-rate publisher.
pub struct FredHistoricalDataProvider {
    client: Client,
    config: Arc<FredConfig>,
}

impl FredHistoricalDataProvider {
    pub fn new(config: FredConfig) -> Result<Self> {
        if config.api_key.trim().is_empty() {
            bail!("FRED API key cannot be empty");
        }
        if config.base_url.trim().is_empty() {
            bail!("FRED base URL cannot be empty");
        }
        if config.series_id.trim().is_empty() {
            bail!("FRED series id cannot be empty");
        }
        let client = Client::builder()
            .timeout(config.timeout)
            .gzip(true)
            .build()
            .context("build FRED HTTP client")?;
        Ok(Self {
            client,
            config: Arc::new(config),
        })
    }

    fn observations_url(&self, range: TimeRange) -> Result<Url> {
        // `TimeRange` is half-open; FRED's `observation_end` is inclusive.
        let start = range.start.date_utc();
        let end = NanosecondTimestamp(range.end.0 - 1).date_utc();
        let mut url = Url::parse(&format!(
            "{}/series/observations",
            self.config.base_url.trim_end_matches('/')
        ))
        .context("build FRED observations URL")?;
        url.query_pairs_mut()
            .append_pair("series_id", &self.config.series_id)
            .append_pair("api_key", &self.config.api_key)
            .append_pair("file_type", "json")
            .append_pair("sort_order", "asc")
            .append_pair("observation_start", &start.format("%Y-%m-%d").to_string())
            .append_pair("observation_end", &end.format("%Y-%m-%d").to_string());
        Ok(url)
    }

    async fn get_observations(&self, url: Url) -> Result<Vec<Observation>> {
        for attempt in 0..=MAX_RETRIES {
            tracing::debug!(
                url = %redacted_url(&url),
                attempt,
                series_id = %self.config.series_id,
                "requesting FRED observations"
            );
            let response = match self.client.get(url.clone()).send().await {
                Ok(response) => response,
                Err(error) if attempt < MAX_RETRIES => {
                    let delay = Duration::from_secs(2_u64.pow(attempt + 1));
                    tracing::warn!(%error, ?delay, "FRED request failed; retrying");
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(error) => {
                    return Err(RiskFreeInterestRateUnavailable::new(format!(
                        "request FRED observations: {error}"
                    )));
                }
            };
            let status = response.status();
            if attempt < MAX_RETRIES
                && (status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error())
            {
                let delay = Duration::from_secs(2_u64.pow(attempt + 1));
                tracing::warn!(%status, ?delay, "FRED throttled the request; retrying");
                tokio::time::sleep(delay).await;
                continue;
            }
            let body = match response.text().await {
                Ok(body) => body,
                Err(error) => {
                    return Err(RiskFreeInterestRateUnavailable::new(format!(
                        "read FRED response: {error}"
                    )));
                }
            };
            if !status.is_success() {
                return Err(RiskFreeInterestRateUnavailable::new(format!(
                    "FRED HTTP {status} for {}: {}",
                    redacted_url(&url),
                    body.chars().take(500).collect::<String>()
                )));
            }
            // A successful response that does not decode is a contract change,
            // not an outage. Fail loudly rather than pricing the whole run at
            // LEAN's default rate.
            let response: ObservationsResponse =
                serde_json::from_str(&body).with_context(|| {
                    format!("decode FRED observations for {}", self.config.series_id)
                })?;
            tracing::debug!(
                series_id = %self.config.series_id,
                observations = response.observations.len(),
                "received FRED observations"
            );
            return Ok(response.observations);
        }
        unreachable!("FRED retry loop returns")
    }
}

#[async_trait]
impl HistoricalDataProvider for FredHistoricalDataProvider {
    fn name(&self) -> &str {
        "fred"
    }

    fn supports(&self, _request: &HistoryRequest) -> bool {
        false
    }

    async fn get_history(&self, _request: &HistoryRequest) -> Result<HistoricalData> {
        bail!("FRED publishes macroeconomic series, not market-data history")
    }

    async fn get_risk_free_interest_rates(
        &self,
        range: TimeRange,
    ) -> Result<Option<Vec<RiskFreeInterestRate>>> {
        let url = self.observations_url(range)?;
        let observations = self.get_observations(url).await?;
        Ok(Some(interest_rates(&self.config.series_id, &observations)?))
    }
}

#[derive(Debug, Deserialize)]
struct ObservationsResponse {
    observations: Vec<Observation>,
}

#[derive(Debug, Deserialize)]
struct Observation {
    date: String,
    value: String,
}

/// FRED publishes these rates in percent (`5.5` means 5.5%); every rlean
/// risk-free model is a decimal fraction (`0.01` is 1%). This function is the
/// single place the two units meet.
fn percent_to_fraction(percent: Decimal) -> Decimal {
    percent / Decimal::new(100, 0)
}

fn interest_rates(
    series_id: &str,
    observations: &[Observation],
) -> Result<Vec<RiskFreeInterestRate>> {
    let mut rows = Vec::with_capacity(observations.len());
    for observation in observations {
        let value = observation.value.trim();
        // A missing observation is a gap in the published series, not a
        // malformed row. The dated model forward fills across it.
        if value == MISSING_OBSERVATION {
            continue;
        }
        let percent = Decimal::from_str_exact(value).with_context(|| {
            format!(
                "parse FRED {series_id} observation value '{}'",
                observation.value
            )
        })?;
        let date =
            NaiveDate::parse_from_str(observation.date.trim(), "%Y-%m-%d").with_context(|| {
                format!(
                    "parse FRED {series_id} observation date '{}'",
                    observation.date
                )
            })?;
        rows.push(
            RiskFreeInterestRate::new(midnight_utc(date)?, percent_to_fraction(percent))
                .with_venue("fred"),
        );
    }
    Ok(rows)
}

fn midnight_utc(date: NaiveDate) -> Result<DateTime> {
    let midnight = date
        .and_hms_opt(0, 0, 0)
        .with_context(|| format!("{date} has no UTC midnight"))?;
    Ok(DateTime::from(Utc.from_utc_datetime(&midnight)))
}

fn redacted_url(url: &Url) -> String {
    let mut redacted = url.clone();
    let pairs = url
        .query_pairs()
        .map(|(name, value)| {
            let value = if name == "api_key" {
                "REDACTED".to_string()
            } else {
                value.into_owned()
            };
            (name.into_owned(), value)
        })
        .collect::<Vec<_>>();
    redacted.query_pairs_mut().clear().extend_pairs(pairs);
    redacted.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn observation(date: &str, value: &str) -> Observation {
        Observation {
            date: date.to_string(),
            value: value.to_string(),
        }
    }

    #[test]
    fn percent_observations_become_decimal_fractions() {
        let rows = interest_rates(
            DEFAULT_SERIES_ID,
            &[
                observation("2003-01-09", "2.25"),
                observation("2023-07-27", "5.5"),
            ],
        )
        .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].annual_rate, dec!(0.0225));
        assert_eq!(rows[1].annual_rate, dec!(0.055));
        assert_eq!(rows[0].venue.as_deref(), Some("fred"));
        assert_eq!(
            rows[1].time,
            midnight_utc(NaiveDate::from_ymd_opt(2023, 7, 27).unwrap()).unwrap()
        );
    }

    #[test]
    fn missing_observations_are_skipped_but_bad_numbers_fail() {
        let rows = interest_rates(DEFAULT_SERIES_ID, &[observation("2020-01-01", ".")]).unwrap();
        assert!(rows.is_empty());

        assert!(interest_rates(DEFAULT_SERIES_ID, &[observation("2020-01-01", "5.5%")]).is_err());
        assert!(interest_rates(DEFAULT_SERIES_ID, &[observation("01/02/2020", "5.5")]).is_err());
    }

    #[test]
    fn observations_url_uses_an_inclusive_end_and_hides_the_api_key() {
        let provider = FredHistoricalDataProvider::new(FredConfig::new("secret")).unwrap();
        let range = TimeRange::new(
            midnight_utc(NaiveDate::from_ymd_opt(1998, 1, 1).unwrap()).unwrap(),
            midnight_utc(NaiveDate::from_ymd_opt(2024, 3, 2).unwrap()).unwrap(),
        )
        .unwrap();

        let url = provider.observations_url(range).unwrap();

        assert!(url.as_str().contains("observation_start=1998-01-01"));
        assert!(url.as_str().contains("observation_end=2024-03-01"));
        assert!(url.as_str().contains("series_id=PCREDIT8"));
        assert!(!redacted_url(&url).contains("secret"));
    }

    #[test]
    fn an_empty_api_key_is_rejected() {
        assert!(FredHistoricalDataProvider::new(FredConfig::new("  ")).is_err());
    }
}
