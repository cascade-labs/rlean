use anyhow::{anyhow, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use lean_core::{DataNormalizationMode, DateTime, Resolution, SecurityType, Symbol};
use lean_data::{
    CustomDataFormat, CustomDataPoint, CustomDataSubscription, CustomDataTransport, TradeBar,
};
use lean_data_providers::{
    DataType, HistoryRequest, ICustomDataSource, IHistoryProvider, LocalHistoryProvider,
};
use lean_storage::{
    custom_data_history_path, custom_data_path, factor_file_path, FactorFileEntry, ParquetReader,
};
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
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

    pub fn load_trade_bars_blocking_with_normalization(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: NaiveDate,
        end: NaiveDate,
        normalization_mode: DataNormalizationMode,
    ) -> Result<Vec<TradeBar>> {
        self.load_trade_bars_between_blocking_with_normalization(
            symbol,
            resolution,
            date_to_datetime(start, 0, 0, 0),
            date_to_datetime(end, 23, 59, 59),
            normalization_mode,
        )
    }

    pub fn load_trade_bars_between_blocking_with_normalization(
        &self,
        symbol: &Symbol,
        resolution: Resolution,
        start: DateTime,
        end: DateTime,
        normalization_mode: DataNormalizationMode,
    ) -> Result<Vec<TradeBar>> {
        let request = HistoryRequest {
            symbol: symbol.clone(),
            resolution,
            start,
            end,
            data_type: DataType::TradeBar,
        };

        let mut bars = if let Some(provider) = self.context.history_provider.clone() {
            block_on_background(async move { provider.get_history(&request).await })?
        } else {
            let local = LocalHistoryProvider::new(&self.context.data_root);
            block_on_background(async move { local.get_history(&request).await })?
        };
        normalize_trade_bars(
            &self.context.data_root,
            symbol,
            normalization_mode,
            &mut bars,
        );
        Ok(bars)
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

fn normalize_trade_bars(
    data_root: &std::path::Path,
    symbol: &Symbol,
    normalization_mode: DataNormalizationMode,
    bars: &mut [TradeBar],
) {
    if bars.is_empty() || !matches!(symbol.security_type(), SecurityType::Equity) {
        return;
    }
    let factor_path = factor_file_path(data_root, symbol.market().as_str(), &symbol.permtick);
    let rows = ParquetReader::new()
        .read_factor_file(&factor_path)
        .unwrap_or_default();
    if rows.is_empty() {
        return;
    }

    for bar in bars {
        apply_normalization_factor(bar, &rows, normalization_mode);
    }
}

fn apply_normalization_factor(
    bar: &mut TradeBar,
    rows: &[FactorFileEntry],
    normalization_mode: DataNormalizationMode,
) {
    let (price_factor, split_factor) = factor_for_entry(rows, bar.time.date_utc());
    let scale = match normalization_mode {
        DataNormalizationMode::Raw => 1.0,
        DataNormalizationMode::SplitAdjusted => split_factor,
        DataNormalizationMode::Adjusted
        | DataNormalizationMode::TotalReturn
        | DataNormalizationMode::ForwardPanamaCanal
        | DataNormalizationMode::BackwardPanamaCanal => price_factor * split_factor,
    };
    if (scale - 1.0).abs() < 1e-9 {
        return;
    }

    let price_scale = Decimal::from_f64(scale).unwrap_or(Decimal::ONE);
    bar.open *= price_scale;
    bar.high *= price_scale;
    bar.low *= price_scale;
    bar.close *= price_scale;

    if !matches!(normalization_mode, DataNormalizationMode::Raw)
        && split_factor != 0.0
        && (split_factor - 1.0).abs() > 1e-9
    {
        let volume_scale = Decimal::from_f64(1.0 / split_factor).unwrap_or(Decimal::ONE);
        bar.volume *= volume_scale;
    }
}

fn factor_for_entry(rows: &[FactorFileEntry], bar_date: NaiveDate) -> (f64, f64) {
    if rows.is_empty() {
        return (1.0, 1.0);
    }
    if let Some(row) = rows
        .iter()
        .filter(|row| row.date < bar_date)
        .max_by_key(|row| row.date)
    {
        return (row.price_factor, row.split_factor);
    }
    rows.iter()
        .min_by_key(|row| row.date)
        .map(|row| (row.price_factor, row.split_factor))
        .unwrap_or((1.0, 1.0))
}

fn date_to_datetime(date: NaiveDate, hour: u32, minute: u32, second: u32) -> DateTime {
    DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, minute, second).unwrap()))
}

fn block_on_background<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let handle = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow!(e))?
            .block_on(future)
    });
    handle
        .join()
        .map_err(|_| anyhow!("history worker panicked"))?
}
