use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use rlean_core::{DatedRiskFreeInterestRateModel, NanosecondTimestamp};
use rlean_data_sidecar::{
    decode_batch, CanonicalDataBatch, DataSidecarClient, DeliveryMode, SubscriptionSpec,
    WireDataType,
};
use tokio_stream::StreamExt;

const FIRST_INTEREST_RATE_DATE: (i32, u32, u32) = (1998, 1, 1);

fn default_risk_free_rate() -> rust_decimal::Decimal {
    rust_decimal::Decimal::new(1, 2)
}

pub async fn load_risk_free_interest_rate_model(
    sidecar: &Arc<DataSidecarClient>,
    end: NaiveDate,
) -> Result<DatedRiskFreeInterestRateModel> {
    let spec = SubscriptionSpec {
        config_id: 0,
        symbol_sid: 0,
        symbol_value: String::new(),
        permanent_ticker: String::new(),
        security_type: 0,
        market: "usa".to_string(),
        resolution: 4,
        tick_type: 0,
        data_type: WireDataType::RiskFreeInterestRate as i32,
        extended_market_hours: false,
        source_type: String::new(),
        ticker: String::new(),
        custom_query: None,
        properties: Default::default(),
        venue: String::new(),
        option_underlying_ticker: String::new(),
        option_min_strike_rank: 0,
        option_max_strike_rank: 0,
        option_min_expiry_days: 0,
        option_max_expiry_days: 0,
    };
    let registration = sidecar
        .add_subscription_spec(spec, DeliveryMode::Backtest)
        .await
        .context("register canonical risk-free interest-rate subscription")?;
    let subscription_id = registration.subscription_id;
    let start = Utc
        .with_ymd_and_hms(
            FIRST_INTEREST_RATE_DATE.0,
            FIRST_INTEREST_RATE_DATE.1,
            FIRST_INTEREST_RATE_DATE.2,
            0,
            0,
            0,
        )
        .single()
        .expect("valid LEAN interest-rate start");
    let end = Utc.from_utc_datetime(&end.and_hms_opt(23, 59, 59).expect("valid end of day"));
    let mut stream = sidecar
        .query(
            subscription_id,
            NanosecondTimestamp::from(start).0,
            NanosecondTimestamp::from(end).0,
        )
        .await
        .context("query canonical risk-free interest rates")?;
    let mut rates = BTreeMap::new();
    while let Some(batch) = stream.next().await {
        let batch = batch.context("stream canonical risk-free interest rates")?;
        match decode_batch(
            WireDataType::RiskFreeInterestRate,
            batch,
            &rlean_core::Symbol::create_base(
                "risk_free_interest_rate",
                "RATE",
                &rlean_core::Market::usa(),
            ),
        )? {
            CanonicalDataBatch::RiskFreeInterestRates(rows) => {
                for row in rows {
                    rates.insert(row.time.date_utc(), row.annual_rate);
                }
            }
            _ => anyhow::bail!("sidecar returned a non-interest-rate canonical batch"),
        }
    }
    sidecar
        .remove_subscription(subscription_id)
        .await
        .context("remove canonical risk-free interest-rate subscription")?;
    dated_model_or_lean_default(rates)
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
