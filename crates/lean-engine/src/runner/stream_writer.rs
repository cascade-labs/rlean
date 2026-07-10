//! Incremental backtest result streaming.
//!
//! Writes progress/order/trade sidecar files to the backtest output directory
//! *while the backtest is still running*, so external tools can tail live
//! results instead of waiting for the batch files that `report.rs` emits at the
//! end.
//!
//! This restores the streaming behavior that existed in the pre-SDK-migration
//! Python runner (`trades.jsonl`, `order-events.jsonl`, `progress.json`,
//! `heartbeat.log`). It intentionally mirrors the live path's
//! `LiveDeploymentWriter` append/atomic-write helpers, but tracks backtest
//! progress (`current_date` / `progress_percent`) rather than wall-clock live
//! state.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::NaiveDate;
use lean_orders::OrderEvent;
use lean_statistics::Trade;
use rust_decimal::Decimal;

use crate::artifacts::RunArtifactSink;

/// Streams incremental backtest results to sidecar files in the output dir.
///
/// Files are written into the sink's working dir. When the sink mirrors to S3
/// (mode `s3`/`mirror`), each write is followed by a throttled checkpoint upload;
/// [`BacktestStreamWriter::finish`] on completion uploads the final state so a
/// run that dies mid-way still leaves its last checkpoint in S3.
pub struct BacktestStreamWriter {
    sink: Arc<RunArtifactSink>,
    dir: PathBuf,
    progress_path: PathBuf,
    order_events_path: PathBuf,
    trades_path: PathBuf,
    heartbeat_path: PathBuf,
    start_date: NaiveDate,
    end_date: NaiveDate,
    started_at: chrono::DateTime<chrono::Utc>,
    last_log_date: Option<NaiveDate>,
    last_heartbeat: Instant,
}

impl BacktestStreamWriter {
    /// Create sidecar files in the sink's working dir. Truncates any
    /// pre-existing streaming files so a re-run starts clean.
    pub fn new(sink: Arc<RunArtifactSink>, start_date: NaiveDate, end_date: NaiveDate) -> Self {
        let dir = sink.working_dir().to_path_buf();
        let _ = std::fs::create_dir_all(&dir);
        let writer = Self {
            progress_path: dir.join("progress.json"),
            order_events_path: dir.join("order-events.jsonl"),
            trades_path: dir.join("trades.jsonl"),
            heartbeat_path: dir.join("heartbeat.log"),
            dir,
            sink,
            start_date,
            end_date,
            started_at: chrono::Utc::now(),
            last_log_date: None,
            // Force the first progress call to emit a heartbeat immediately.
            last_heartbeat: Instant::now()
                .checked_sub(Duration::from_secs(60))
                .unwrap_or_else(Instant::now),
        };
        // Truncate append-only streams so a re-run into the same dir is clean.
        let _ = std::fs::File::create(&writer.order_events_path);
        let _ = std::fs::File::create(&writer.trades_path);
        let _ = std::fs::File::create(&writer.heartbeat_path);
        writer
    }

    /// The working directory files are written into (the sink's working dir).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn progress_fraction(&self, current_date: NaiveDate) -> f64 {
        let total = (self.end_date - self.start_date).num_days().max(1) as f64;
        let done = (current_date - self.start_date).num_days().max(0) as f64;
        (done / total).clamp(0.0, 1.0)
    }

    /// Append newly generated order events to `order-events.jsonl`.
    pub fn append_order_events(&self, events: &[OrderEvent]) {
        append_json_lines(&self.order_events_path, events);
    }

    /// Append newly completed trades to `trades.jsonl`.
    pub fn append_trades(&self, trades: &[Trade]) {
        append_json_lines(&self.trades_path, trades);
    }

    /// Rewrite `progress.json` and (at most every 30s) append to `heartbeat.log`.
    pub fn record_progress(
        &mut self,
        current_date: NaiveDate,
        trading_days: i64,
        portfolio_value: Decimal,
        order_events: usize,
        trades: usize,
    ) {
        let progress = self.progress_fraction(current_date);
        let payload = serde_json::json!({
            "status": "running",
            "current_date": current_date.to_string(),
            "start_date": self.start_date.to_string(),
            "end_date": self.end_date.to_string(),
            "progress": progress,
            "progress_percent": (progress * 100.0),
            "trading_days": trading_days,
            "portfolio_value": portfolio_value.to_string(),
            "order_events": order_events,
            "trades": trades,
            "started_at": self.started_at.to_rfc3339(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
        });
        write_json_pretty_atomic(&self.progress_path, &payload);

        if self.last_log_date != Some(current_date) {
            tracing::info!(
                "Backtest progress: {} ({:.1}%) trading_days={} portfolio={} orders={} trades={} output={}",
                current_date,
                progress * 100.0,
                trading_days,
                portfolio_value,
                order_events,
                trades,
                self.dir.display()
            );
            self.last_log_date = Some(current_date);
        }

        if self.last_heartbeat.elapsed() >= Duration::from_secs(30) {
            self.append_heartbeat(current_date, progress, trading_days, portfolio_value);
            self.last_heartbeat = Instant::now();
        }

        // Checkpoint the streaming files to S3. The sink throttles these to at
        // most one upload per file per checkpoint interval, so calling on every
        // progress flush is cheap and no-ops entirely in local mode.
        self.sink.mirror("progress.json");
        self.sink.mirror("order-events.jsonl");
        self.sink.mirror("trades.jsonl");
        self.sink.mirror("heartbeat.log");
    }

    fn append_heartbeat(
        &self,
        current_date: NaiveDate,
        progress: f64,
        trading_days: i64,
        portfolio_value: Decimal,
    ) {
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.heartbeat_path)
        {
            let _ = writeln!(
                file,
                "{} current_date={} progress={:.3} trading_days={} portfolio={}",
                chrono::Utc::now().to_rfc3339(),
                current_date,
                progress,
                trading_days,
                portfolio_value
            );
        }
    }

    /// Flip `progress.json` to a terminal `completed` state after the run loop.
    pub fn mark_completed(
        &self,
        trading_days: i64,
        portfolio_value: Decimal,
        order_events: usize,
        trades: usize,
    ) {
        let payload = serde_json::json!({
            "status": "completed",
            "current_date": self.end_date.to_string(),
            "start_date": self.start_date.to_string(),
            "end_date": self.end_date.to_string(),
            "progress": 1.0,
            "progress_percent": 100.0,
            "trading_days": trading_days,
            "portfolio_value": portfolio_value.to_string(),
            "order_events": order_events,
            "trades": trades,
            "started_at": self.started_at.to_rfc3339(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
        });
        write_json_pretty_atomic(&self.progress_path, &payload);
    }

    /// Upload the final run state to S3 (no-op in local mode). Call after all
    /// report files have been written into the run dir so the S3 copy includes
    /// them. In `s3`-only mode this also deletes the local working buffer when
    /// the writer is dropped.
    pub fn finish(&self) {
        self.sink.flush_all();
    }

    /// Clone the artifact sink handle so the CLI can flush again after it writes
    /// the final report files into the run dir.
    pub fn sink(&self) -> Arc<RunArtifactSink> {
        self.sink.clone()
    }
}

fn write_json_pretty_atomic<T: serde::Serialize>(path: &Path, value: &T) {
    let tmp = path.with_extension("json.tmp");
    if let Ok(json) = serde_json::to_string_pretty(value) {
        let _ = std::fs::write(&tmp, json);
        let _ = std::fs::rename(&tmp, path);
    }
}

fn append_json_lines<T: serde::Serialize>(path: &Path, values: &[T]) {
    if values.is_empty() {
        return;
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        for value in values {
            if let Ok(line) = serde_json::to_string(value) {
                let _ = writeln!(file, "{line}");
            }
        }
    }
}
