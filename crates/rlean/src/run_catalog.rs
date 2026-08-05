//! Durable backtest results stored as typed Arrow rows in Verglas.
//!
//! A backtest process is disposable. The catalog is the source of truth for
//! run state, progress, orders, fills, trades, insights, charts, and statistics.

use std::sync::Arc;

use anyhow::{Context, Result};
use arrow_array::{ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::stream;
use rlean_engine::{BacktestProgress, BacktestRunResult};
use serde_json::Value;
use verglas_sdk::{Client, ColumnSpec, TableDefinition};

const RUNS: &str = "rlean.runs";
const PROGRESS: &str = "rlean.run_progress";
const ORDERS: &str = "rlean.orders";
const ORDER_EVENTS: &str = "rlean.order_events";
const TRADES: &str = "rlean.trades";
const INSIGHTS: &str = "rlean.insights";
const STATISTICS: &str = "rlean.statistics";
const CHARTS: &str = "rlean.charts";
const DATA_REQUESTS: &str = "rlean.data_requests";
const LOGS: &str = "rlean.logs";

#[derive(Clone)]
pub(crate) struct RunCatalog {
    client: Client,
    run_id: String,
    strategy: String,
    started_at: chrono::DateTime<chrono::Utc>,
    parameters_json: String,
}

impl RunCatalog {
    pub(crate) async fn connect(
        client: Client,
        run_id: String,
        strategy: String,
        parameters: &std::collections::HashMap<String, String>,
    ) -> Result<Self> {
        let catalog = Self {
            client,
            run_id,
            strategy,
            started_at: chrono::Utc::now(),
            parameters_json: serde_json::to_string(parameters)?,
        };
        catalog.ensure_tables().await?;
        Ok(catalog)
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) async fn record_started(&self) -> Result<()> {
        self.append(RUNS, runs_batch(self, "running", None, None), "started")
            .await
    }

    pub(crate) async fn record_failed(&self, error: &anyhow::Error) -> Result<()> {
        self.append(
            RUNS,
            runs_batch(self, "failed", None, Some(error.to_string())),
            "failed",
        )
        .await
    }

    pub(crate) async fn record_progress(&self, rows: Vec<BacktestProgress>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let suffix = format!("progress-{}", rows.last().map_or(0, |row| row.trading_days));
        self.append(PROGRESS, progress_batch(&self.run_id, &rows), &suffix)
            .await
    }

    pub(crate) async fn record_completed(&self, result: &BacktestRunResult) -> Result<()> {
        self.append(ORDERS, orders_batch(&self.run_id, result)?, "orders")
            .await?;
        self.append(
            ORDER_EVENTS,
            order_events_batch(&self.run_id, result)?,
            "order-events",
        )
        .await?;
        self.append(TRADES, trades_batch(&self.run_id, result)?, "trades")
            .await?;
        self.append(INSIGHTS, insights_batch(&self.run_id, result)?, "insights")
            .await?;
        self.append(
            STATISTICS,
            statistics_batch(&self.run_id, result)?,
            "statistics",
        )
        .await?;
        self.append(CHARTS, charts_batch(&self.run_id, result), "charts")
            .await?;
        self.append(
            DATA_REQUESTS,
            data_requests_batch(&self.run_id, result),
            "data-requests",
        )
        .await?;
        self.append(LOGS, summary_log_batch(&self.run_id, result), "summary-log")
            .await?;
        self.append(
            RUNS,
            runs_batch(self, "completed", Some(result), None),
            "completed",
        )
        .await
    }

    async fn append(&self, table: &str, batch: RecordBatch, suffix: &str) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        self.client
            .append_stream(
                table,
                stream::iter(vec![Ok(batch)]),
                &format!("{}:{suffix}", self.run_id),
            )
            .await
            .with_context(|| format!("append backtest results to {table}"))?;
        Ok(())
    }

    async fn ensure_tables(&self) -> Result<()> {
        for (name, schema) in table_schemas() {
            self.client
                .ensure_table(name, &definition(&schema))
                .await
                .with_context(|| format!("ensure Verglas table {name}"))?;
        }
        Ok(())
    }
}

fn definition(schema: &Schema) -> TableDefinition {
    TableDefinition {
        schema: schema
            .fields()
            .iter()
            .map(|field| {
                let type_name = match field.data_type() {
                    DataType::Utf8 => "utf8",
                    DataType::Int64 => "int64",
                    DataType::Float64 => "float64",
                    DataType::Boolean => "boolean",
                    other => panic!("unsupported run catalog type: {other}"),
                };
                if field.is_nullable() {
                    ColumnSpec::nullable(field.name(), type_name)
                } else {
                    ColumnSpec::required(field.name(), type_name)
                }
            })
            .collect(),
        partitions: Vec::new(),
    }
}

fn schema(fields: Vec<Field>) -> SchemaRef {
    Arc::new(Schema::new(fields))
}

fn f(name: &str, data_type: DataType) -> Field {
    Field::new(name, data_type, false)
}

fn nf(name: &str, data_type: DataType) -> Field {
    Field::new(name, data_type, true)
}

fn table_schemas() -> Vec<(&'static str, SchemaRef)> {
    vec![
        (RUNS, runs_schema()),
        (PROGRESS, progress_schema()),
        (ORDERS, orders_schema()),
        (ORDER_EVENTS, order_events_schema()),
        (TRADES, trades_schema()),
        (INSIGHTS, insights_schema()),
        (STATISTICS, statistics_schema()),
        (CHARTS, charts_schema()),
        (DATA_REQUESTS, data_requests_schema()),
        (LOGS, logs_schema()),
    ]
}

fn runs_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("strategy", DataType::Utf8),
        f("status", DataType::Utf8),
        f("recorded_at", DataType::Utf8),
        f("started_at", DataType::Utf8),
        nf("finished_at", DataType::Utf8),
        nf("start_date", DataType::Utf8),
        nf("end_date", DataType::Utf8),
        nf("starting_cash", DataType::Float64),
        nf("final_value", DataType::Float64),
        nf("total_return", DataType::Float64),
        nf("total_fees", DataType::Float64),
        nf("error", DataType::Utf8),
        f("parameters_json", DataType::Utf8),
    ])
}

fn runs_batch(
    catalog: &RunCatalog,
    status: &str,
    result: Option<&BacktestRunResult>,
    error: Option<String>,
) -> RecordBatch {
    let finished = result.map(|_| chrono::Utc::now().to_rfc3339());
    let values = result.map(|result| {
        (
            result.start_date.to_string(),
            result.end_date.to_string(),
            result.starting_cash,
            result.final_value,
            result.total_return,
            result.total_fees,
        )
    });
    RecordBatch::try_new(
        runs_schema(),
        vec![
            strings([catalog.run_id.clone()]),
            strings([catalog.strategy.clone()]),
            strings([status.to_owned()]),
            strings([chrono::Utc::now().to_rfc3339()]),
            strings([catalog.started_at.to_rfc3339()]),
            optional_strings([finished]),
            optional_strings([values.as_ref().map(|v| v.0.clone())]),
            optional_strings([values.as_ref().map(|v| v.1.clone())]),
            optional_f64([values.as_ref().map(|v| v.2)]),
            optional_f64([values.as_ref().map(|v| v.3)]),
            optional_f64([values.as_ref().map(|v| v.4)]),
            optional_f64([values.as_ref().map(|v| v.5)]),
            optional_strings([error]),
            strings([catalog.parameters_json.clone()]),
        ],
    )
    .expect("runs batch schema")
}

fn progress_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("current_date", DataType::Utf8),
        f("trading_days", DataType::Int64),
        f("portfolio_value", DataType::Float64),
    ])
}

fn progress_batch(run_id: &str, rows: &[BacktestProgress]) -> RecordBatch {
    RecordBatch::try_new(
        progress_schema(),
        vec![
            strings(rows.iter().map(|_| run_id.to_owned())),
            strings(rows.iter().map(|r| r.current_date.to_string())),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|r| r.trading_days),
            )),
            Arc::new(Float64Array::from_iter_values(
                rows.iter().map(|r| r.portfolio_value),
            )),
        ],
    )
    .expect("progress batch schema")
}

fn orders_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("order_id", DataType::Int64),
        f("time_ns", DataType::Int64),
        f("symbol_sid", DataType::Int64),
        f("symbol", DataType::Utf8),
        f("order_type", DataType::Utf8),
        f("status", DataType::Utf8),
        f("quantity", DataType::Utf8),
        f("filled_quantity", DataType::Utf8),
        f("average_fill_price", DataType::Utf8),
        f("payload_json", DataType::Utf8),
    ])
}

fn orders_batch(run_id: &str, result: &BacktestRunResult) -> Result<RecordBatch> {
    let rows = &result.orders;
    Ok(RecordBatch::try_new(
        orders_schema(),
        vec![
            strings(rows.iter().map(|_| run_id.to_owned())),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|o| o.id))),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|o| o.time.0))),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|o| o.symbol.id.sid as i64),
            )),
            strings(rows.iter().map(|o| o.symbol.value.to_string())),
            strings(
                rows.iter()
                    .map(|o| format!("{:?}", o.order_type).to_lowercase()),
            ),
            strings(
                rows.iter()
                    .map(|o| format!("{:?}", o.status).to_lowercase()),
            ),
            strings(rows.iter().map(|o| o.quantity.to_string())),
            strings(rows.iter().map(|o| o.filled_quantity.to_string())),
            strings(rows.iter().map(|o| o.average_fill_price.to_string())),
            strings(
                rows.iter()
                    .map(serde_json::to_string)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            ),
        ],
    )?)
}

fn order_events_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("event_id", DataType::Int64),
        f("order_id", DataType::Int64),
        f("time_ns", DataType::Int64),
        f("symbol_sid", DataType::Int64),
        f("symbol", DataType::Utf8),
        f("status", DataType::Utf8),
        f("fill_price", DataType::Utf8),
        f("fill_quantity", DataType::Utf8),
        f("order_fee", DataType::Utf8),
        f("message", DataType::Utf8),
        f("payload_json", DataType::Utf8),
    ])
}

fn order_events_batch(run_id: &str, result: &BacktestRunResult) -> Result<RecordBatch> {
    let rows = &result.order_events;
    Ok(RecordBatch::try_new(
        order_events_schema(),
        vec![
            strings(rows.iter().map(|_| run_id.to_owned())),
            Arc::new(Int64Array::from_iter_values(rows.iter().map(|e| e.id))),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|e| e.order_id),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|e| e.utc_time.0),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|e| e.symbol.id.sid as i64),
            )),
            strings(rows.iter().map(|e| e.symbol.value.to_string())),
            strings(
                rows.iter()
                    .map(|e| format!("{:?}", e.status).to_lowercase()),
            ),
            strings(rows.iter().map(|e| e.fill_price.to_string())),
            strings(rows.iter().map(|e| e.fill_quantity.to_string())),
            strings(rows.iter().map(|e| e.order_fee.to_string())),
            strings(rows.iter().map(|e| e.message.clone())),
            strings(
                rows.iter()
                    .map(serde_json::to_string)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            ),
        ],
    )?)
}

fn trades_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("symbol_sid", DataType::Int64),
        f("symbol", DataType::Utf8),
        f("entry_time_ns", DataType::Int64),
        f("exit_time_ns", DataType::Int64),
        f("entry_price", DataType::Utf8),
        f("exit_price", DataType::Utf8),
        f("quantity", DataType::Utf8),
        f("pnl", DataType::Utf8),
        f("pnl_pct", DataType::Utf8),
        f("fees", DataType::Utf8),
        f("is_win", DataType::Boolean),
        f("payload_json", DataType::Utf8),
    ])
}

fn trades_batch(run_id: &str, result: &BacktestRunResult) -> Result<RecordBatch> {
    let rows = &result.trades;
    Ok(RecordBatch::try_new(
        trades_schema(),
        vec![
            strings(rows.iter().map(|_| run_id.to_owned())),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|t| t.symbol.id.sid as i64),
            )),
            strings(rows.iter().map(|t| t.symbol.value.to_string())),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|t| t.entry_time.0),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|t| t.exit_time.0),
            )),
            strings(rows.iter().map(|t| t.entry_price.to_string())),
            strings(rows.iter().map(|t| t.exit_price.to_string())),
            strings(rows.iter().map(|t| t.quantity.to_string())),
            strings(rows.iter().map(|t| t.pnl.to_string())),
            strings(rows.iter().map(|t| t.pnl_pct.to_string())),
            strings(rows.iter().map(|t| t.fees.to_string())),
            Arc::new(BooleanArray::from_iter(rows.iter().map(|t| Some(t.is_win)))),
            strings(
                rows.iter()
                    .map(serde_json::to_string)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            ),
        ],
    )?)
}

fn insights_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("insight_id", DataType::Int64),
        f("event_time_ns", DataType::Int64),
        f("event_kind", DataType::Utf8),
        f("symbol_sid", DataType::Int64),
        f("symbol", DataType::Utf8),
        f("direction", DataType::Utf8),
        f("source_model", DataType::Utf8),
        nf("weight", DataType::Utf8),
        f("payload_json", DataType::Utf8),
    ])
}

fn insights_batch(run_id: &str, result: &BacktestRunResult) -> Result<RecordBatch> {
    let rows = &result.insight_events;
    Ok(RecordBatch::try_new(
        insights_schema(),
        vec![
            strings(rows.iter().map(|_| run_id.to_owned())),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|e| e.insight.id as i64),
            )),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|e| e.event_time_utc.0),
            )),
            strings(rows.iter().map(|e| format!("{:?}", e.kind).to_lowercase())),
            Arc::new(Int64Array::from_iter_values(
                rows.iter().map(|e| e.insight.symbol.id.sid as i64),
            )),
            strings(rows.iter().map(|e| e.insight.symbol.value.to_string())),
            strings(
                rows.iter()
                    .map(|e| format!("{:?}", e.insight.direction).to_lowercase()),
            ),
            strings(rows.iter().map(|e| e.insight.source_model.to_string())),
            optional_strings(rows.iter().map(|e| e.insight.weight.map(|v| v.to_string()))),
            strings(
                rows.iter()
                    .map(serde_json::to_string)
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            ),
        ],
    )?)
}

fn statistics_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("name", DataType::Utf8),
        f("value", DataType::Utf8),
    ])
}

fn statistics_batch(run_id: &str, result: &BacktestRunResult) -> Result<RecordBatch> {
    let value = serde_json::to_value(&result.statistics)?;
    let object = value
        .as_object()
        .context("portfolio statistics must be an object")?;
    let mut rows = object
        .iter()
        .map(|(name, value)| (name.clone(), scalar(value)))
        .collect::<Vec<_>>();
    rows.extend([
        ("total_return".to_owned(), result.total_return.to_string()),
        ("final_value".to_owned(), result.final_value.to_string()),
        ("total_fees".to_owned(), result.total_fees.to_string()),
        ("total_funding".to_owned(), result.total_funding.to_string()),
        (
            "alpha_analytics".to_owned(),
            serde_json::to_string(&result.alpha_analytics)?,
        ),
    ]);
    Ok(RecordBatch::try_new(
        statistics_schema(),
        vec![
            strings(rows.iter().map(|_| run_id.to_owned())),
            strings(rows.iter().map(|r| r.0.clone())),
            strings(rows.into_iter().map(|r| r.1)),
        ],
    )?)
}

fn charts_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("chart", DataType::Utf8),
        f("series", DataType::Utf8),
        f("series_type", DataType::Utf8),
        f("time", DataType::Utf8),
        f("value", DataType::Float64),
    ])
}

fn charts_batch(run_id: &str, result: &BacktestRunResult) -> RecordBatch {
    let mut rows = Vec::new();
    for (chart_name, chart) in &result.charts.charts {
        for (series_name, series) in &chart.series {
            for point in &series.points {
                rows.push((
                    chart_name.clone(),
                    series_name.clone(),
                    format!("{:?}", series.series_type).to_lowercase(),
                    point.time.clone(),
                    point.value,
                ));
            }
        }
    }
    RecordBatch::try_new(
        charts_schema(),
        vec![
            strings(rows.iter().map(|_| run_id.to_owned())),
            strings(rows.iter().map(|r| r.0.clone())),
            strings(rows.iter().map(|r| r.1.clone())),
            strings(rows.iter().map(|r| r.2.clone())),
            strings(rows.iter().map(|r| r.3.clone())),
            Arc::new(Float64Array::from_iter_values(rows.iter().map(|r| r.4))),
        ],
    )
    .expect("charts batch schema")
}

fn data_requests_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("status", DataType::Utf8),
        f("request", DataType::Utf8),
    ])
}

fn data_requests_batch(run_id: &str, result: &BacktestRunResult) -> RecordBatch {
    let rows = result
        .succeeded_data_requests
        .iter()
        .map(|r| ("succeeded", r))
        .chain(result.failed_data_requests.iter().map(|r| ("failed", r)))
        .collect::<Vec<_>>();
    RecordBatch::try_new(
        data_requests_schema(),
        vec![
            strings(rows.iter().map(|_| run_id.to_owned())),
            strings(rows.iter().map(|r| r.0.to_owned())),
            strings(rows.iter().map(|r| r.1.clone())),
        ],
    )
    .expect("data request batch schema")
}

fn logs_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("time", DataType::Utf8),
        f("level", DataType::Utf8),
        f("message", DataType::Utf8),
    ])
}

fn summary_log_batch(run_id: &str, result: &BacktestRunResult) -> RecordBatch {
    let message = format!(
        "Backtest completed: {} trading days, final value ${:.2}, total return {:.4}%, {} orders, {} trades",
        result.trading_days, result.final_value, result.total_return * 100.0,
        result.orders.len(), result.trades.len(),
    );
    RecordBatch::try_new(
        logs_schema(),
        vec![
            strings([run_id.to_owned()]),
            strings([chrono::Utc::now().to_rfc3339()]),
            strings(["info".to_owned()]),
            strings([message]),
        ],
    )
    .expect("logs batch schema")
}

fn strings<I>(values: I) -> ArrayRef
where
    I: IntoIterator<Item = String>,
{
    Arc::new(StringArray::from_iter_values(values))
}

fn optional_strings<I>(values: I) -> ArrayRef
where
    I: IntoIterator<Item = Option<String>>,
{
    Arc::new(StringArray::from_iter(values))
}

fn optional_f64<I>(values: I) -> ArrayRef
where
    I: IntoIterator<Item = Option<f64>>,
{
    Arc::new(Float64Array::from_iter(values))
}

fn scalar(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_contracts_use_only_supported_arrow_types() {
        for (_, schema) in table_schemas() {
            let definition = definition(&schema);
            assert_eq!(definition.schema.len(), schema.fields().len());
            assert!(definition.schema.iter().all(|column| {
                matches!(
                    column.type_name.as_str(),
                    "utf8" | "int64" | "float64" | "boolean"
                )
            }));
        }
    }

    #[test]
    fn catalog_has_one_contract_for_every_backtest_result_family() {
        let names = table_schemas()
            .into_iter()
            .map(|(name, _)| name)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                RUNS,
                PROGRESS,
                ORDERS,
                ORDER_EVENTS,
                TRADES,
                INSIGHTS,
                STATISTICS,
                CHARTS,
                DATA_REQUESTS,
                LOGS
            ]
        );
    }
}
