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
//! - **Mirror**: write to the run dir AND mirror files to S3. Local is primary;
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
//! - **Live**: the deployment writer calls [`RunArtifactSink::mirror`] only on
//!   state changes (fills, trades, insight events — debounced), at process
//!   start, at calendar-day rollover, and via `flush_all` on clean shutdown —
//!   not on every snapshot (see `MirrorPolicy` in `live::deployment_writer`),
//!   so a quiet live instance enqueues nothing. Beneath that, uploads are
//!   **latest-wins per file**: at most one upload is ever in flight for a given
//!   file name, plus at most one pending "dirty" flag. `mirror` on a file that
//!   is already uploading just sets the dirty flag and returns (coalescing) —
//!   it never blocks the live loop. When an in-flight upload finishes (success,
//!   error, or [`UPLOAD_TIMEOUT`]), if the file was marked dirty it re-reads the
//!   file and uploads the newest contents, so the mirror always converges to the
//!   latest state of every file once the endpoint is healthy. A separate bound
//!   ([`LIVE_QUEUE_CAPACITY`] distinct in-flight files) stops hundreds of
//!   *distinct* files from stampeding. Per-event drop/failure/timeout logs are
//!   emitted at `debug`; a periodic INFO summary (at most once per
//!   [`LIVE_SUMMARY_INTERVAL`]) reports succeeded / failed / timed-out /
//!   coalesced counts since the last summary, and a one-line INFO announces
//!   recovery on the first success after any failure — so an unhealthy-then-
//!   healed endpoint tells a clear story in live.log without per-event spam.
//!   [`RunArtifactSink::flush_all`] on clean shutdown waits (bounded) for
//!   in-flight uploads and then uploads the final state of every file.
//!
//! S3 key layout mirrors the local dirs:
//! `<prefix>/<project>/backtests/<run-id>/<file>` and
//! `<prefix>/<project>/live/<deploy-id>/<file>`.
//!
//! The sink is strictly write-only: it exposes no read API. In `mirror` mode all
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

/// How often a backtest checkpoint re-uploads a given file, at most.
///
/// Backtests call `mirror` on every progress flush (potentially many times a
/// second). Uploading that often would be wasteful, so per-file uploads are
/// throttled to this cadence. `flush_all` on completion bypasses the throttle.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// Bound on the number of *distinct* files that may have an upload in flight
/// at once. Live uploads are latest-wins per file (at most one in-flight upload
/// plus one dirty flag per file), so duplicates of the same file can never
/// stampede; this cap only stops hundreds of *distinct* files from doing so.
/// When it is reached a further first-touch of a new file is dropped at `debug`
/// (and counted) rather than blocking the live loop.
pub const LIVE_QUEUE_CAPACITY: usize = 256;

/// Upper bound on a single live artifact upload. A hung endpoint (stalled
/// connection, aggressive retry loop) releases its in-flight slot after this
/// long and logs at `debug` instead of silently wedging the pipeline.
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Minimum spacing between periodic INFO summaries of live upload health. Rather
/// than a WARN per dropped/failed upload, the live sink accumulates counts and
/// emits at most one summary line per interval (plus an immediate recovery line
/// on the first success after any failure).
pub const LIVE_SUMMARY_INTERVAL: Duration = Duration::from_secs(300);

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
    Mirror,
}

impl ArtifactStoreMode {
    /// Parse `local` / `s3` / `mirror` (case-insensitive). Unknown values return
    /// `None` so callers can reject a typo rather than silently losing S3.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "local" => Some(Self::Local),
            "s3" => Some(Self::S3),
            "mirror" => Some(Self::Mirror),
            _ => None,
        }
    }

    fn uses_s3(self) -> bool {
        matches!(self, Self::S3 | Self::Mirror)
    }
}

/// Async S3 relay: owns the object store handle plus the key prefix for one run.
struct S3Relay {
    store: Arc<dyn ObjectStore>,
    /// `<prefix>/<project>/<backtests|live>/<run-id>` — files are appended.
    key_base: String,
    /// Present for live sinks: latest-wins per-file upload accounting shared
    /// with the background upload tasks.
    live: Option<Arc<LiveMirror>>,
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

/// Per-file upload state under the live mirror lock.
#[derive(Default)]
struct FileUploadState {
    /// An upload task for this file is currently running.
    in_flight: bool,
    /// The file changed while an upload was in flight; re-upload the newest
    /// contents when the current upload finishes. At most one pending flag.
    dirty: bool,
}

/// Counters accumulated between periodic summaries. Reset each time a summary
/// is emitted.
#[derive(Default)]
struct LiveStats {
    succeeded: u64,
    failed: u64,
    timed_out: u64,
    coalesced: u64,
    dropped: u64,
}

impl LiveStats {
    fn any_activity(&self) -> bool {
        self.succeeded != 0
            || self.failed != 0
            || self.timed_out != 0
            || self.coalesced != 0
            || self.dropped != 0
    }

    fn any_trouble(&self) -> bool {
        self.failed != 0 || self.timed_out != 0 || self.dropped != 0
    }
}

/// Mutable state shared between `mirror_live` and the background upload tasks.
struct LiveState {
    /// Per-file in-flight / dirty accounting.
    files: std::collections::HashMap<String, FileUploadState>,
    /// Number of distinct files with an upload in flight, bounded by
    /// [`LIVE_QUEUE_CAPACITY`].
    inflight_files: usize,
    /// Counters since the last summary.
    stats: LiveStats,
    /// Consecutive upload failures (error or timeout) not yet followed by a
    /// success. Non-zero means the endpoint is currently unhealthy; the next
    /// success emits a recovery line.
    failures_since_success: u64,
    /// When the last periodic summary was emitted.
    last_summary: Instant,
}

impl Default for LiveState {
    fn default() -> Self {
        Self {
            files: std::collections::HashMap::new(),
            inflight_files: 0,
            stats: LiveStats::default(),
            failures_since_success: 0,
            last_summary: Instant::now(),
        }
    }
}

/// Latest-wins per-file live upload coordinator. Holds the shared state behind
/// a mutex so `mirror_live` and finishing upload tasks agree on what is in
/// flight and what still needs uploading.
struct LiveMirror {
    state: Mutex<LiveState>,
}

/// Outcome of a single live upload attempt, used to update stats/health.
enum UploadOutcome {
    Ok,
    Failed,
    TimedOut,
}

impl LiveMirror {
    fn new() -> Self {
        Self {
            state: Mutex::new(LiveState::default()),
        }
    }

    /// Try to begin an upload for `file_name`. Returns `true` if the caller now
    /// owns the (single) in-flight slot for this file and must drive an upload;
    /// `false` if it coalesced into an already-running upload or was dropped
    /// because the distinct-file cap is full.
    fn begin(&self, file_name: &str) -> bool {
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.files.get_mut(file_name) {
            if entry.in_flight {
                // Already uploading: mark dirty (idempotent) and coalesce.
                if !entry.dirty {
                    entry.dirty = true;
                }
                state.stats.coalesced += 1;
                return false;
            }
        }
        if state.inflight_files >= LIVE_QUEUE_CAPACITY {
            // Too many *distinct* files in flight. Drop this first-touch rather
            // than block the live loop.
            state.stats.dropped += 1;
            return false;
        }
        let entry = state.files.entry(file_name.to_string()).or_default();
        entry.in_flight = true;
        entry.dirty = false;
        state.inflight_files += 1;
        true
    }

    /// Record an upload result and decide whether this file needs another pass
    /// (because it was marked dirty mid-flight). Returns `true` if the caller
    /// should immediately re-upload the newest contents of `file_name` while
    /// keeping the in-flight slot; `false` if the slot is now released.
    fn finish(&self, file_name: &str, outcome: UploadOutcome) -> bool {
        let mut recovery: Option<String> = None;
        let requeue;
        let summary;
        {
            let mut state = self.state.lock().unwrap();
            match outcome {
                UploadOutcome::Ok => {
                    state.stats.succeeded += 1;
                    if state.failures_since_success != 0 {
                        recovery = Some(format!(
                            "live artifact S3 mirror recovered after {} failed upload(s)",
                            state.failures_since_success
                        ));
                        state.failures_since_success = 0;
                    }
                }
                UploadOutcome::Failed => {
                    state.stats.failed += 1;
                    state.failures_since_success += 1;
                }
                UploadOutcome::TimedOut => {
                    state.stats.timed_out += 1;
                    state.failures_since_success += 1;
                }
            }

            let dirty = state.files.get(file_name).map(|e| e.dirty).unwrap_or(false);
            if dirty {
                // Keep the in-flight slot; re-upload the newest contents.
                if let Some(entry) = state.files.get_mut(file_name) {
                    entry.dirty = false;
                    entry.in_flight = true;
                }
                requeue = true;
            } else {
                if let Some(entry) = state.files.get_mut(file_name) {
                    entry.in_flight = false;
                }
                state.inflight_files = state.inflight_files.saturating_sub(1);
                requeue = false;
            }

            summary = Self::maybe_take_summary(&mut state);
        }
        if let Some(line) = recovery {
            tracing::info!("{line}");
        }
        if let Some(line) = summary {
            tracing::info!("{line}");
        }
        requeue
    }

    /// If enough time has passed since the last summary and there was activity,
    /// format the summary line and reset the counters. Returns the line to log
    /// (outside the lock).
    fn maybe_take_summary(state: &mut LiveState) -> Option<String> {
        if state.last_summary.elapsed() < LIVE_SUMMARY_INTERVAL {
            return None;
        }
        if !state.stats.any_activity() {
            state.last_summary = Instant::now();
            return None;
        }
        let s = &state.stats;
        let line = format!(
            "live artifact S3 mirror: {} succeeded, {} failed, {} timed out, {} coalesced, {} dropped in the last {:?}{}",
            s.succeeded,
            s.failed,
            s.timed_out,
            s.coalesced,
            s.dropped,
            LIVE_SUMMARY_INTERVAL,
            if state.stats.any_trouble() && state.failures_since_success != 0 {
                " (endpoint still degraded)"
            } else {
                ""
            }
        );
        state.stats = LiveStats::default();
        state.last_summary = Instant::now();
        Some(line)
    }

    /// True when no uploads are in flight — used by `flush_all` to wait for the
    /// mirror to quiesce.
    fn is_idle(&self) -> bool {
        self.state.lock().unwrap().inflight_files == 0
    }
}

/// The one abstraction consumed by both artifact writers.
pub struct RunArtifactSink {
    mode: ArtifactStoreMode,
    /// The dir the writers actually write files into. For `Local`/`Mirror` this
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

        // Start the dedicated upload runtime eagerly so its worker threads
        // exist before the first mirror call.
        let _ = upload_runtime();

        // Live sinks coordinate latest-wins per-file uploads: at most one
        // upload in flight per file plus one pending dirty flag, so duplicate
        // snapshots of the same file coalesce instead of stampeding the queue.
        let live = (kind == RunKind::Live).then(|| Arc::new(LiveMirror::new()));

        let relay = Arc::new(S3Relay {
            store,
            key_base,
            live,
        });
        tracing::info!(
            "artifact S3 mirroring active: mode={:?} kind={:?} key_base={}",
            mode,
            kind,
            relay.key_base
        );

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

    /// True when the working dir is the real, persistent run dir (local / mirror).
    /// False in `s3`-only mode, where the working dir is a temp buffer that is
    /// deleted after upload — callers should not, e.g., point a `latest` symlink
    /// at it.
    pub fn writes_local_run_dir(&self) -> bool {
        self.mode != ArtifactStoreMode::S3
    }

    /// Mirror the named file (relative to the working dir) to S3.
    ///
    /// - Local mode: no-op.
    /// - Backtest (Mirror/S3): throttled — uploads at most once per
    ///   [`CHECKPOINT_INTERVAL`] per file. Reads the current file contents.
    /// - Live (Mirror/S3): latest-wins. If no upload is in flight for this file
    ///   it spawns a background upload of the current contents; if one is
    ///   already running it just marks the file dirty and returns (the finishing
    ///   task re-uploads the newest contents). Never blocks the caller.
    pub fn mirror(&self, file_name: &str) {
        let Some(relay) = self.relay.as_ref() else {
            return;
        };
        if relay.live.is_some() {
            self.mirror_live(relay, file_name);
        } else {
            self.mirror_checkpoint(relay, file_name);
        }
    }

    fn mirror_live(&self, relay: &Arc<S3Relay>, file_name: &str) {
        let Some(live) = relay.live.clone() else {
            return;
        };
        // Latest-wins accounting: if an upload is already in flight for this
        // file we only mark it dirty (coalesce) and return; otherwise we claim
        // the single in-flight slot for this file and drive the upload. Either
        // way the live loop never waits on S3.
        if !live.begin(file_name) {
            return;
        }
        let relay = relay.clone();
        let working_dir = self.working_dir.clone();
        let file_name = file_name.to_string();
        upload_runtime().spawn(async move {
            // Loop so a file marked dirty mid-flight is re-uploaded with its
            // newest contents without releasing the in-flight slot.
            loop {
                let key = relay.key_for(&file_name);
                // Read the latest contents at upload time (not enqueue time).
                let Some(bytes) = std::fs::read(working_dir.join(&file_name)).ok() else {
                    // File vanished; treat as a completed (no-op) upload so the
                    // slot is released and any dirty flag re-evaluated.
                    if live.finish(&file_name, UploadOutcome::Ok) {
                        continue;
                    }
                    break;
                };
                let outcome =
                    match tokio::time::timeout(UPLOAD_TIMEOUT, relay.put(&key, bytes)).await {
                        Ok(Ok(())) => {
                            tracing::debug!("live artifact uploaded: {key}");
                            UploadOutcome::Ok
                        }
                        Ok(Err(err)) => {
                            tracing::debug!("live artifact S3 upload failed for {key}: {err}");
                            UploadOutcome::Failed
                        }
                        Err(_) => {
                            tracing::debug!(
                            "live artifact S3 upload timed out for {key} after {UPLOAD_TIMEOUT:?}"
                        );
                            UploadOutcome::TimedOut
                        }
                    };
                if !live.finish(&file_name, outcome) {
                    break;
                }
            }
        });
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
    /// checkpoint throttle, after waiting (bounded) for in-flight live uploads.
    /// Call on completion / clean shutdown. No-op for local-only sinks.
    pub fn flush_all(&self) {
        let Some(relay) = self.relay.as_ref() else {
            return;
        };

        // Let in-flight live uploads finish first so they cannot race (and
        // overwrite) the final full upload below. Bounded wait: a wedged upload
        // times out on its own (UPLOAD_TIMEOUT) and must not hang shutdown, so
        // we poll for the mirror to go idle up to one timeout's worth plus a
        // small margin, then proceed regardless.
        if let Some(live) = relay.live.clone() {
            run_blocking(async move {
                let deadline = Instant::now() + UPLOAD_TIMEOUT + Duration::from_secs(1);
                while !live.is_idle() && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            });
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
            ArtifactStoreMode::parse(" Mirror "),
            Some(ArtifactStoreMode::Mirror)
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
    async fn mirror_mode_mirrors_on_flush() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("backtests").join("run1");
        std::fs::create_dir_all(&dir).unwrap();
        let (sink, store) = in_memory_sink(
            ArtifactStoreMode::Mirror,
            RunKind::Backtest,
            dir.clone(),
            "proj",
            "run1",
            "runs",
        );
        write_file(&dir, "progress.json", "{\"p\":1}");
        write_file(&dir, "code/main.py", "print(1)");
        sink.flush_all();

        // Local files still present (Mirror is local-primary).
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
            ArtifactStoreMode::Mirror,
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
            ArtifactStoreMode::Mirror,
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

    /// A gated, optionally-failing object store used to drive the live upload
    /// path deterministically. `put`s block on `gate` (a semaphore that starts
    /// with zero permits) so a test can hold uploads in flight, count how many
    /// PUTs were accepted, and inspect the payload of the last accepted PUT.
    /// When `fail` is set, `put`s return an error instead of storing.
    #[derive(Debug)]
    struct GatedStore {
        gate: Arc<tokio::sync::Semaphore>,
        /// Incremented when a put *reaches* the store, before waiting on the
        /// gate — counts distinct in-flight uploads even while they block.
        started: Arc<std::sync::atomic::AtomicUsize>,
        /// Incremented when a put gets past the gate (i.e. actually completes).
        accepted: Arc<std::sync::atomic::AtomicUsize>,
        fail: Arc<std::sync::atomic::AtomicBool>,
        last_body: Arc<Mutex<Option<String>>>,
    }

    impl GatedStore {
        fn new(gate: Arc<tokio::sync::Semaphore>) -> Self {
            Self {
                gate,
                started: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                accepted: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                fail: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                last_body: Arc::new(Mutex::new(None)),
            }
        }
    }

    impl std::fmt::Display for GatedStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "GatedStore")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for GatedStore {
        async fn put_opts(
            &self,
            _location: &ObjectPath,
            payload: PutPayload,
            _opts: object_store::PutOptions,
        ) -> object_store::Result<object_store::PutResult> {
            use std::sync::atomic::Ordering;
            self.started.fetch_add(1, Ordering::SeqCst);
            // Wait for a permit so the test controls when a put completes. A
            // closed gate returns Err and lets the put proceed (drain mode).
            let _ = self.gate.acquire().await;
            self.accepted.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err(object_store::Error::Generic {
                    store: "GatedStore",
                    source: "injected failure".into(),
                });
            }
            let bytes = payload.iter().flat_map(|b| b.to_vec()).collect::<Vec<u8>>();
            *self.last_body.lock().unwrap() = Some(String::from_utf8(bytes).unwrap());
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

    #[tokio::test(flavor = "multi_thread")]
    async fn live_same_file_coalesces_and_converges_to_latest() {
        // Many rapid mirrors of the SAME file against a gated (slow) store must
        // produce only a bounded number of PUTs — one in flight plus a single
        // coalesced dirty retry — never one per call. The caller never blocks,
        // and once the store is released the final stored contents equal the
        // LAST written version (latest-wins convergence).
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("live").join("deploy1");
        std::fs::create_dir_all(&dir).unwrap();
        // Zero permits: the first put blocks until we release the gate.
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let store = Arc::new(GatedStore::new(gate.clone()));
        let accepted = store.accepted.clone();
        let last_body = store.last_body.clone();
        let sink = RunArtifactSink::with_store(
            ArtifactStoreMode::Mirror,
            RunKind::Live,
            dir.clone(),
            "proj",
            "deploy1",
            "runs",
            store,
        );

        // Fire many mirrors of the same file. Each rewrites the file first.
        for i in 0..500u32 {
            write_file(&dir, "portfolio.json", &format!("v{i}"));
            sink.mirror("portfolio.json");
        }
        // The loop finishing at all proves the caller never blocked. With the
        // gate closed (zero permits), exactly one put is in flight and blocked;
        // all 499 further calls coalesced into a single dirty flag.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            0,
            "no put should have completed while the gate is closed"
        );

        // Write the definitive last version, then release uploads. The in-flight
        // upload completes (put #1), sees the dirty flag, and re-uploads the
        // newest contents (put #2). Nowhere near 500 PUTs.
        write_file(&dir, "portfolio.json", "final");
        gate.add_permits(1000);
        sink.flush_all();

        let puts = accepted.load(Ordering::SeqCst);
        assert!(
            puts <= 4,
            "expected a small bounded number of PUTs (1 in-flight + dirty retry + flush), got {puts}"
        );
        assert_eq!(last_body.lock().unwrap().clone(), Some("final".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_error_burst_then_heal_converges() {
        // The store fails puts for a while, then heals. The mirror must keep
        // accepting mirrors (no wedge, no per-event WARN) and converge to the
        // latest contents once puts succeed again.
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("live").join("deploy1");
        std::fs::create_dir_all(&dir).unwrap();
        // Plenty of permits so puts run immediately; they just fail at first.
        let gate = Arc::new(tokio::sync::Semaphore::new(usize::MAX >> 4));
        let store = Arc::new(GatedStore::new(gate.clone()));
        let fail = store.fail.clone();
        let accepted = store.accepted.clone();
        let last_body = store.last_body.clone();
        fail.store(true, Ordering::SeqCst);
        let sink = RunArtifactSink::with_store(
            ArtifactStoreMode::Mirror,
            RunKind::Live,
            dir.clone(),
            "proj",
            "deploy1",
            "runs",
            store,
        );

        // Burst of mirrors while the store is failing.
        for i in 0..50u32 {
            write_file(&dir, "portfolio.json", &format!("bad{i}"));
            sink.mirror("portfolio.json");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        // Some puts were attempted and failed; the sink is not wedged.
        for _ in 0..200 {
            if accepted.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(accepted.load(Ordering::SeqCst) > 0, "puts were attempted");

        // Heal the store and write the definitive latest contents, then mirror.
        fail.store(false, Ordering::SeqCst);
        write_file(&dir, "portfolio.json", "healed");
        sink.mirror("portfolio.json");
        sink.flush_all();

        // Poll for convergence to the latest contents.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if last_body.lock().unwrap().clone() == Some("healed".to_string()) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "mirror never converged to latest contents after heal: {:?}",
                last_body.lock().unwrap().clone()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn live_distinct_file_cap_drops_without_blocking() {
        // Hundreds of DISTINCT files against a gated store fill the distinct-file
        // cap. Beyond it, first-touches of new files are dropped (counted, not
        // WARNed) and the caller never blocks.
        use std::sync::atomic::Ordering;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("live").join("deploy1");
        std::fs::create_dir_all(&dir).unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let store = Arc::new(GatedStore::new(gate.clone()));
        let started = store.started.clone();
        let sink = RunArtifactSink::with_store(
            ArtifactStoreMode::Mirror,
            RunKind::Live,
            dir.clone(),
            "proj",
            "deploy1",
            "runs",
            store,
        );

        // Fire more distinct files than the cap; each blocks on the gate.
        for i in 0..(LIVE_QUEUE_CAPACITY + 50) {
            let name = format!("file{i}.json");
            write_file(&dir, &name, "x");
            sink.mirror(&name);
        }
        // At most LIVE_QUEUE_CAPACITY distinct uploads reach the store (each
        // blocks on the gate); the rest were dropped without blocking the caller
        // (the loop completing at all proves it).
        for _ in 0..200 {
            if started.load(Ordering::SeqCst) >= LIVE_QUEUE_CAPACITY {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(started.load(Ordering::SeqCst), LIVE_QUEUE_CAPACITY);

        gate.close();
        sink.flush_all();
    }

    /// Reproduction of the live path: construct the sink exactly the way the
    /// live runner does (RunKind::Live, Mirror mode, from inside a multi-thread
    /// tokio runtime), mirror a snapshot, and assert the upload lands WITHOUT
    /// any explicit flush — i.e. the background uploads actually drain.
    #[tokio::test(flavor = "multi_thread")]
    async fn live_sink_drains_in_background_without_flush() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("live").join("deploy1");
        std::fs::create_dir_all(&dir).unwrap();
        let (sink, store) = in_memory_sink(
            ArtifactStoreMode::Mirror,
            RunKind::Live,
            dir.clone(),
            "uw_control",
            "deploy1",
            "runs",
        );
        write_file(&dir, "portfolio.json", "snap1");
        write_file(&dir, "heartbeat.log", "beat");
        sink.mirror("portfolio.json");
        sink.mirror("heartbeat.log");

        // No flush: poll until the background uploads land.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let keys = list_keys(&store).await;
            if keys.contains(&"runs/uw_control/live/deploy1/portfolio.json".to_string())
                && keys.contains(&"runs/uw_control/live/deploy1/heartbeat.log".to_string())
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "live uploads never drained in the background: {keys:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            get_key(&store, "runs/uw_control/live/deploy1/portfolio.json").await,
            "snap1"
        );
    }
}
