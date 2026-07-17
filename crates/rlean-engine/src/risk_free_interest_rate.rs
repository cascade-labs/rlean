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
    };
    let registration = sidecar
        .add_subscription_spec(spec, DeliveryMode::Backtest)
        .await
        .context("register canonical risk-free interest-rate subscription")?;
    let subscription_id = registration.subscription_id;
    let start = Utc
        .with_ymd_and_hms(1998, 1, 1, 0, 0, 0)
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
    DatedRiskFreeInterestRateModel::new(rates).map_err(anyhow::Error::msg)
}
