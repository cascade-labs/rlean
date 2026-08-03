use crate::framework::FrameworkState;
use anyhow::Context;
use rlean_algorithm::{portfolio::SecurityPortfolioManager, qc_algorithm::QcAlgorithm};
use rlean_alpha::InsightCollectionSnapshot;
use rlean_core::{DateTime, Resolution, Symbol};
use rlean_live::AccountState;
use rlean_orders::order::Order;
use rlean_orders::order_processor::OrderProcessor;
use rlean_orders::{OrderEvent, TransactionManager};
use rlean_statistics::Trade;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::warn;

pub const LIVE_INSIGHTS_SCHEMA_VERSION: u32 = 1;

/// Debounce for state-change S3 mirrors. A burst of fills (or insight events)
/// inside this window coalesces into one upload after the burst quiesces; the
/// dirty files are uploaded by the first snapshot pass that runs after the
/// window (the live loop polls every ~250ms, snapshots per data slice).
pub const MIRROR_DEBOUNCE: Duration = Duration::from_secs(3);

/// Restart-recovery artifacts mirrored when order events / trades changed
/// since the last mirror (debounced by [`MIRROR_DEBOUNCE`]).
const ORDER_STATE_FILES: [&str; 5] = [
    "portfolio.json",
    "progress.json",
    "orders.json",
    "order-events.jsonl",
    "trades.jsonl",
];

/// Insight-derived artifacts mirrored when the insight set changed — i.e. the
/// framework drained insight events (emit/expire/close/restore) — debounced by
/// [`MIRROR_DEBOUNCE`]. alpha-analytics.json belongs here: it is computed from
/// closed insights, so it only changes when the insight set does.
const INSIGHT_STATE_FILES: [&str; 3] = [
    "insights.json",
    "insight-events.jsonl",
    "alpha-analytics.json",
];

/// PnL/status artifacts mirrored once at process start, once at each calendar
/// day rollover (first snapshot of a new day, by slice time), and by the clean
/// shutdown flush — never per slice.
const EOD_FILES: [&str; 3] = ["portfolio.json", "progress.json", "heartbeat.log"];

/// Tracks what has been mirrored so `record_snapshot` only enqueues S3 uploads
/// on state changes, day rollover, and process start — a quiet live instance
/// enqueues nothing.
struct MirrorPolicy {
    /// Order-event / trade counts as of the last time the order artifacts were
    /// marked for upload.
    order_events_seen: usize,
    trades_seen: usize,
    /// Calendar day (from slice time) of the last EOD-bucket mirror. `None`
    /// until the process-start snapshot, which mirrors immediately.
    last_eod_day: Option<chrono::NaiveDate>,
    /// Files awaiting a debounced upload, keyed by name, with the time of the
    /// most recent change (trailing debounce: new changes push the upload out).
    dirty: HashMap<&'static str, Instant>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LiveInsightSnapshot {
    pub schema_version: u32,
    pub deployment_id: Option<String>,
    pub written_at: String,
    pub state: InsightCollectionSnapshot,
}

#[derive(Debug, Clone)]
pub struct DeployRestore {
    pub account_state: AccountState,
    pub order_events: Vec<OrderEvent>,
    pub trades: Vec<Trade>,
}

pub struct LiveSnapshotCounts {
    pub slices_processed: usize,
    pub order_events: usize,
    pub trades: usize,
}

pub struct LiveDeploymentWriter {
    dir: PathBuf,
    progress_path: PathBuf,
    portfolio_path: PathBuf,
    orders_path: PathBuf,
    order_events_path: PathBuf,
    trades_path: PathBuf,
    insights_path: PathBuf,
    insight_events_path: PathBuf,
    alpha_analytics_path: PathBuf,
    heartbeat_path: PathBuf,
    started_at: chrono::DateTime<chrono::Utc>,
    last_heartbeat: Mutex<Instant>,
    /// Optional artifact sink. When set, snapshot writes are mirrored to S3
    /// asynchronously per [`MirrorPolicy`]: order/insight artifacts on state
    /// change (debounced), PnL/status artifacts at start / day rollover /
    /// shutdown. Local file writes are unconditional; only the S3 enqueue is
    /// gated, and the sink itself never blocks the live loop.
    sink: Option<Arc<crate::artifacts::RunArtifactSink>>,
    mirror_policy: Mutex<MirrorPolicy>,
}

impl LiveDeploymentWriter {
    /// Local-only writer: writes deployment files into `dir`, no S3 mirroring.
    pub fn new(dir: PathBuf) -> Self {
        let sink = Arc::new(crate::artifacts::RunArtifactSink::local(dir));
        Self::with_sink(sink)
    }

    /// Build a writer whose files live in `sink.working_dir()` and are mirrored
    /// to S3 per the sink's mode. For live, the working dir is always the real
    /// deploy dir (the live control surface reads it locally).
    pub fn with_sink(sink: Arc<crate::artifacts::RunArtifactSink>) -> Self {
        let dir = sink.working_dir().to_path_buf();
        let _ = std::fs::create_dir_all(&dir);
        let writer = Self {
            progress_path: dir.join("progress.json"),
            portfolio_path: dir.join("portfolio.json"),
            orders_path: dir.join("orders.json"),
            order_events_path: dir.join("order-events.jsonl"),
            trades_path: dir.join("trades.jsonl"),
            insights_path: dir.join("insights.json"),
            insight_events_path: dir.join("insight-events.jsonl"),
            alpha_analytics_path: dir.join("alpha-analytics.json"),
            heartbeat_path: dir.join("heartbeat.log"),
            dir,
            started_at: chrono::Utc::now(),
            last_heartbeat: Mutex::new(Instant::now() - Duration::from_secs(60)),
            sink: Some(sink),
            mirror_policy: Mutex::new(MirrorPolicy {
                order_events_seen: 0,
                trades_seen: 0,
                last_eod_day: None,
                dirty: HashMap::new(),
            }),
        };
        let ensure_exists = |path: &Path| {
            let _ = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path);
        };
        ensure_exists(&writer.order_events_path);
        ensure_exists(&writer.trades_path);
        ensure_exists(&writer.insight_events_path);
        let _ = std::fs::File::create(&writer.heartbeat_path);
        writer
    }

    fn mirror(&self, file_name: &str) {
        if let Some(sink) = &self.sink {
            sink.mirror(file_name);
        }
    }

    /// Flush any queued S3 uploads. Call on clean shutdown so the live loop's
    /// final snapshot lands in S3.
    pub fn flush(&self) {
        if let Some(sink) = &self.sink {
            sink.flush_all();
        }
    }

    pub fn append_order_events(&self, events: &[OrderEvent]) {
        append_json_lines(&self.order_events_path, events);
    }

    pub fn append_trades(&self, trades: &[Trade]) {
        append_json_lines(&self.trades_path, trades);
    }

    pub fn record_snapshot(
        &self,
        time: DateTime,
        portfolio: &SecurityPortfolioManager,
        order_processor: &OrderProcessor,
        framework: Option<&Arc<Mutex<FrameworkState>>>,
        counts: LiveSnapshotCounts,
    ) {
        self.record_snapshot_from_transactions(
            time,
            portfolio,
            order_processor.transaction_manager.as_ref(),
            framework,
            counts,
        );
    }

    /// Persist the authoritative live state after brokerage-originated order
    /// events, even when no market-data slice is available. C# LEAN's live
    /// result handler consumes transaction events independently and refreshes
    /// its portfolio/result state from that path; sparse Daily strategies must
    /// not leave their restart artifacts at the pre-fill account snapshot.
    pub(crate) fn record_snapshot_from_transactions(
        &self,
        time: DateTime,
        portfolio: &SecurityPortfolioManager,
        transactions: &TransactionManager,
        framework: Option<&Arc<Mutex<FrameworkState>>>,
        counts: LiveSnapshotCounts,
    ) {
        let updated_at = chrono::Utc::now().to_rfc3339();
        let holdings = portfolio
            .all_holdings()
            .into_iter()
            // Custom-data (Base) subscriptions are not positions. They should
            // never have been inserted into the holdings map, but filter any
            // non-invested Base entry here as defense-in-depth so snapshots from
            // older state files never surface a custom feed as a holding.
            .filter(|holding| {
                holding.symbol.security_type() != rlean_core::SecurityType::Base
                    || holding.is_invested()
            })
            .map(|holding| {
                serde_json::json!({
                    "symbol": holding.symbol.value,
                    "symbol_id": holding.symbol,
                    "sid": holding.symbol.id.sid,
                    "market": holding.symbol.id.market.as_str(),
                    "security_type": holding.symbol.security_type().to_string(),
                    "quantity": holding.quantity.to_string(),
                    "average_price": holding.average_price.to_string(),
                    "last_price": holding.last_price.to_string(),
                    "market_value": holding.market_value().to_string(),
                    "portfolio_value_contribution": holding.portfolio_value_contribution().to_string(),
                    "unrealized_pnl": holding.unrealized_pnl.to_string(),
                    "realized_pnl": holding.realized_pnl.to_string(),
                    "total_fees": holding.total_fees.to_string(),
                    "total_funding": holding.total_funding.to_string(),
                    "contract_multiplier": holding.contract_multiplier.to_string(),
                    "invested": holding.is_invested(),
                })
            })
            .collect::<Vec<_>>();

        let portfolio_payload = serde_json::json!({
            "status": "running",
            "time": time.to_string(),
            "updated_at": updated_at,
            "started_at": self.started_at.to_rfc3339(),
            "cash": portfolio.cash.read().to_string(),
            "starting_cash": portfolio.starting_cash().to_string(),
            "total_portfolio_value": portfolio.total_portfolio_value().to_string(),
            "total_holdings_value": portfolio.total_holdings_value().to_string(),
            "unrealized_pnl": portfolio.unrealized_profit().to_string(),
            "total_return": portfolio.total_return_pct().to_string(),
            "total_fees": portfolio.total_fees.read().to_string(),
            "total_funding": portfolio.total_funding.read().to_string(),
            "holdings": holdings,
        });
        write_json_pretty_atomic(&self.portfolio_path, &portfolio_payload);

        let orders = transactions.get_all_orders();
        let orders_payload = serde_json::json!({
            "status": "running",
            "time": time.to_string(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
            "count": orders.len(),
            "orders": orders,
        });
        write_json_pretty_atomic(&self.orders_path, &orders_payload);

        let progress_payload = serde_json::json!({
            "status": "running",
            "time": time.to_string(),
            "updated_at": chrono::Utc::now().to_rfc3339(),
            "started_at": self.started_at.to_rfc3339(),
            "slices_processed": counts.slices_processed,
            "portfolio_value": portfolio.total_portfolio_value().to_string(),
            "order_events": counts.order_events,
            "trades": counts.trades,
            "output": self.dir,
        });
        write_json_pretty_atomic(&self.progress_path, &progress_payload);
        let insights_changed = self.record_insight_snapshot(framework);
        self.append_heartbeat(
            time,
            portfolio,
            counts.slices_processed,
            counts.order_events,
            counts.trades,
        );

        // Local writes above are unconditional; S3 mirroring is gated so a
        // quiet live instance enqueues nothing (see MirrorPolicy).
        self.apply_mirror_policy(time, &counts, insights_changed);
    }

    /// Decide which of this snapshot's files to enqueue for S3 upload.
    ///
    /// - Order/trade or insight state changes mark their file buckets dirty;
    ///   dirty files upload once the change has quiesced for [`MIRROR_DEBOUNCE`]
    ///   (checked on each snapshot pass, so a fill burst becomes one upload).
    /// - The PnL/status bucket uploads immediately on the first snapshot of a
    ///   new calendar day (slice time) and on the very first snapshot (process
    ///   start). Clean shutdown uploads everything via `flush`.
    fn apply_mirror_policy(
        &self,
        time: DateTime,
        counts: &LiveSnapshotCounts,
        insights_changed: bool,
    ) {
        let Ok(mut policy) = self.mirror_policy.lock() else {
            return;
        };
        let now = Instant::now();

        if counts.order_events != policy.order_events_seen || counts.trades != policy.trades_seen {
            policy.order_events_seen = counts.order_events;
            policy.trades_seen = counts.trades;
            for file in ORDER_STATE_FILES {
                policy.dirty.insert(file, now);
            }
        }
        if insights_changed {
            for file in INSIGHT_STATE_FILES {
                policy.dirty.insert(file, now);
            }
        }

        let day = time.date_utc();
        if policy.last_eod_day != Some(day) {
            policy.last_eod_day = Some(day);
            for file in EOD_FILES {
                self.mirror(file);
            }
        }

        self.flush_debounced_mirrors_locked(&mut policy, now);
    }

    fn flush_debounced_mirrors_locked(&self, policy: &mut MirrorPolicy, now: Instant) {
        policy.dirty.retain(|file, changed_at| {
            if now.duration_since(*changed_at) >= MIRROR_DEBOUNCE {
                self.mirror(file);
                false
            } else {
                true
            }
        });
    }

    fn flush_debounced_mirrors(&self) {
        let Ok(mut policy) = self.mirror_policy.lock() else {
            return;
        };
        self.flush_debounced_mirrors_locked(&mut policy, Instant::now());
    }

    /// Write the insight snapshot files. Returns true when the insight set
    /// changed since the last snapshot — the framework drained insight events
    /// (emitted/expired/closed/restored). Per-slice re-scoring of active
    /// insights emits no events, so a quiet market reports no change.
    fn record_insight_snapshot(&self, framework: Option<&Arc<Mutex<FrameworkState>>>) -> bool {
        let Some(framework) = framework else {
            return false;
        };
        let Ok(mut fw) = framework.lock() else {
            return false;
        };
        let payload = LiveInsightSnapshot {
            schema_version: LIVE_INSIGHTS_SCHEMA_VERSION,
            deployment_id: deployment_id_from_dir(&self.dir),
            written_at: chrono::Utc::now().to_rfc3339(),
            state: fw.insight_snapshot(),
        };
        write_json_pretty_atomic(&self.insights_path, &payload);
        let analytics = fw.compute_alpha_analytics();
        write_json_pretty_atomic(&self.alpha_analytics_path, &analytics);
        let events = fw.take_insight_events();
        let changed = !events.is_empty();
        append_json_lines(&self.insight_events_path, &events);
        changed
    }

    fn append_heartbeat(
        &self,
        time: DateTime,
        portfolio: &SecurityPortfolioManager,
        slices_processed: usize,
        order_events: usize,
        trades: usize,
    ) {
        let Ok(mut last_heartbeat) = self.last_heartbeat.lock() else {
            return;
        };
        if last_heartbeat.elapsed() < Duration::from_secs(30) {
            return;
        }
        *last_heartbeat = Instant::now();
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.heartbeat_path)
        {
            let _ = writeln!(
                file,
                "{} time={} slices={} portfolio={} order_events={} trades={}",
                chrono::Utc::now().to_rfc3339(),
                time,
                slices_processed,
                portfolio.total_portfolio_value(),
                order_events,
                trades
            );
        }
    }

    /// Record liveness from LEAN-style wall-clock time pulses. This deliberately
    /// bypasses the heavier portfolio/order/insight snapshot writes; the
    /// heartbeat's own 30-second gate keeps quiet Daily strategies observable
    /// without turning an empty time pulse into market data.
    pub(crate) fn record_time_pulse(
        &self,
        time: DateTime,
        portfolio: &SecurityPortfolioManager,
        counts: LiveSnapshotCounts,
    ) {
        self.append_heartbeat(
            time,
            portfolio,
            counts.slices_processed,
            counts.order_events,
            counts.trades,
        );
        // A quiet Daily strategy may have no data slice after its final fill.
        // The wall-clock pulse completes the trailing debounce without doing
        // another heavy portfolio/insight serialization pass.
        self.flush_debounced_mirrors();
    }
}

pub fn load_live_insight_snapshot(dir: &Path) -> Option<LiveInsightSnapshot> {
    let path = dir.join("insights.json");
    if !path.exists() {
        return None;
    }
    match serde_json::from_str::<LiveInsightSnapshot>(&std::fs::read_to_string(&path).ok()?) {
        Ok(snapshot) if snapshot.schema_version == LIVE_INSIGHTS_SCHEMA_VERSION => Some(snapshot),
        Ok(snapshot) => {
            warn!(
                "live insight restore: unsupported schema version {} in {}",
                snapshot.schema_version,
                path.display()
            );
            None
        }
        Err(err) => {
            warn!(
                "live insight restore: failed to parse {}: {err:#}",
                path.display()
            );
            None
        }
    }
}

pub fn load_deploy_restore(dir: &Path) -> Option<DeployRestore> {
    if !dir.join("portfolio.json").exists() {
        return None;
    }
    let account_state = match parse_account_snapshot(dir) {
        Ok(state) => state,
        Err(err) => {
            warn!(
                "paper restore: failed to parse snapshots in {}: {err:#}; starting flat",
                dir.display()
            );
            return None;
        }
    };
    Some(DeployRestore {
        order_events: load_json_lines(&dir.join("order-events.jsonl")),
        trades: load_json_lines(&dir.join("trades.jsonl")),
        account_state,
    })
}

pub fn restore_live_insights(
    framework: &Arc<Mutex<FrameworkState>>,
    dir: &Path,
) -> Option<(usize, usize)> {
    let snapshot = load_live_insight_snapshot(dir)?;
    let active_count = snapshot.state.active.len();
    let closed_count = snapshot.state.closed.len();
    framework
        .lock()
        .ok()?
        .restore_insights(snapshot.state, DateTime::now());
    Some((active_count, closed_count))
}

pub fn apply_initial_brokerage_account_state(
    algorithm: &Arc<Mutex<QcAlgorithm>>,
    account_state: &AccountState,
) {
    let mut algorithm = algorithm.lock().unwrap();
    if !account_state.cash_balances.is_empty() {
        *algorithm.portfolio.cash.write() = account_state.cash;
    }

    for holding in &account_state.holdings {
        if !algorithm.securities.contains(&holding.symbol) {
            algorithm.add_security_symbol(holding.symbol.clone(), Resolution::Minute);
        }
        let multiplier = algorithm.contract_multiplier_for_symbol(&holding.symbol);
        algorithm.portfolio.set_holdings(
            &holding.symbol,
            holding.average_price,
            holding.quantity,
            multiplier,
        );
        if holding.market_price > Decimal::ZERO {
            algorithm
                .securities
                .update_price(&holding.symbol, holding.market_price);
            algorithm
                .portfolio
                .update_prices(&holding.symbol, holding.market_price);
        }
    }

    for order in &account_state.open_orders {
        algorithm.transactions.add_or_update_order(order.clone());
    }
}

pub fn set_live_starting_portfolio_value_from_synced_account(
    portfolio: &SecurityPortfolioManager,
) -> Decimal {
    let starting_value = portfolio.total_portfolio_value();
    portfolio.set_starting_cash(starting_value);
    starting_value
}

fn deployment_id_from_dir(dir: &Path) -> Option<String> {
    dir.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

fn parse_account_snapshot(dir: &Path) -> anyhow::Result<AccountState> {
    let portfolio: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("portfolio.json"))?)
            .context("parse portfolio.json")?;

    let parse_dec = |value: &serde_json::Value, key: &str| -> Decimal {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .and_then(|s| Decimal::from_str_exact(s).ok())
            .unwrap_or(Decimal::ZERO)
    };

    let cash = parse_dec(&portfolio, "cash");

    let mut holdings = Vec::new();
    if let Some(items) = portfolio.get("holdings").and_then(|v| v.as_array()) {
        for item in items {
            let Some(symbol_value) = item.get("symbol_id") else {
                warn!("paper restore: holding missing symbol_id, skipping");
                continue;
            };
            let symbol: Symbol = match serde_json::from_value(symbol_value.clone()) {
                Ok(symbol) => symbol,
                Err(err) => {
                    warn!("paper restore: unparseable symbol_id, skipping: {err}");
                    continue;
                }
            };
            let quantity = parse_dec(item, "quantity");
            if quantity.is_zero() {
                continue;
            }
            holdings.push(rlean_brokerages::BrokerageHolding {
                symbol,
                quantity,
                average_price: parse_dec(item, "average_price"),
                market_price: parse_dec(item, "last_price"),
            });
        }
    }

    let positions = holdings
        .iter()
        .map(|holding| (holding.symbol.id.ticker.clone(), holding.quantity))
        .collect();

    Ok(AccountState {
        cash,
        cash_balances: vec![("USD".to_string(), cash)],
        positions,
        holdings,
        open_orders: load_open_orders(&dir.join("orders.json")),
        last_sync_time: chrono::Utc::now(),
    })
}

fn load_open_orders(path: &Path) -> Vec<Order> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    value
        .get("orders")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<Order>(item.clone()).ok())
                .filter(|order| order.is_open())
                .collect()
        })
        .unwrap_or_default()
}

fn load_json_lines<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str::<T>(line).ok())
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn brokerage_snapshot_refreshes_portfolio_without_market_data_slice() {
        let dir = tempfile::tempdir().unwrap();
        let writer = LiveDeploymentWriter::new(dir.path().to_path_buf());
        let portfolio = SecurityPortfolioManager::new_live(dec!(1_000));
        let transactions = TransactionManager::new();
        let first_time = DateTime::from_secs(1_700_000_000);

        writer.record_snapshot_from_transactions(
            first_time,
            &portfolio,
            &transactions,
            None,
            LiveSnapshotCounts {
                slices_processed: 0,
                order_events: 0,
                trades: 0,
            },
        );
        *portfolio.cash.write() = dec!(875);
        let fill_time = first_time + rlean_core::TimeSpan::from_secs(5);
        writer.record_snapshot_from_transactions(
            fill_time,
            &portfolio,
            &transactions,
            None,
            LiveSnapshotCounts {
                slices_processed: 0,
                order_events: 1,
                trades: 0,
            },
        );

        let snapshot: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("portfolio.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot["time"], fill_time.to_string());
        assert_eq!(snapshot["cash"], "875");
    }
}
