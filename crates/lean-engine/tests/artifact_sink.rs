//! Integration tests: the artifact writers drive the RunArtifactSink and the
//! expected keys land in an in-memory object store. No cloud credentials.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use lean_algorithm::portfolio::SecurityPortfolioManager;
use lean_alpha::{Insight, InsightCollectionSnapshot, InsightDirection};
use lean_core::{DateTime, Market, Symbol, TimeSpan};
use lean_engine::artifacts::{in_memory_sink, ArtifactStoreMode, RunArtifactSink, RunKind};
use lean_engine::framework::FrameworkState;
use lean_engine::live::deployment_writer::{
    LiveDeploymentWriter, LiveSnapshotCounts, MIRROR_DEBOUNCE,
};
use lean_engine::runner::stream_writer::BacktestStreamWriter;
use lean_orders::fill_model::ImmediateFillModel;
use lean_orders::order_processor::OrderProcessor;
use lean_orders::slippage::NullSlippageModel;
use lean_orders::TransactionManager;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use rust_decimal_macros::dec;

async fn keys(store: &InMemory) -> Vec<String> {
    let mut keys: Vec<String> = store
        .list(None)
        .map(|meta| meta.unwrap().location.to_string())
        .collect::<Vec<_>>()
        .await;
    keys.sort();
    keys
}

async fn get(store: &InMemory, key: &str) -> String {
    let bytes = store
        .get(&ObjectPath::from(key.to_string()))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn backtest_stream_writer_mirrors_run_files() {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("backtests").join("20260101_120000_algo");
    let (sink, store) = in_memory_sink(
        ArtifactStoreMode::Mirror,
        RunKind::Backtest,
        run_dir.clone(),
        "algo",
        "20260101_120000_algo",
        "runs",
    );
    let sink = Arc::new(sink);

    let start = chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap();
    let end = chrono::NaiveDate::from_ymd_opt(2024, 1, 31).unwrap();
    let mut writer = BacktestStreamWriter::new(sink.clone(), start, end);

    // First progress on a new trading day appends one line and checkpoints it.
    // progress.json is append-only compact JSON, one object per line.
    writer.record_progress(
        chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
        10,
        dec!(100000),
        0,
        0,
    );
    let uploaded = get(
        &store,
        "runs/algo/backtests/20260101_120000_algo/progress.json",
    )
    .await;
    assert!(uploaded.contains("\"status\":\"running\""));

    // Completion: mark appends the terminal line; finish uploads every file.
    writer.mark_completed(21, dec!(105000), 3, 2);
    writer.finish();

    let all = keys(&store).await;
    assert!(all.contains(&"runs/algo/backtests/20260101_120000_algo/progress.json".to_string()));
    assert!(
        all.contains(&"runs/algo/backtests/20260101_120000_algo/order-events.jsonl".to_string())
    );
    assert!(all.contains(&"runs/algo/backtests/20260101_120000_algo/trades.jsonl".to_string()));
    let final_progress = get(
        &store,
        "runs/algo/backtests/20260101_120000_algo/progress.json",
    )
    .await;
    // Append-only: the running line is retained and the completed line is last,
    // so a reader that takes the last line sees the terminal status.
    assert!(final_progress.contains("\"status\":\"running\""));
    let last_line = final_progress.lines().last().unwrap();
    assert!(last_line.contains("\"status\":\"completed\""));

    // Local run dir still holds everything (Mirror is local-primary).
    assert!(run_dir.join("progress.json").exists());
    assert!(run_dir.join("order-events.jsonl").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn live_deployment_writer_flush_uploads_deploy_files() {
    let tmp = tempfile::tempdir().unwrap();
    let deploy_dir = tmp.path().join("live").join("deploy-1");
    let (sink, store) = in_memory_sink(
        ArtifactStoreMode::Mirror,
        RunKind::Live,
        deploy_dir.clone(),
        "algo",
        "deploy-1",
        "runs",
    );
    let writer = LiveDeploymentWriter::with_sink(Arc::new(sink));

    // The writer created its streaming files in the deploy dir; a clean
    // shutdown flush must upload them under the live key layout.
    writer.flush();

    let all = keys(&store).await;
    assert!(all.contains(&"runs/algo/live/deploy-1/order-events.jsonl".to_string()));
    assert!(all.contains(&"runs/algo/live/deploy-1/trades.jsonl".to_string()));
    assert!(all.contains(&"runs/algo/live/deploy-1/heartbeat.log".to_string()));
    // Local deploy dir is kept.
    assert!(deploy_dir.join("heartbeat.log").exists());
}

// ---------------------------------------------------------------------------
// Live mirror trigger policy: `record_snapshot` enqueues S3 uploads only on
// state changes (fills/trades/insight events, debounced), at process start, at
// calendar-day rollover, and via the shutdown flush — a quiet live instance
// enqueues nothing.
// ---------------------------------------------------------------------------

const K_PORTFOLIO: &str = "runs/algo/live/deploy-1/portfolio.json";
const K_PROGRESS: &str = "runs/algo/live/deploy-1/progress.json";
const K_HEARTBEAT: &str = "runs/algo/live/deploy-1/heartbeat.log";
const K_ORDERS: &str = "runs/algo/live/deploy-1/orders.json";
const K_ORDER_EVENTS: &str = "runs/algo/live/deploy-1/order-events.jsonl";
const K_TRADES: &str = "runs/algo/live/deploy-1/trades.jsonl";
const K_INSIGHTS: &str = "runs/algo/live/deploy-1/insights.json";
const K_INSIGHT_EVENTS: &str = "runs/algo/live/deploy-1/insight-events.jsonl";
const K_ALPHA_ANALYTICS: &str = "runs/algo/live/deploy-1/alpha-analytics.json";

/// Wraps `InMemory`, counting puts per key so tests can assert exactly when
/// the live writer enqueued an upload for each artifact.
#[derive(Debug)]
struct CountingStore {
    inner: InMemory,
    puts: Mutex<HashMap<String, usize>>,
}

impl CountingStore {
    fn new() -> Self {
        Self {
            inner: InMemory::new(),
            puts: Mutex::new(HashMap::new()),
        }
    }

    fn put_count(&self, key: &str) -> usize {
        self.puts.lock().unwrap().get(key).copied().unwrap_or(0)
    }

    fn total_puts(&self) -> usize {
        self.puts.lock().unwrap().values().sum()
    }
}

impl std::fmt::Display for CountingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CountingStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for CountingStore {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        *self
            .puts
            .lock()
            .unwrap()
            .entry(location.to_string())
            .or_insert(0) += 1;
        self.inner.put_opts(location, payload, opts).await
    }
    async fn put_multipart_opts(
        &self,
        _location: &ObjectPath,
        _opts: object_store::PutMultipartOpts,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        unimplemented!()
    }
    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(location, options).await
    }
    async fn delete(&self, _location: &ObjectPath) -> object_store::Result<()> {
        unimplemented!()
    }
    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> futures::stream::BoxStream<'_, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(
        &self,
        _prefix: Option<&ObjectPath>,
    ) -> object_store::Result<object_store::ListResult> {
        unimplemented!()
    }
    async fn copy(&self, _from: &ObjectPath, _to: &ObjectPath) -> object_store::Result<()> {
        unimplemented!()
    }
    async fn copy_if_not_exists(
        &self,
        _from: &ObjectPath,
        _to: &ObjectPath,
    ) -> object_store::Result<()> {
        unimplemented!()
    }
}

struct LiveFixture {
    _tmp: tempfile::TempDir,
    writer: LiveDeploymentWriter,
    store: Arc<CountingStore>,
    portfolio: SecurityPortfolioManager,
    processor: OrderProcessor,
}

impl LiveFixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let deploy_dir = tmp.path().join("live").join("deploy-1");
        let store = Arc::new(CountingStore::new());
        let sink = RunArtifactSink::with_store(
            ArtifactStoreMode::Mirror,
            RunKind::Live,
            deploy_dir,
            "algo",
            "deploy-1",
            "runs",
            store.clone(),
        );
        Self {
            _tmp: tmp,
            writer: LiveDeploymentWriter::with_sink(Arc::new(sink)),
            store,
            portfolio: SecurityPortfolioManager::new(dec!(100_000)),
            processor: OrderProcessor::new(
                Box::new(ImmediateFillModel::new(Box::new(NullSlippageModel))),
                Arc::new(TransactionManager::new()),
            ),
        }
    }

    fn snapshot(
        &self,
        time: DateTime,
        framework: Option<&Arc<Mutex<FrameworkState>>>,
        counts: LiveSnapshotCounts,
    ) {
        self.writer
            .record_snapshot(time, &self.portfolio, &self.processor, framework, counts);
    }
}

fn counts(slices_processed: usize, order_events: usize, trades: usize) -> LiveSnapshotCounts {
    LiveSnapshotCounts {
        slices_processed,
        order_events,
        trades,
    }
}

/// A slice timestamp on the given July 2026 calendar day.
fn day_time(day: u32) -> DateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 7, day)
        .unwrap()
        .and_hms_opt(15, 0, 0)
        .unwrap()
        .into()
}

async fn wait_for_puts(store: &CountingStore, key: &str, at_least: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while store.put_count(key) < at_least {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {key} to reach {at_least} puts (have {})",
            store.put_count(key)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn live_mirror_steady_state_uploads_nothing() {
    let fx = LiveFixture::new();

    // The process-start snapshot mirrors the PnL/status bucket once.
    fx.snapshot(day_time(1), None, counts(0, 0, 0));
    wait_for_puts(&fx.store, K_PORTFOLIO, 1).await;
    wait_for_puts(&fx.store, K_PROGRESS, 1).await;
    wait_for_puts(&fx.store, K_HEARTBEAT, 1).await;
    let baseline = fx.store.total_puts();

    // A quiet stretch: many snapshots on the same day with no fills, no trades,
    // no insight events. Slice counts advance, but nothing else changes.
    for i in 1..=20 {
        fx.snapshot(day_time(1), None, counts(i, 0, 0));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        fx.store.total_puts(),
        baseline,
        "steady-state snapshots must enqueue zero uploads"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn live_mirror_order_event_triggers_debounced_upload() {
    let fx = LiveFixture::new();
    fx.snapshot(day_time(1), None, counts(0, 0, 0));
    wait_for_puts(&fx.store, K_PORTFOLIO, 1).await;

    // A fill: order-event and trade counts move. The order artifacts are marked
    // dirty but not uploaded until the change quiesces for MIRROR_DEBOUNCE.
    fx.snapshot(day_time(1), None, counts(1, 1, 1));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fx.store.put_count(K_ORDERS),
        0,
        "order upload must wait for the debounce"
    );

    // After the debounce, the next snapshot pass uploads the order bucket.
    tokio::time::sleep(MIRROR_DEBOUNCE).await;
    fx.snapshot(day_time(1), None, counts(2, 1, 1));
    wait_for_puts(&fx.store, K_ORDERS, 1).await;
    wait_for_puts(&fx.store, K_ORDER_EVENTS, 1).await;
    wait_for_puts(&fx.store, K_TRADES, 1).await;

    // Further unchanged snapshots do not re-upload.
    fx.snapshot(day_time(1), None, counts(3, 1, 1));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fx.store.put_count(K_ORDERS), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_mirror_insight_change_triggers_upload() {
    let fx = LiveFixture::new();
    let framework = Arc::new(Mutex::new(FrameworkState::new()));
    fx.snapshot(day_time(1), Some(&framework), counts(0, 0, 0));
    wait_for_puts(&fx.store, K_PORTFOLIO, 1).await;

    // No insight activity yet: the insight bucket stays un-mirrored.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fx.store.put_count(K_INSIGHTS), 0);

    // The insight set changes (a restore pushes a Restored insight event —
    // emissions/expiries/closes push events the same way).
    let insight = Insight::new(
        Symbol::create_equity("SPY", &Market::usa()),
        InsightDirection::Up,
        TimeSpan::ONE_HOUR,
        None,
        None,
        "test_alpha",
    );
    framework.lock().unwrap().restore_insights(
        InsightCollectionSnapshot {
            active: vec![insight],
            closed: vec![],
            total_count: 1,
        },
        DateTime::now(),
    );
    fx.snapshot(day_time(1), Some(&framework), counts(1, 0, 0));

    // After the debounce, the next snapshot uploads the insight bucket.
    tokio::time::sleep(MIRROR_DEBOUNCE).await;
    fx.snapshot(day_time(1), Some(&framework), counts(2, 0, 0));
    wait_for_puts(&fx.store, K_INSIGHTS, 1).await;
    wait_for_puts(&fx.store, K_INSIGHT_EVENTS, 1).await;
    wait_for_puts(&fx.store, K_ALPHA_ANALYTICS, 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn live_mirror_day_rollover_uploads_eod_bucket_once() {
    let fx = LiveFixture::new();
    fx.snapshot(day_time(1), None, counts(0, 0, 0));
    wait_for_puts(&fx.store, K_PORTFOLIO, 1).await;

    // More snapshots the same day: no further EOD uploads.
    for i in 1..=5 {
        fx.snapshot(day_time(1), None, counts(i, 0, 0));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fx.store.put_count(K_PORTFOLIO), 1);

    // First snapshot of the next calendar day mirrors the EOD bucket once.
    fx.snapshot(day_time(2), None, counts(6, 0, 0));
    wait_for_puts(&fx.store, K_PORTFOLIO, 2).await;
    wait_for_puts(&fx.store, K_PROGRESS, 2).await;
    wait_for_puts(&fx.store, K_HEARTBEAT, 2).await;

    // The rest of the new day stays quiet again.
    for i in 7..=10 {
        fx.snapshot(day_time(2), None, counts(i, 0, 0));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fx.store.put_count(K_PORTFOLIO), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn live_mirror_shutdown_flush_uploads_pending_changes() {
    let fx = LiveFixture::new();
    fx.snapshot(day_time(1), None, counts(0, 0, 0));
    wait_for_puts(&fx.store, K_PORTFOLIO, 1).await;

    // A fill arrives, then the process shuts down before the debounce ever
    // fires. The flush must still upload everything, so no change is lost.
    fx.snapshot(day_time(1), None, counts(1, 1, 1));
    assert_eq!(fx.store.put_count(K_ORDERS), 0);
    fx.writer.flush();
    assert!(fx.store.put_count(K_ORDERS) >= 1);
    assert!(fx.store.put_count(K_ORDER_EVENTS) >= 1);
    assert!(fx.store.put_count(K_TRADES) >= 1);
    assert!(fx.store.put_count(K_PORTFOLIO) >= 2);
}
