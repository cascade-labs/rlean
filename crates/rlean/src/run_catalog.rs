//! Durable backtest results stored as typed Arrow rows in Verglas.
//!
//! A backtest process is disposable. The catalog is the source of truth for
//! run state, progress, fills, trades, insights, charts, and statistics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use futures::{future::try_join_all, stream};
use rlean_engine::{BacktestProgress, BacktestRunResult, BacktestStreamUpdate};
use serde_json::Value;
use verglas_sdk::{ColumnSpec, Database, TableDefinition};

const RUNS: &str = "rlean.runs";
const PROGRESS: &str = "rlean.run_progress";
const ORDER_EVENTS: &str = "rlean.order_events";
const TRADES: &str = "rlean.trades";
const INSIGHTS: &str = "rlean.insights";
const INSIGHT_STATE: &str = "rlean.insight_state";
const CHECKPOINTS: &str = "rlean.checkpoints";
const STATISTICS: &str = "rlean.statistics";
const CHARTS: &str = "rlean.charts";
const DATA_REQUESTS: &str = "rlean.data_requests";
const LOGS: &str = "rlean.logs";
const APPEND_ATTEMPTS: usize = 4;
// `verglas_sdk::Database::append_stream` commits each incoming RecordBatch as an
// independent bounded write. Keep the remaining terminal artifacts bounded as
// well; no result family should depend on a run-sized request body.
const APPEND_BATCH_ROWS: usize = 4_096;

/// How a failed append is treated.
///
/// Streaming rows are progress telemetry: a run that loses a progress snapshot
/// is still producing correct results, so a cache backend that will not commit
/// costs the batch, not the run. Terminal artifacts are the deliverable, so
/// their failures still surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppendFailure {
    Drop,
    Fail,
}

/// Per-run tally of telemetry batches the catalog could not commit, reported
/// once when the run finishes so a run that lost rows does not look clean.
#[derive(Default)]
struct DroppedAppends {
    batches: AtomicU64,
    rows: AtomicU64,
}

#[derive(Clone)]
pub(crate) struct RunCatalog {
    client: Database,
    run_id: String,
    process_generation: uuid::Uuid,
    strategy: String,
    started_at: chrono::DateTime<chrono::Utc>,
    parameters_json: String,
    dropped_appends: Arc<DroppedAppends>,
}

impl RunCatalog {
    pub(crate) async fn connect(
        client: Database,
        run_id: String,
        strategy: String,
        parameters: &std::collections::HashMap<String, String>,
    ) -> Result<Self> {
        let catalog = Self {
            client,
            run_id,
            process_generation: uuid::Uuid::new_v4(),
            strategy,
            started_at: chrono::Utc::now(),
            parameters_json: serde_json::to_string(parameters)?,
            dropped_appends: Arc::new(DroppedAppends::default()),
        };
        catalog.ensure_tables().await?;
        Ok(catalog)
    }

    pub(crate) fn run_id(&self) -> &str {
        &self.run_id
    }

    pub(crate) async fn record_started(&self) -> Result<()> {
        self.append(
            RUNS,
            runs_batch(self, "running", None, None),
            "started",
            AppendFailure::Fail,
        )
        .await
    }

    pub(crate) async fn record_failed(&self, error: &anyhow::Error) -> Result<()> {
        self.append(
            RUNS,
            runs_batch(self, "failed", None, Some(error.to_string())),
            "failed",
            AppendFailure::Fail,
        )
        .await
    }

    /// Terminal live row (no full BacktestRunResult statistics package).
    pub(crate) async fn record_live_stopped(
        &self,
        final_value: f64,
        slices_processed: usize,
        order_events: usize,
    ) -> Result<()> {
        self.append(
            RUNS,
            live_stopped_batch(self, final_value),
            "stopped",
            AppendFailure::Fail,
        )
        .await?;
        self.append(
            LOGS,
            live_summary_log_batch(self.run_id(), final_value, slices_processed, order_events),
            "summary-log",
            AppendFailure::Fail,
        )
        .await
    }

    pub(crate) async fn record_stream_updates(
        &self,
        updates: Vec<BacktestStreamUpdate>,
        sequence: u64,
    ) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }
        let progress = updates
            .iter()
            .filter(|update| update.record_daily_progress)
            .map(|update| update.progress.clone())
            .collect::<Vec<_>>();
        let order_events = updates
            .iter()
            .flat_map(|update| update.order_events.iter().cloned())
            .collect::<Vec<_>>();
        let trades = updates
            .iter()
            .flat_map(|update| update.trades.iter().cloned())
            .collect::<Vec<_>>();
        let insight_events = updates
            .iter()
            .flat_map(|update| update.insight_events.iter().cloned())
            .collect::<Vec<_>>();
        let checkpoints = updates
            .iter()
            .filter_map(|update| update.checkpoint_json.clone())
            .collect::<Vec<_>>();
        let insight_states = updates
            .iter()
            .filter_map(|update| update.insight_state_json.clone())
            .collect::<Vec<_>>();
        let snapshots = updates
            .iter()
            .map(|update| update.progress.clone())
            .collect::<Vec<_>>();
        let suffix = format!("stream-{sequence}");
        let order_batch = order_event_rows_batch(&self.run_id, &order_events)?;
        let trade_batch = trade_rows_batch(&self.run_id, &trades)?;
        let insight_batch = insight_event_rows_batch(&self.run_id, &insight_events)?;
        let insight_state_batch = state_payloads_batch(&self.run_id, &insight_states);
        let checkpoint_batch = checkpoints_batch(&self.run_id, &checkpoints);
        // Progress telemetry must never unwind a run that is still computing
        // correct results, so these appends drop instead of failing.
        try_join_all([
            self.append(
                RUNS,
                running_snapshots_batch(self, &snapshots),
                &suffix,
                AppendFailure::Drop,
            ),
            self.append(
                PROGRESS,
                progress_batch(&self.run_id, &progress),
                &suffix,
                AppendFailure::Drop,
            ),
            self.append(ORDER_EVENTS, order_batch, &suffix, AppendFailure::Drop),
            self.append(TRADES, trade_batch, &suffix, AppendFailure::Drop),
            self.append(INSIGHTS, insight_batch, &suffix, AppendFailure::Drop),
            self.append(
                INSIGHT_STATE,
                insight_state_batch,
                &suffix,
                AppendFailure::Fail,
            ),
            // Account state is restart truth independent from framework state.
            // Do not report success when Verglas did not commit it.
            self.append(CHECKPOINTS, checkpoint_batch, &suffix, AppendFailure::Fail),
        ])
        .await?;
        Ok(())
    }

    /// Load independent account and insight state for a live run, plus fills.
    pub(crate) async fn load_live_restore(
        client: &Database,
        run_id: &str,
    ) -> Result<Option<rlean_engine::live::catalog_state::LiveRestoreState>> {
        let escaped = run_id.replace('\'', "''");
        let checkpoint_sql = format!(
            "SELECT payload_json FROM rlean.checkpoints \
             WHERE run_id = '{escaped}' ORDER BY recorded_at DESC LIMIT 1"
        );
        let insight_state_sql = format!(
            "SELECT payload_json FROM rlean.insight_state \
             WHERE run_id = '{escaped}' ORDER BY recorded_at DESC LIMIT 1"
        );
        let checkpoint_payload = load_latest_payload(client, &checkpoint_sql).await?;
        let mut insight_payload = load_latest_payload(client, &insight_state_sql).await?;
        if insight_payload.is_none() {
            if let Some(migrated) = checkpoint_payload
                .as_deref()
                .and_then(legacy_checkpoint_insight_state)
            {
                let batch = state_payloads_batch(run_id, std::slice::from_ref(&migrated));
                append_insight_state_migration(client, run_id, batch).await?;
                insight_payload = Some(migrated);
            }
        }

        // Fills/trades are streamed into the same tables as backtests; reload
        // the full history so paper trade builders and CLI views stay consistent.
        let events_sql = format!(
            "SELECT payload_json FROM rlean.order_events WHERE run_id = '{escaped}' \
             ORDER BY time_ns ASC"
        );
        let trades_sql = format!(
            "SELECT payload_json FROM rlean.trades WHERE run_id = '{escaped}' \
             ORDER BY exit_time_ns ASC"
        );
        let order_events = load_json_payloads::<rlean_orders::OrderEvent>(client, &events_sql)
            .await
            .unwrap_or_default();
        let trades = load_json_payloads::<rlean_statistics::Trade>(client, &trades_sql)
            .await
            .unwrap_or_default();

        assemble_live_restore(
            checkpoint_payload.as_deref(),
            insight_payload.as_deref(),
            order_events,
            trades,
        )
    }

    /// One summary line at the end of a run. Silence means nothing was lost.
    pub(crate) fn log_dropped_appends_summary(&self) {
        let batches = self.dropped_appends.batches.load(Ordering::Relaxed);
        if batches == 0 {
            return;
        }
        tracing::error!(
            run_id = %self.run_id,
            dropped_batches = batches,
            dropped_rows = self.dropped_appends.rows.load(Ordering::Relaxed),
            "streaming run telemetry was dropped during this run; progress, order events, and \
             trades in the catalog are incomplete"
        );
    }

    pub(crate) async fn record_completed(&self, result: &BacktestRunResult) -> Result<()> {
        // Terminal artifacts are the deliverable, not telemetry: a failure to
        // persist them still surfaces as an error.
        let artifacts = vec![
            (
                INSIGHTS,
                insight_event_rows_batch(&self.run_id, &result.insight_events)?,
                "insights",
            ),
            (
                STATISTICS,
                statistics_batch(&self.run_id, result)?,
                "statistics",
            ),
            (CHARTS, charts_batch(&self.run_id, result), "charts"),
            (
                DATA_REQUESTS,
                data_requests_batch(&self.run_id, result),
                "data-requests",
            ),
            (LOGS, summary_log_batch(&self.run_id, result), "summary-log"),
        ];
        try_join_all(
            artifacts.into_iter().map(|(table, batch, suffix)| {
                self.append(table, batch, suffix, AppendFailure::Fail)
            }),
        )
        .await?;
        // Publish the completed run row only after every dependent artifact is
        // durable. The independent table writes above do not need to serialize.
        self.append(
            RUNS,
            runs_batch(self, "completed", Some(result), None),
            "completed",
            AppendFailure::Fail,
        )
        .await
    }

    async fn append(
        &self,
        table: &str,
        batch: RecordBatch,
        suffix: &str,
        on_failure: AppendFailure,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        // A deployment ID survives process restarts. Include this process
        // generation so sequence zero after a restart cannot be mistaken for
        // sequence zero from the prior process. Retries within this process
        // still reuse the same key and remain idempotent.
        let idempotency_key = append_idempotency_key(&self.run_id, self.process_generation, suffix);
        let mut delay = Duration::from_secs(2);
        for attempt in 1..=APPEND_ATTEMPTS {
            match self
                .client
                .append_stream(
                    table,
                    stream::iter(record_batch_chunks(&batch).map(Ok)),
                    &idempotency_key,
                )
                .await
            {
                Ok(_) => return Ok(()),
                Err(error) if attempt < APPEND_ATTEMPTS => {
                    let retry_delay = retry_after_from_error(&error).unwrap_or(delay);
                    tracing::warn!(
                        table,
                        attempt,
                        max_attempts = APPEND_ATTEMPTS,
                        retry_after_seconds = retry_delay.as_secs(),
                        error = %error,
                        "retrying failed backtest catalog append"
                    );
                    tokio::time::sleep(retry_delay).await;
                    delay = delay.saturating_mul(2);
                }
                Err(error) if on_failure == AppendFailure::Drop => {
                    tracing::error!(
                        table,
                        attempts = attempt,
                        rows = batch.num_rows(),
                        error = %error,
                        "dropping run telemetry the catalog would not commit; the backtest \
                         continues and its terminal results are unaffected"
                    );
                    self.dropped_appends.batches.fetch_add(1, Ordering::Relaxed);
                    self.dropped_appends
                        .rows
                        .fetch_add(batch.num_rows() as u64, Ordering::Relaxed);
                    return Ok(());
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("append backtest results to {table}"));
                }
            }
        }
        unreachable!("backtest catalog append retry loop always returns")
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

/// Extracts the pre-`rlean.insight_state` snapshot for one-time migration.
/// New account checkpoints never contain this field.
fn legacy_checkpoint_insight_state(payload: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    let insights = value.get("insights")?;
    if insights.is_null() {
        return None;
    }
    serde_json::to_string(insights).ok()
}

fn assemble_live_restore(
    checkpoint_payload: Option<&str>,
    insight_payload: Option<&str>,
    order_events: Vec<rlean_orders::OrderEvent>,
    trades: Vec<rlean_statistics::Trade>,
) -> Result<Option<rlean_engine::live::catalog_state::LiveRestoreState>> {
    use rlean_engine::live::catalog_state::{
        account_state_from_checkpoint, parse_checkpoint_json, LiveRestoreState,
    };

    let account_state = checkpoint_payload
        .and_then(parse_checkpoint_json)
        .as_ref()
        .and_then(account_state_from_checkpoint);
    let insights = insight_payload
        .map(serde_json::from_str)
        .transpose()
        .context("parse live insight state")?;
    if account_state.is_none() && insights.is_none() && order_events.is_empty() && trades.is_empty()
    {
        return Ok(None);
    }

    Ok(Some(LiveRestoreState {
        account_state,
        order_events,
        trades,
        insights,
    }))
}

fn append_idempotency_key(run_id: &str, process_generation: uuid::Uuid, suffix: &str) -> String {
    format!("{run_id}:{process_generation}:{suffix}")
}

fn record_batch_chunks(batch: &RecordBatch) -> impl Iterator<Item = RecordBatch> + '_ {
    (0..batch.num_rows())
        .step_by(APPEND_BATCH_ROWS)
        .map(|offset| batch.slice(offset, APPEND_BATCH_ROWS.min(batch.num_rows() - offset)))
}

fn retry_after_from_error(error: &impl std::fmt::Display) -> Option<Duration> {
    let message = error.to_string();
    let value = message
        .split_once("\"retry-after\": \"")?
        .1
        .split_once('"')?
        .0
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(value))
}

fn running_snapshots_batch(catalog: &RunCatalog, rows: &[BacktestProgress]) -> RecordBatch {
    RecordBatch::try_new(
        runs_schema(),
        vec![
            strings(rows.iter().map(|_| catalog.run_id.clone())),
            strings(rows.iter().map(|_| catalog.strategy.clone())),
            strings(rows.iter().map(|_| "running".to_owned())),
            strings(rows.iter().map(|_| chrono::Utc::now().to_rfc3339())),
            strings(rows.iter().map(|_| catalog.started_at.to_rfc3339())),
            optional_strings(rows.iter().map(|_| None)),
            optional_strings(rows.iter().map(|row| Some(row.start_date.to_string()))),
            optional_strings(rows.iter().map(|row| Some(row.end_date.to_string()))),
            optional_f64(rows.iter().map(|row| Some(row.starting_cash))),
            optional_f64(rows.iter().map(|row| Some(row.portfolio_value))),
            optional_f64(rows.iter().map(|row| {
                (row.starting_cash != 0.0).then_some(row.portfolio_value / row.starting_cash - 1.0)
            })),
            optional_f64(rows.iter().map(|_| None)),
            optional_strings(rows.iter().map(|_| None)),
            strings(rows.iter().map(|_| catalog.parameters_json.clone())),
        ],
    )
    .expect("running snapshots schema")
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
        (ORDER_EVENTS, order_events_schema()),
        (TRADES, trades_schema()),
        (INSIGHTS, insights_schema()),
        (INSIGHT_STATE, state_payloads_schema()),
        (CHECKPOINTS, checkpoints_schema()),
        (STATISTICS, statistics_schema()),
        (CHARTS, charts_schema()),
        (DATA_REQUESTS, data_requests_schema()),
        (LOGS, logs_schema()),
    ]
}

async fn load_json_payloads<T: serde::de::DeserializeOwned>(
    client: &Database,
    sql: &str,
) -> Result<Vec<T>> {
    use futures::TryStreamExt;
    let batches = client
        .query_stream(sql)
        .await
        .with_context(|| format!("query {sql}"))?
        .try_collect::<Vec<_>>()
        .await
        .with_context(|| format!("read query stream for {sql}"))?;
    let mut out = Vec::new();
    for batch in batches {
        let col = batch.column(0);
        for row in 0..batch.num_rows() {
            if col.is_null(row) {
                continue;
            }
            let text = arrow_cast::display::array_value_to_string(col.as_ref(), row)?;
            if let Ok(value) = serde_json::from_str::<T>(&text) {
                out.push(value);
            }
        }
    }
    Ok(out)
}

async fn load_latest_payload(client: &Database, sql: &str) -> Result<Option<String>> {
    use futures::TryStreamExt;
    let mut delay = Duration::from_secs(1);
    let mut attempt = 1_u32;
    let batches = loop {
        let result = match client.query_stream(sql).await {
            Ok(stream) => stream
                .try_collect::<Vec<_>>()
                .await
                .with_context(|| format!("read query stream for {sql}")),
            Err(error) => Err(anyhow::Error::new(error).context(format!("query {sql}"))),
        };
        match result {
            Ok(batches) => break batches,
            Err(error) if attempt < 8 && is_retryable_catalog_state_error(&error) => {
                tracing::warn!(%error, attempt, max_attempts = 8, retry_after_seconds = delay.as_secs(), "retrying live state query");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(8));
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    };
    Ok(batches.iter().find_map(|batch| {
        let column = batch.column(0);
        (0..batch.num_rows()).find_map(|row| {
            if column.is_null(row) {
                None
            } else {
                arrow_cast::display::array_value_to_string(column.as_ref(), row).ok()
            }
        })
    }))
}

async fn append_insight_state_migration(
    client: &Database,
    run_id: &str,
    batch: RecordBatch,
) -> Result<()> {
    let mut delay = Duration::from_secs(1);
    let mut attempt = 1_u32;
    loop {
        match client
            .append_stream(
                INSIGHT_STATE,
                stream::iter([Ok(batch.clone())]),
                &format!("migrate-legacy-insight-state-{run_id}"),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(error) if attempt < 8 && is_retryable_catalog_state_error(&error) => {
                tracing::warn!(%error, attempt, max_attempts = 8, retry_after_seconds = delay.as_secs(), "retrying legacy insight-state migration");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(8));
                attempt += 1;
            }
            Err(error) => {
                return Err(error).context("migrate legacy live insight state");
            }
        }
    }
}

fn is_retryable_catalog_state_error(error: &impl std::fmt::Display) -> bool {
    let message = error.to_string();
    message.contains("catalog changed while rebuilding the query session")
        || message.contains("authorization service unavailable")
        || message.contains("503 Service Unavailable")
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

fn order_event_rows_batch(run_id: &str, rows: &[rlean_orders::OrderEvent]) -> Result<RecordBatch> {
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

fn trade_rows_batch(run_id: &str, rows: &[rlean_statistics::Trade]) -> Result<RecordBatch> {
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

fn insight_event_rows_batch(
    run_id: &str,
    rows: &[rlean_alpha::InsightEvent],
) -> Result<RecordBatch> {
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

fn checkpoints_schema() -> SchemaRef {
    state_payloads_schema()
}

fn state_payloads_schema() -> SchemaRef {
    schema(vec![
        f("run_id", DataType::Utf8),
        f("recorded_at", DataType::Utf8),
        f("payload_json", DataType::Utf8),
    ])
}

fn checkpoints_batch(run_id: &str, payloads: &[String]) -> RecordBatch {
    state_payloads_batch(run_id, payloads)
}

fn state_payloads_batch(run_id: &str, payloads: &[String]) -> RecordBatch {
    let recorded_at = chrono::Utc::now().to_rfc3339();
    RecordBatch::try_new(
        state_payloads_schema(),
        vec![
            strings(payloads.iter().map(|_| run_id.to_owned())),
            strings(payloads.iter().map(|_| recorded_at.clone())),
            strings(payloads.iter().cloned()),
        ],
    )
    .expect("checkpoints batch schema")
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

fn live_stopped_batch(catalog: &RunCatalog, final_value: f64) -> RecordBatch {
    RecordBatch::try_new(
        runs_schema(),
        vec![
            strings([catalog.run_id.clone()]),
            strings([catalog.strategy.clone()]),
            strings(["stopped".to_owned()]),
            strings([chrono::Utc::now().to_rfc3339()]),
            strings([catalog.started_at.to_rfc3339()]),
            optional_strings([Some(chrono::Utc::now().to_rfc3339())]),
            optional_strings([Some(catalog.started_at.date_naive().to_string())]),
            optional_strings([Some(chrono::Utc::now().date_naive().to_string())]),
            optional_f64([None]),
            optional_f64([Some(final_value)]),
            optional_f64([None]),
            optional_f64([None]),
            optional_strings([None]),
            strings([catalog.parameters_json.clone()]),
        ],
    )
    .expect("live stopped batch schema")
}

fn live_summary_log_batch(
    run_id: &str,
    final_value: f64,
    slices_processed: usize,
    order_events: usize,
) -> RecordBatch {
    let message = format!(
        "Live stopped: {slices_processed} slices, final value ${final_value:.2}, {order_events} order events"
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
    .expect("live logs batch schema")
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
                ORDER_EVENTS,
                TRADES,
                INSIGHTS,
                INSIGHT_STATE,
                CHECKPOINTS,
                STATISTICS,
                CHARTS,
                DATA_REQUESTS,
                LOGS
            ]
        );
    }

    #[test]
    fn insight_state_has_a_dedicated_catalog_contract() {
        let tables = table_schemas();
        let insight_state = tables
            .iter()
            .find(|(name, _)| *name == INSIGHT_STATE)
            .expect("dedicated insight state table");
        let checkpoint = tables
            .iter()
            .find(|(name, _)| *name == CHECKPOINTS)
            .expect("account checkpoint table");

        assert_ne!(insight_state.0, checkpoint.0);
        assert_eq!(
            insight_state
                .1
                .fields()
                .iter()
                .map(|field| field.name().as_str())
                .collect::<Vec<_>>(),
            vec!["run_id", "recorded_at", "payload_json"]
        );
    }

    #[test]
    fn insight_restore_does_not_require_an_account_checkpoint() {
        let now = rlean_core::DateTime::from_secs(2_000_000);
        let insight = rlean_alpha::Insight::up(
            rlean_core::Symbol::create_equity("SPY", &rlean_core::Market::usa()),
            rlean_core::TimeSpan::ONE_DAY,
        )
        .with_generated_time_utc(now);
        let state = rlean_engine::live::catalog_state::LiveInsightSnapshot {
            schema_version: rlean_engine::live::catalog_state::LIVE_INSIGHTS_SCHEMA_VERSION,
            deployment_id: Some("live-test".to_string()),
            written_at: "2026-08-12T00:00:00Z".to_string(),
            state: rlean_alpha::InsightCollectionSnapshot {
                active: vec![insight.clone()],
                closed: Vec::new(),
                total_count: 1,
            },
        };
        let state_json = serde_json::to_string(&state).unwrap();

        let restore = assemble_live_restore(None, Some(&state_json), Vec::new(), Vec::new())
            .unwrap()
            .expect("insight-only restore state");

        assert!(restore.account_state.is_none());
        let restored_insights = restore.insights.expect("restored insight state");
        assert_eq!(restored_insights.state.active.len(), 1);
        assert_eq!(restored_insights.state.active[0].id, insight.id);
    }

    #[test]
    fn legacy_checkpoint_insights_are_extracted_for_one_time_migration() {
        let payload = serde_json::json!({
            "schema_version": 1,
            "portfolio": {"cash": "100000"},
            "open_orders": [],
            "insights": {
                "schema_version": 1,
                "deployment_id": "live-test",
                "written_at": "2026-08-12T00:00:00Z",
                "state": {"active": [], "closed": [], "total_count": 0}
            }
        })
        .to_string();

        let migrated = legacy_checkpoint_insight_state(&payload).expect("legacy insight state");
        let decoded: rlean_engine::live::catalog_state::LiveInsightSnapshot =
            serde_json::from_str(&migrated).unwrap();

        assert_eq!(decoded.deployment_id.as_deref(), Some("live-test"));
        assert_eq!(decoded.state.total_count, 0);
    }

    #[test]
    fn current_account_checkpoint_has_no_legacy_insight_state() {
        let payload = serde_json::json!({
            "schema_version": 1,
            "portfolio": {"cash": "100000"},
            "open_orders": []
        })
        .to_string();

        assert!(legacy_checkpoint_insight_state(&payload).is_none());
    }

    #[test]
    fn live_state_retry_classifies_catalog_rebuild_and_auth_unavailable() {
        assert!(is_retryable_catalog_state_error(
            &"catalog changed while rebuilding the query session; retry the query"
        ));
        assert!(is_retryable_catalog_state_error(
            &"503 Service Unavailable: authorization service unavailable"
        ));
        assert!(!is_retryable_catalog_state_error(
            &"400 Bad Request: unknown column"
        ));
    }

    #[test]
    fn append_stream_batches_are_bounded_without_losing_rows() {
        let batch = RecordBatch::try_new(
            schema(vec![f("value", DataType::Utf8)]),
            vec![strings((0..8_500).map(|value| value.to_string()))],
        )
        .unwrap();
        let chunks = record_batch_chunks(&batch).collect::<Vec<_>>();
        assert_eq!(
            chunks.iter().map(RecordBatch::num_rows).sum::<usize>(),
            8_500
        );
        assert_eq!(chunks.len(), 3);
        assert!(chunks
            .iter()
            .all(|chunk| chunk.num_rows() <= APPEND_BATCH_ROWS));
    }

    #[test]
    fn catalog_retry_honors_embedded_retry_after_header() {
        let error = "status: 429 Too Many Requests, headers: {\"retry-after\": \"60\"}";
        assert_eq!(
            retry_after_from_error(&error),
            Some(Duration::from_secs(60))
        );
    }

    #[test]
    fn append_keys_are_stable_for_retries_and_distinct_across_restarts() {
        let first_process = uuid::Uuid::new_v4();
        let second_process = uuid::Uuid::new_v4();
        let first_attempt = append_idempotency_key("live-strategy", first_process, "stream-0");
        let retry = append_idempotency_key("live-strategy", first_process, "stream-0");
        let restarted = append_idempotency_key("live-strategy", second_process, "stream-0");

        assert_eq!(first_attempt, retry);
        assert_ne!(first_attempt, restarted);
    }
}
