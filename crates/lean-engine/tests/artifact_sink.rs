//! Integration tests: the artifact writers drive the RunArtifactSink and the
//! expected keys land in an in-memory object store. No cloud credentials.

use std::sync::Arc;

use futures::StreamExt;
use lean_engine::artifacts::{in_memory_sink, ArtifactStoreMode, RunKind};
use lean_engine::live::deployment_writer::LiveDeploymentWriter;
use lean_engine::runner::stream_writer::BacktestStreamWriter;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::ObjectStore;
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

    // First progress flush checkpoints immediately (no prior upload timestamp).
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
    assert!(uploaded.contains("\"status\": \"running\""));

    // Completion: mark + flush uploads the final state of every file.
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
    assert!(final_progress.contains("\"status\": \"completed\""));

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
