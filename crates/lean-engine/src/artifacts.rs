//! Run artifact sink — relays backtest and live run files to S3 object storage.
//!
//! Backtest run dirs (`backtests/<run-id>/...`) and live deployment dirs
//! (`live/<deploy-id>/...`) exist only on the node that ran them. This module
//! adds an optional mirror to S3 so results survive ephemeral nodes and can be
//! read back centrally later (the read path itself is out of scope here — see
//! epic #22 workstream 1.5).
//!
//! One abstraction, `RunArtifactSink`, is consumed by both the backtest stream
//! writer and the live deployment writer. It has three modes:
//!
//! - **Local**: write to the run dir only. No S3. This is the default; nothing
//!   changes for users who do not opt in.
//! - **Both**: write to the run dir AND mirror files to S3. Local is primary;
//!   S3 upload is best-effort and never blocks the writer.
//! - **S3 only**: use a temp local dir as a working buffer, upload files to S3,
//!   and delete the temp dir when the run finishes.
//!
//! ## Write semantics
//!
//! Both writers still write files locally exactly as before. After each local
//! write they call [`RunArtifactSink::mirror`] with the file's name. The sink
//! then decides when to actually upload:
//!
//! - **Backtests** call [`RunArtifactSink::mirror`] frequently (on every
//!   progress flush). The sink throttles uploads to at most one per file every
//!   [`CHECKPOINT_INTERVAL`] so a long run does not hammer S3, and
//!   [`RunArtifactSink::flush_all`] on completion uploads the final state. A run
//!   that dies mid-way therefore leaves its last checkpoint in S3.
//! - **Live** calls [`RunArtifactSink::mirror`] on every snapshot. The sink
//!   pushes the upload onto a bounded async queue; if S3 is slow or down the
//!   queue fills and further uploads are dropped with a WARN — the live loop
//!   never blocks on S3. [`RunArtifactSink::flush_all`] on clean shutdown drains
//!   the queue.
//!
//! S3 key layout mirrors the local dirs:
//! `<prefix>/<project>/backtests/<run-id>/<file>` and
//! `<prefix>/<project>/live/<deploy-id>/<file>`.
//!
//! The sink is strictly write-only: it exposes no read API. In `both` mode all
//! artifact reads (live restore, `rlean live portfolio`, report access) keep
//! going through the local dir exactly as before — S3 is replication only.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use object_store::aws::AmazonS3Builder;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, PutPayload};
use tokio::sync::mpsc;

/// How often a backtest checkpoint re-uploads a given file, at most.
///
/// Backtests call `mirror` on every progress flush (potentially many times a
/// second). Uploading that often would be wasteful, so per-file uploads are
/// throttled to this cadence. `flush_all` on completion bypasses the throttle.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// Bound on the live mirror queue. When full, further mirror requests are
/// dropped with a WARN rather than blocking the live loop.
pub const LIVE_QUEUE_CAPACITY: usize = 256;

/// Credentials / endpoint for an S3-compatible object store. Works against AWS
/// and any S3-compatible endpoint (OCI, MinIO, etc.) via a custom endpoint URL.
#[derive(Debug, Clone, Default)]
pub struct S3Settings {
    pub bucket: String,
    /// Key prefix under which all runs are stored, e.g. `runs`. May be empty.
    pub prefix: String,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
}

/// Which run kind a sink is mirroring — decides the `backtests/` vs `live/`
/// path segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunKind {
    Backtest,
    Live,
}

impl RunKind {
    fn segment(self) -> &'static str {
        match self {
            RunKind::Backtest => "backtests",
            RunKind::Live => "live",
        }
    }
}

/// Requested artifact storage mode (from CLI / env / config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactStoreMode {
    Local,
    S3,
    Both,
}

impl ArtifactStoreMode {
    /// Parse `local` / `s3` / `both` (case-insensitive). Unknown values return
    /// `None` so callers can reject a typo rather than silently losing S3.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Some(Self::Local),
            "s3" => Some(Self::S3),
            "both" => Some(Self::Both),
            _ => None,
        }
    }

    fn uses_s3(self) -> bool {
        matches!(self, Self::S3 | Self::Both)
    }
}

/// Async S3 relay: owns the object store handle plus the key prefix for one run.
struct S3Relay {
    store: Arc<dyn ObjectStore>,
    /// `<prefix>/<project>/<backtests|live>/<run-id>` — files are appended.
    key_base: String,
    /// Bounded queue to the background upload task (live mode).
    tx: Mutex<Option<mpsc::Sender<UploadJob>>>,
    /// Handle to the background task, joined on flush.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct UploadJob {
    key: String,
    bytes: Vec<u8>,
}

impl S3Relay {
    fn key_for(&self, file_name: &str) -> String {
        format!("{}/{}", self.key_base, file_name)
    }

    /// Upload a single object (used by backtest checkpoints and final flush,
    /// where blocking briefly is acceptable).
    async fn put(&self, key: &str, bytes: Vec<u8>) -> object_store::Result<()> {
        let path = ObjectPath::from(key.to_string());
        self.store.put(&path, PutPayload::from(bytes)).await?;
        Ok(())
    }
}

/// The one abstraction consumed by both artifact writers.
pub struct RunArtifactSink {
    mode: ArtifactStoreMode,
    /// The dir the writers actually write files into. For `Local`/`Both` this
    /// is the real run dir. For `S3` only it is a temp dir working buffer.
    working_dir: PathBuf,
    /// Present when `mode` uses S3.
    relay: Option<Arc<S3Relay>>,
    /// True when `working_dir` is a temp dir we own and must delete on drop.
    owns_working_dir: bool,
    /// Per-file last-upload time, for backtest checkpoint throttling.
    last_upload: Mutex<std::collections::HashMap<String, Instant>>,
}

impl RunArtifactSink {
    /// Local-only sink: writes to `dir`, no S3. This is the default path.
    pub fn local(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self {
            mode: ArtifactStoreMode::Local,
            working_dir: dir,
            relay: None,
            owns_working_dir: false,
            last_upload: Mutex::new(Default::default()),
        }
    }

    /// Build a sink for the requested mode.
    ///
    /// `local_dir` is the natural run dir (`backtests/<run-id>` or
    /// `live/<deploy-id>`). `project` is the strategy/project name used as the
    /// S3 key segment. `run_id` is the run/deploy id (the local dir's final
    /// path component). For `S3`-only mode a temp working dir is created and
    /// `local_dir` is not written to.
    pub fn new(
        mode: ArtifactStoreMode,
        kind: RunKind,
        local_dir: PathBuf,
        project: &str,
        run_id: &str,
        s3: Option<&S3Settings>,
    ) -> Self {
        if !mode.uses_s3() {
            return Self::local(local_dir);
        }
        let settings = match s3 {
            Some(settings) if !settings.bucket.is_empty() => settings,
            _ => {
                tracing::warn!(
                    "artifact store mode {:?} requested but no S3 bucket configured; falling back to local",
                    mode
                );
                return Self::local(local_dir);
            }
        };
        let store = match build_object_store(settings) {
            Ok(store) => store,
            Err(err) => {
                tracing::warn!("failed to build S3 artifact store: {err:#}; falling back to local");
                return Self::local(local_dir);
            }
        };
        Self::with_store(
            mode,
            kind,
            local_dir,
            project,
            run_id,
            &settings.prefix,
            store,
        )
    }

    /// Build a sink against an explicit object store. Tests pass `InMemory`.
    pub fn with_store(
        mode: ArtifactStoreMode,
        kind: RunKind,
        local_dir: PathBuf,
        project: &str,
        run_id: &str,
        prefix: &str,
        store: Arc<dyn ObjectStore>,
    ) -> Self {
        let key_base = build_key_base(prefix, project, kind, run_id);
        let (working_dir, owns_working_dir) = match mode {
            ArtifactStoreMode::S3 => match tempfile::Builder::new()
                .prefix("rlean-artifacts-")
                .tempdir()
            {
                Ok(dir) => (dir.keep(), true),
                Err(err) => {
                    tracing::warn!("failed to create S3 working temp dir: {err}; using run dir");
                    (local_dir.clone(), false)
                }
            },
            _ => (local_dir.clone(), false),
        };
        let _ = std::fs::create_dir_all(&working_dir);

        // Live mode gets a bounded async queue with a drop-and-warn drain task.
        let (tx, task) = if kind == RunKind::Live {
            let (tx, mut rx) = mpsc::channel::<UploadJob>(LIVE_QUEUE_CAPACITY);
            let store_for_task = store.clone();
            let handle = upload_runtime().spawn(async move {
                while let Some(job) = rx.recv().await {
                    let path = ObjectPath::from(job.key.clone());
                    if let Err(err) = store_for_task.put(&path, PutPayload::from(job.bytes)).await {
                        tracing::warn!("live artifact S3 upload failed for {}: {err}", job.key);
                    }
                }
            });
            (Mutex::new(Some(tx)), Mutex::new(Some(handle)))
        } else {
            (Mutex::new(None), Mutex::new(None))
        };

        let relay = Arc::new(S3Relay {
            store,
            key_base,
            tx,
            task,
        });

        Self {
            mode,
            working_dir,
            relay: Some(relay),
            owns_working_dir,
            last_upload: Mutex::new(Default::default()),
        }
    }

    /// The dir writers should write files into.
    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }

    /// The S3 key base for this run, or `None` if local-only. Test/introspection
    /// helper.
    pub fn s3_key_base(&self) -> Option<&str> {
        self.relay.as_ref().map(|relay| relay.key_base.as_str())
    }

    /// The requested mode.
    pub fn mode(&self) -> ArtifactStoreMode {
        self.mode
    }

    /// True when the working dir is the real, persistent run dir (local / both).
    /// False in `s3`-only mode, where the working dir is a temp buffer that is
    /// deleted after upload — callers should not, e.g., point a `latest` symlink
    /// at it.
    pub fn writes_local_run_dir(&self) -> bool {
        self.mode != ArtifactStoreMode::S3
    }

    /// Mirror the named file (relative to the working dir) to S3.
    ///
    /// - Local mode: no-op.
    /// - Backtest (Both/S3): throttled — uploads at most once per
    ///   [`CHECKPOINT_INTERVAL`] per file. Reads the current file contents.
    /// - Live (Both/S3): enqueues the current file contents on the bounded
    ///   queue; drops with a WARN if the queue is full.
    pub fn mirror(&self, file_name: &str) {
        let Some(relay) = self.relay.as_ref() else {
            return;
        };
        let is_live = { relay.tx.lock().unwrap().is_some() };
        if is_live {
            self.mirror_live(relay, file_name);
        } else {
            self.mirror_checkpoint(relay, file_name);
        }
    }

    fn mirror_live(&self, relay: &Arc<S3Relay>, file_name: &str) {
        let Some(bytes) = self.read_working_file(file_name) else {
            return;
        };
        let key = relay.key_for(file_name);
        let guard = relay.tx.lock().unwrap();
        if let Some(tx) = guard.as_ref() {
            match tx.try_send(UploadJob { key, bytes }) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(job)) => {
                    tracing::warn!(
                        "live artifact S3 queue full; dropping upload of {}",
                        job.key
                    );
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }

    fn mirror_checkpoint(&self, relay: &Arc<S3Relay>, file_name: &str) {
        {
            let mut last = self.last_upload.lock().unwrap();
            if let Some(when) = last.get(file_name) {
                if when.elapsed() < CHECKPOINT_INTERVAL {
                    return;
                }
            }
            last.insert(file_name.to_string(), Instant::now());
        }
        let Some(bytes) = self.read_working_file(file_name) else {
            return;
        };
        let key = relay.key_for(file_name);
        let relay = relay.clone();
        let _ = run_blocking(async move { relay.put(&key, bytes).await });
    }

    fn read_working_file(&self, file_name: &str) -> Option<Vec<u8>> {
        std::fs::read(self.working_dir.join(file_name)).ok()
    }

    /// Upload every file currently in the working dir to S3, bypassing the
    /// checkpoint throttle, and drain the live queue. Call on completion / clean
    /// shutdown. No-op for local-only sinks.
    pub fn flush_all(&self) {
        let Some(relay) = self.relay.as_ref() else {
            return;
        };

        // Drain the live queue first so nothing in flight is lost, then close it.
        {
            let mut guard = relay.tx.lock().unwrap();
            *guard = None;
        }
        if let Some(handle) = relay.task.lock().unwrap().take() {
            let _ = run_blocking(handle);
        }

        // Upload every file in the working dir.
        for file_name in self.list_working_files() {
            if let Some(bytes) = self.read_working_file(&file_name) {
                let key = relay.key_for(&file_name);
                let relay = relay.clone();
                let _ = run_blocking(async move { relay.put(&key, bytes).await });
            }
        }
    }

    fn list_working_files(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_files(&self.working_dir, &self.working_dir, &mut out);
        out
    }
}

impl Drop for RunArtifactSink {
    fn drop(&mut self) {
        // S3-only mode: the working dir was a temp buffer. Uploads already
        // happened via flush_all; delete the local buffer now.
        if self.owns_working_dir && self.mode == ArtifactStoreMode::S3 {
            let _ = std::fs::remove_dir_all(&self.working_dir);
        }
    }
}

/// Recursively collect file paths under `dir`, returned relative to `root`
/// using forward slashes (so nested `code/main.py` mirrors correctly).
fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out);
        } else if let Ok(rel) = path.strip_prefix(root) {
            let name = rel.to_string_lossy();
            // Skip atomic-write temp files.
            if name.ends_with(".tmp") {
                continue;
            }
            out.push(name.replace('\\', "/"));
        }
    }
}

fn build_key_base(prefix: &str, project: &str, kind: RunKind, run_id: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    let prefix = prefix.trim_matches('/');
    if !prefix.is_empty() {
        segments.push(prefix);
    }
    if !project.is_empty() {
        segments.push(project);
    }
    segments.push(kind.segment());
    segments.push(run_id);
    segments.join("/")
}

fn build_object_store(settings: &S3Settings) -> anyhow::Result<Arc<dyn ObjectStore>> {
    let mut builder = AmazonS3Builder::new().with_bucket_name(&settings.bucket);
    if let Some(region) = &settings.region {
        builder = builder.with_region(region);
    }
    if let Some(endpoint) = &settings.endpoint {
        builder = builder.with_endpoint(endpoint);
        // Custom endpoints (MinIO / OCI / local) commonly use path-style and
        // may be plain HTTP. Allow both so S3-compatible stores work.
        builder = builder.with_virtual_hosted_style_request(false);
        if endpoint.starts_with("http://") {
            builder = builder.with_allow_http(true);
        }
    }
    if let Some(access_key) = &settings.access_key {
        builder = builder.with_access_key_id(access_key);
    }
    if let Some(secret_key) = &settings.secret_key {
        builder = builder.with_secret_access_key(secret_key);
    }
    let store = builder.build()?;
    Ok(Arc::new(store))
}

/// A dedicated multi-thread runtime that owns all S3 upload work.
///
/// Uploads run on this runtime's own threads — never on the caller's runtime
/// workers — so driving them to completion synchronously (backtest checkpoints
/// and final flush) does not deadlock the engine's async loop.
fn upload_runtime() -> tokio::runtime::Handle {
    static SHARED: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    let runtime = SHARED.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("rlean-artifacts")
            .enable_all()
            .build()
            .expect("build artifact upload runtime")
    });
    runtime.handle().clone()
}

/// Drive `fut` to completion on the dedicated upload runtime and block the
/// caller until it finishes.
///
/// The future is spawned on the upload runtime (its own threads), and the
/// caller waits on the join handle. When the caller is itself inside a
/// multi-thread tokio runtime we use `block_in_place` so waiting does not stall
/// that runtime's scheduler; otherwise we block the thread directly.
fn run_blocking<F>(fut: F) -> F::Output
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    use tokio::runtime::RuntimeFlavor;
    let task = upload_runtime().spawn(fut);
    let wait = async move { task.await.expect("artifact upload task panicked") };
    match tokio::runtime::Handle::try_current() {
        // On a multi-thread runtime, `block_in_place` lets the current worker
        // block without starving the scheduler.
        Ok(current) if current.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| upload_runtime().block_on(wait))
        }
        // On a current-thread runtime we cannot block the single worker, so wait
        // on a dedicated OS thread that drives the upload runtime.
        Ok(_) => std::thread::scope(|scope| {
            scope
                .spawn(|| upload_runtime().block_on(wait))
                .join()
                .expect("artifact upload wait thread panicked")
        }),
        // Not inside any runtime: block the calling thread directly.
        Err(_) => upload_runtime().block_on(wait),
    }
}

/// Convenience for tests: build an in-memory backed sink.
#[doc(hidden)]
pub fn in_memory_sink(
    mode: ArtifactStoreMode,
    kind: RunKind,
    local_dir: PathBuf,
    project: &str,
    run_id: &str,
    prefix: &str,
) -> (RunArtifactSink, Arc<InMemory>) {
    let store = Arc::new(InMemory::new());
    let sink = RunArtifactSink::with_store(
        mode,
        kind,
        local_dir,
        project,
        run_id,
        prefix,
        store.clone(),
    );
    (sink, store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn write_file(dir: &Path, name: &str, contents: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    async fn list_keys(store: &InMemory) -> Vec<String> {
        let mut keys: Vec<String> = store
            .list(None)
            .map(|meta| meta.unwrap().location.to_string())
            .collect::<Vec<_>>()
            .await;
        keys.sort();
        keys
    }

    async fn get_key(store: &InMemory, key: &str) -> String {
        let path = ObjectPath::from(key.to_string());
        let bytes = store.get(&path).await.unwrap().bytes().await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[test]
    fn parse_modes() {
        assert_eq!(
            ArtifactStoreMode::parse("local"),
            Some(ArtifactStoreMode::Local)
        );
        assert_eq!(ArtifactStoreMode::parse("S3"), Some(ArtifactStoreMode::S3));
        assert_eq!(
            ArtifactStoreMode::parse(" Both "),
            Some(ArtifactStoreMode::Both)
        );
        assert_eq!(ArtifactStoreMode::parse("nope"), None);
    }

    #[test]
    fn key_layout_backtest_and_live() {
        assert_eq!(
            build_key_base("runs", "my_project", RunKind::Backtest, "2024_run"),
            "runs/my_project/backtests/2024_run"
        );
        assert_eq!(
            build_key_base("runs/", "proj", RunKind::Live, "deploy1"),
            "runs/proj/live/deploy1"
        );
        // Empty prefix is tolerated.
        assert_eq!(
            build_key_base("", "proj", RunKind::Backtest, "r"),
            "proj/backtests/r"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn local_mode_never_touches_s3() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backtests").join("run1");
        std::fs::create_dir_all(&dir).unwrap();
        let sink = RunArtifactSink::local(dir.clone());
        write_file(&dir, "progress.json", "{}");
        sink.mirror("progress.json");
        sink.flush_all();
        assert!(sink.s3_key_base().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn both_mode_mirrors_on_flush() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backtests").join("run1");
        std::fs::create_dir_all(&dir).unwrap();
        let (sink, store) = in_memory_sink(
            ArtifactStoreMode::Both,
            RunKind::Backtest,
            dir.clone(),
            "proj",
            "run1",
            "runs",
        );
        write_file(&dir, "progress.json", "{\"p\":1}");
        write_file(&dir, "code/main.py", "print(1)");
        sink.flush_all();

        // Local files still present (Both is local-primary).
        assert!(dir.join("progress.json").exists());

        let keys = list_keys(&store).await;
        assert_eq!(
            keys,
            vec![
                "runs/proj/backtests/run1/code/main.py".to_string(),
                "runs/proj/backtests/run1/progress.json".to_string(),
            ]
        );
        assert_eq!(
            get_key(&store, "runs/proj/backtests/run1/progress.json").await,
            "{\"p\":1}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn checkpoint_upload_is_throttled_but_flush_forces() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run1");
        std::fs::create_dir_all(&dir).unwrap();
        let (sink, store) = in_memory_sink(
            ArtifactStoreMode::Both,
            RunKind::Backtest,
            dir.clone(),
            "proj",
            "run1",
            "runs",
        );
        // First mirror uploads immediately (no prior timestamp).
        write_file(&dir, "progress.json", "v1");
        sink.mirror("progress.json");
        assert_eq!(
            get_key(&store, "runs/proj/backtests/run1/progress.json").await,
            "v1"
        );
        // Second mirror within the interval is throttled: S3 still holds v1.
        write_file(&dir, "progress.json", "v2");
        sink.mirror("progress.json");
        assert_eq!(
            get_key(&store, "runs/proj/backtests/run1/progress.json").await,
            "v1"
        );
        // flush_all bypasses the throttle and uploads the latest.
        sink.flush_all();
        assert_eq!(
            get_key(&store, "runs/proj/backtests/run1/progress.json").await,
            "v2"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn s3_only_mode_uses_temp_buffer_and_deletes_it() {
        let tmp = tempfile::tempdir().unwrap();
        // local_dir is intentionally NOT created — s3-only must not use it.
        let local_dir = tmp.path().join("backtests").join("run1");
        let (sink, store) = in_memory_sink(
            ArtifactStoreMode::S3,
            RunKind::Backtest,
            local_dir.clone(),
            "proj",
            "run1",
            "runs",
        );
        let working = sink.working_dir().to_path_buf();
        assert_ne!(working, local_dir);
        write_file(&working, "summary.json", "done");
        sink.flush_all();
        assert_eq!(
            get_key(&store, "runs/proj/backtests/run1/summary.json").await,
            "done"
        );
        // The real run dir was never written.
        assert!(!local_dir.exists());
        drop(sink);
        // Temp working buffer deleted on drop.
        assert!(!working.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_mode_mirrors_each_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("live").join("deploy1");
        std::fs::create_dir_all(&dir).unwrap();
        let (sink, store) = in_memory_sink(
            ArtifactStoreMode::Both,
            RunKind::Live,
            dir.clone(),
            "proj",
            "deploy1",
            "runs",
        );
        write_file(&dir, "portfolio.json", "snap1");
        sink.mirror("portfolio.json");
        // flush_all drains the async queue.
        sink.flush_all();
        assert_eq!(
            get_key(&store, "runs/proj/live/deploy1/portfolio.json").await,
            "snap1"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_bounded_queue_drops_when_full() {
        // A blocking store makes every upload hang so the queue fills up. The
        // sink must drop excess mirror requests and never block the caller.
        use std::sync::atomic::{AtomicUsize, Ordering};

        #[derive(Debug)]
        struct BlockingStore {
            gate: Arc<tokio::sync::Semaphore>,
            accepted: Arc<AtomicUsize>,
        }

        impl std::fmt::Display for BlockingStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "BlockingStore")
            }
        }

        #[async_trait::async_trait]
        impl ObjectStore for BlockingStore {
            async fn put_opts(
                &self,
                _location: &ObjectPath,
                _payload: PutPayload,
                _opts: object_store::PutOptions,
            ) -> object_store::Result<object_store::PutResult> {
                self.accepted.fetch_add(1, Ordering::SeqCst);
                // Hang until the semaphore is closed — simulates a stalled S3.
                let _ = self.gate.acquire().await;
                Ok(object_store::PutResult {
                    e_tag: None,
                    version: None,
                })
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
                _location: &ObjectPath,
                _options: object_store::GetOptions,
            ) -> object_store::Result<object_store::GetResult> {
                unimplemented!()
            }
            async fn delete(&self, _location: &ObjectPath) -> object_store::Result<()> {
                unimplemented!()
            }
            fn list(
                &self,
                _prefix: Option<&ObjectPath>,
            ) -> futures::stream::BoxStream<'_, object_store::Result<object_store::ObjectMeta>>
            {
                futures::stream::empty().boxed()
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

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("live").join("deploy1");
        std::fs::create_dir_all(&dir).unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let accepted = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(BlockingStore {
            gate: gate.clone(),
            accepted: accepted.clone(),
        });
        let sink = RunArtifactSink::with_store(
            ArtifactStoreMode::Both,
            RunKind::Live,
            dir.clone(),
            "proj",
            "deploy1",
            "runs",
            store,
        );
        write_file(&dir, "portfolio.json", "snap");

        // Fire many more mirrors than the queue can hold. Each call must return
        // promptly (the loop finishing at all proves it never blocked).
        for _ in 0..(LIVE_QUEUE_CAPACITY + 50) {
            sink.mirror("portfolio.json");
        }
        // At most one job is in-flight in the worker; the queue holds the rest
        // up to its bound and everything beyond was dropped.
        assert!(accepted.load(Ordering::SeqCst) <= 1);

        // Close the gate so every pending and future put completes immediately,
        // then drain the queue on flush.
        gate.close();
        sink.flush_all();
    }
}
