use anyhow::{Context, Result};
use arrow::util::pretty::pretty_format_batches;
use arrow_array::{Array, RecordBatch};
use clap::{Args, Subcommand};
use futures::TryStreamExt;
use verglas_sdk::Database;

#[derive(Args)]
pub(crate) struct RunsArgs {
    #[command(subcommand)]
    command: RunsCommand,
}

#[derive(Subcommand)]
enum RunsCommand {
    /// List the latest state of recent backtests.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Show one backtest and its statistics.
    Show {
        run_id: String,
        #[arg(long)]
        json: bool,
    },
}

pub(crate) async fn run(args: RunsArgs) -> Result<()> {
    let config = crate::config::GlobalConfig::load()?;
    let client = crate::runtime::require_verglas(&config)
        .await
        .context("connect to the Verglas catalog through the local cache")?;
    match args.command {
        RunsCommand::List { limit, json } => {
            let sql = format!(
                "SELECT run_id, strategy, status, started_at, finished_at, final_value \
                 FROM (SELECT *, row_number() OVER (PARTITION BY run_id ORDER BY recorded_at DESC) AS rn \
                 FROM rlean.runs) WHERE rn = 1 ORDER BY started_at DESC LIMIT {}",
                limit.min(10_000)
            );
            render(query(&client, &sql).await?, json)
        }
        RunsCommand::Show { run_id, json } => {
            let run_id = run_id.replace('\'', "''");
            let sql = format!(
                "SELECT 'run' AS section, status AS name, \
                 concat('strategy=', strategy, ', started_at=', started_at, \
                 ', finished_at=', coalesce(finished_at, ''), ', final_value=', \
                 coalesce(cast(final_value AS varchar), '')) AS value \
                 FROM rlean.runs WHERE run_id = '{run_id}' \
                 UNION ALL SELECT 'statistic', name, value FROM rlean.statistics \
                 WHERE run_id = '{run_id}'"
            );
            render(query(&client, &sql).await?, json)
        }
    }
}

async fn query(client: &Database, sql: &str) -> Result<Vec<RecordBatch>> {
    client
        .query_stream(sql)
        .await
        .context("query Verglas run catalog")?
        .try_collect()
        .await
        .context("read Verglas query stream")
}

fn render(batches: Vec<RecordBatch>, json: bool) -> Result<()> {
    if json {
        let mut rows = Vec::new();
        for batch in &batches {
            for row_index in 0..batch.num_rows() {
                let mut row = serde_json::Map::new();
                for (column_index, field) in batch.schema().fields().iter().enumerate() {
                    let array = batch.column(column_index);
                    let value = if array.is_null(row_index) {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(arrow_cast::display::array_value_to_string(
                            array.as_ref(),
                            row_index,
                        )?)
                    };
                    row.insert(field.name().clone(), value);
                }
                rows.push(serde_json::Value::Object(row));
            }
        }
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else if batches.iter().all(|batch| batch.num_rows() == 0) {
        println!("No runs found.");
    } else {
        println!("{}", pretty_format_batches(&batches)?);
    }
    Ok(())
}
