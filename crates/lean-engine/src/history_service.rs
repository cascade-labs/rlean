use anyhow::{anyhow, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use lean_core::{DateTime, Resolution, Symbol};
use lean_data::{
    CustomDataFormat, CustomDataPoint, CustomDataSubscription, CustomDataTransport, TradeBar,
};
use lean_data_providers::{
    DataType, HistoryRequest, ICustomDataSource, IHistoryProvider, LocalHistoryProvider,
};
use lean_storage::{custom_data_history_path, custom_data_path, ParquetReader};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone)]
pub struct AlgorithmHistoryContext {
    pub data_root: PathBuf,
    pub history_provider: Option<Arc<dyn IHistoryProvider>>,
    pub custom_data_sources: Vec<Arc<dyn ICustomDataSource>>,
}

#[derive(Clone)]
pub struct HistoryService {
    context: AlgorithmHistoryContext,
}

impl HistoryService {
    pub fn new(context: AlgorithmHistoryContext) -> Self {
        Self { context }
    }

    pub fn load_trade_bars_blocking(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<TradeBar>> {
        let request = HistoryRequest {
            symbol: symbol.clone(),
            resolution,
            start: date_to_datetime(start, 0, 0, 0),
            end: date_to_datetime(end, 23, 59, 59),
            data_type: DataType::TradeBar,
        };

        if let Some(provider) = self.context.history_provider.clone() {
            return block_on_background(async move { provider.get_history(&request).await });
        }

        let local = LocalHistoryProvider::new(&self.context.data_root);
        block_on_background(async move { local.get_history(&request).await })
    }

    pub fn load_custom_history_blocking(
        &self,
        subscription: &CustomDataSubscription,
        start: NaiveDate,
        end: NaiveDate,
    ) -> Result<Vec<CustomDataPoint>> {
        let reader = ParquetReader::new();
        let full_history_path = custom_data_history_path(
            &self.context.data_root,
            &subscription.source_type,
            &subscription.ticker,
        );
        if full_history_path.exists() {
            return Ok(reader
                .read_custom_data_points(&full_history_path)
                .unwrap_or_default()
                .into_iter()
                .filter(|point| point.time >= start && point.time <= end)
                .collect());
        }

        let source = self
            .context
            .custom_data_sources
            .iter()
            .find(|source| source.name() == subscription.source_type)
            .cloned();
        let mut out = Vec::new();
        let mut date = start;
        while date <= end {
            let cache_path = custom_data_path(
                &self.context.data_root,
                &subscription.source_type,
                &subscription.ticker,
                date,
            );
            if cache_path.exists() {
                out.extend(
                    reader
                        .read_custom_data_points(&cache_path)
                        .unwrap_or_default(),
                );
                date += chrono::Duration::days(1);
                continue;
            }

            let Some(source) = source.as_ref() else {
                date += chrono::Duration::days(1);
                continue;
            };

            let mut config = subscription.config.clone();
            let effective_query = config.query.merge(&subscription.dynamic_query).merge(
                &lean_data::CustomDataQuery {
                    start_date: Some(start),
                    end_date: Some(end),
                    ..Default::default()
                },
            );
            config.query = effective_query.clone();

            if let Some(parquet_source) =
                source.get_parquet_source(&subscription.ticker, date, &config, &effective_query)
            {
                out.extend(block_on_background(async move {
                    ParquetReader::new()
                        .read_custom_parquet_points(&parquet_source, &effective_query, date)
                        .await
                        .map_err(|e| anyhow!(e))
                })?);
                date += chrono::Duration::days(1);
                continue;
            }

            if let Some(data_source) = source.get_source(&subscription.ticker, date, &config) {
                let raw = match data_source.transport {
                    CustomDataTransport::LocalFile => {
                        std::fs::read_to_string(&data_source.uri).unwrap_or_default()
                    }
                    CustomDataTransport::Http => reqwest::blocking::Client::builder()
                        .timeout(std::time::Duration::from_secs(120))
                        .user_agent("Mozilla/5.0 (compatible; rlean/0.1)")
                        .build()
                        .and_then(|client| client.get(&data_source.uri).send())
                        .and_then(|response| response.text())
                        .unwrap_or_default(),
                };
                match data_source.format {
                    CustomDataFormat::Csv => {
                        for line in raw.lines() {
                            if let Some(point) = source.reader(line, date, &config) {
                                out.push(point);
                            }
                        }
                    }
                    CustomDataFormat::Json => {
                        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                            match value {
                                serde_json::Value::Array(rows) => {
                                    for row in rows {
                                        if let Some(point) =
                                            source.reader(&row.to_string(), date, &config)
                                        {
                                            out.push(point);
                                        }
                                    }
                                }
                                row => {
                                    if let Some(point) =
                                        source.reader(&row.to_string(), date, &config)
                                    {
                                        out.push(point);
                                    }
                                }
                            }
                        }
                    }
                }
            }

            date += chrono::Duration::days(1);
        }
        out.retain(|point| point.time >= start && point.time <= end);
        Ok(out)
    }
}

fn date_to_datetime(date: NaiveDate, hour: u32, minute: u32, second: u32) -> DateTime {
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, second).unwrap()))
}

fn block_on_background<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let runtime_handle = tokio::runtime::Handle::try_current().ok();
    let handle = std::thread::spawn(move || {
        if let Some(runtime_handle) = runtime_handle {
            runtime_handle.block_on(future)
        } else {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| anyhow!(e))?
                .block_on(future)
        }
    });
    handle
        .join()
        .map_err(|_| anyhow!("history worker panicked"))?
}
