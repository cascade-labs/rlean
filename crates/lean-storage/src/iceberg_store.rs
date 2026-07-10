use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

use anyhow::{anyhow, Context, Result};
use arrow::compute;
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, LargeStringArray, RecordBatch,
    StringArray, UInt64Array,
};
use arrow_cast::cast;
use arrow_data::{ByteView, MAX_INLINE_VIEW_LEN};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::ParquetReadOptions;
use datafusion::prelude::*;
use iceberg::io::{LocalFsStorageFactory, Storage, StorageConfig, StorageFactory};
use iceberg::spec::{
    DataFileFormat, Literal, NestedField, PartitionKey, PartitionSpec, PrimitiveType, Schema,
    Struct, Transform, Type,
};
use iceberg::transaction::{ApplyTransactionAction, Transaction};
use iceberg::writer::base_writer::data_file_writer::DataFileWriterBuilder;
use iceberg::writer::file_writer::location_generator::{
    DefaultFileNameGenerator, DefaultLocationGenerator,
};
use iceberg::writer::file_writer::rolling_writer::RollingFileWriterBuilder;
use iceberg::writer::file_writer::ParquetWriterBuilder;
use iceberg::writer::partitioning::fanout_writer::FanoutWriter;
use iceberg::writer::partitioning::PartitioningWriter;
use iceberg::{
    Catalog, CatalogBuilder, Error as IcebergError, ErrorKind as IcebergErrorKind, NamespaceIdent,
    TableCreation, TableIdent,
};
use iceberg_catalog_rest::{
    RestCatalogBuilder, REST_CATALOG_PROP_URI, REST_CATALOG_PROP_WAREHOUSE,
};
use iceberg_catalog_sql::{SqlBindStyle, SqlCatalogBuilder};
use iceberg_datafusion::IcebergTableProviderFactory;
use iceberg_storage_opendal::OpenDalStorageFactory;
use lean_core::{Resolution, SecurityType, Symbol, TickType};
use lean_data::{
    CustomDataPoint, CustomDataQuery, MarginInterestRate, PerpetualContext, QuoteBar, Tick,
    TradeBar,
};
use object_store::aws::AmazonS3Builder;
use parquet::arrow::PARQUET_FIELD_ID_META_KEY;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::prelude::ToPrimitive;
use sqlx::migrate::MigrateDatabase;
use sqlx::sqlite::Sqlite;
use url::Url;

static APPEND_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

use crate::convert;
use crate::partition_index::{
    CustomPartitionFields, CustomPartitionIndex, MarketPartitionDayQuery, MarketPartitionFields,
    MarketPartitionIndex,
};
use crate::schema::{self, FactorFileEntry, MapFileEntry, OptionUniverseRow};
use crate::QueryParams;

const CATALOG_NAME: &str = "rlean";
const NAMESPACE: &str = "lean";

pub const MARKET_TRADE_BARS: &str = "market_trade_bars";
pub const MARKET_QUOTE_BARS: &str = "market_quote_bars";
pub const MARKET_TICKS: &str = "market_ticks";
pub const OPTION_EOD_BARS: &str = "option_eod_bars";
pub const OPTION_UNIVERSE: &str = "option_universe";
pub const MARGIN_INTEREST: &str = "margin_interest";
pub const PERPETUAL_CONTEXT: &str = "perpetual_context";
pub const CUSTOM_POINTS: &str = "custom_points";
pub const FACTOR_FILES: &str = "factor_files";
pub const MAP_FILES: &str = "map_files";

/// Every table the local cache manages, in a stable order. Used by maintenance
/// tooling and the end-of-backtest compaction pass.
pub const CACHE_TABLES: &[&str] = &[
    MARKET_TRADE_BARS,
    MARKET_QUOTE_BARS,
    MARKET_TICKS,
    OPTION_EOD_BARS,
    OPTION_UNIVERSE,
    MARGIN_INTEREST,
    PERPETUAL_CONTEXT,
    CUSTOM_POINTS,
    FACTOR_FILES,
    MAP_FILES,
];

/// Byte budget for a single staged compaction chunk (and therefore for any
/// batch read back from it). Kept well under Arrow's 2GB 32-bit string offset
/// limit so re-reading a chunk cannot overflow.
const COMPACT_CHUNK_BYTES: usize = 512 * 1024 * 1024;
/// Row ceiling for a single staged compaction chunk.
const COMPACT_CHUNK_ROWS: usize = 4_000_000;
/// Byte budget for one re-insert append commit. Bounds peak memory and keeps
/// the concatenated batch's string columns under the 2GB offset limit.
const COMPACT_APPEND_BYTES: usize = 512 * 1024 * 1024;
/// Row ceiling for one re-insert append commit.
const COMPACT_APPEND_ROWS: usize = 4_000_000;
/// Cap on distinct partitions per re-insert append. The fanout writer keeps one
/// open file per partition until the commit closes, so this bounds open files.
const COMPACT_APPEND_PARTITIONS: usize = 2_048;

/// Approximate in-memory footprint of a record batch (sum of column buffer
/// sizes). For sliced batches this over-counts shared buffers, which only makes
/// the compaction budgets more conservative.
fn record_batch_bytes(batch: &RecordBatch) -> usize {
    batch
        .columns()
        .iter()
        .map(|column| column.get_array_memory_size())
        .sum()
}

fn concat_record_batches(batches: &[RecordBatch]) -> Result<RecordBatch> {
    let Some(first) = batches.first() else {
        return Ok(RecordBatch::new_empty(Arc::new(ArrowSchema::empty())));
    };
    Ok(compute::concat_batches(&first.schema(), batches)?)
}

/// Result of compacting one Iceberg table via [`IcebergStore::compact_table`].
#[derive(Debug, Clone)]
pub struct CompactionStats {
    pub table: String,
    /// Number of live data files referenced by the snapshot before compaction.
    pub files_before: usize,
    /// Number of rows preserved.
    pub rows: usize,
    /// Number of append commits (snapshots + manifests) written afterward.
    pub appends: usize,
}

/// Iceberg REST catalog (Lakekeeper) connection settings for `data_store = s3`.
///
/// The S3 data store always resolves its tables through a REST catalog; the
/// resolver in the `rlean` crate enforces the presence of these before
/// constructing the store. (Local mode uses its own SQLite catalog and does not
/// take this.)
#[derive(Clone, Debug)]
pub struct RestCatalogConnection {
    /// REST catalog base URL, e.g. `http://localhost:8181/catalog`.
    pub uri: String,
    /// Lakekeeper warehouse name the catalog resolves for this run.
    pub warehouse: String,
}

/// S3 FileIO settings for an S3-backed market-data warehouse.
///
/// Path-style addressing is always used (the bucket name may contain characters
/// that break virtual-host addressing). These props are also merged into the
/// FileIO the REST catalog constructs, so the store never depends on Lakekeeper
/// vending a complete-and-correct S3 config.
#[derive(Clone, Debug)]
pub struct S3DataStoreConfig {
    /// Iceberg warehouse root, e.g. `s3://bucket/prefix`. Used to derive the
    /// DataFusion object-store bucket; the catalog resolves data-file locations.
    pub warehouse: String,
    pub endpoint: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

/// Where an [`IcebergStore`]'s data and metadata live.
#[derive(Clone)]
enum WarehouseBackend {
    /// Local filesystem warehouse rooted at `root`.
    Local { root: PathBuf },
    /// S3-compatible object store warehouse.
    S3 {
        warehouse: String,
        s3: S3DataStoreConfig,
    },
}

#[derive(Clone)]
pub struct IcebergStore {
    backend: WarehouseBackend,
    catalog: Arc<dyn Catalog>,
    namespace: NamespaceIdent,
    table_contexts: Arc<Mutex<HashMap<String, Arc<IcebergTableContext>>>>,
    partition_indexes: Arc<Mutex<HashMap<String, Arc<MarketPartitionIndex>>>>,
    custom_partition_indexes: Arc<Mutex<HashMap<String, Arc<CustomPartitionIndex>>>>,
    table_write_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

struct IcebergTableContext {
    ctx: SessionContext,
}

fn is_catalog_commit_conflict(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        if let Some(error) = cause.downcast_ref::<IcebergError>() {
            return error.kind() == IcebergErrorKind::CatalogCommitConflicts;
        }
        let message = cause.to_string();
        message.contains("CatalogCommitConflicts") || message.contains("snapshot has changed")
    })
}

impl IcebergStore {
    /// Connect to a local-filesystem warehouse via the Iceberg SQL (SQLite)
    /// catalog.
    ///
    /// This is the `data_store = local` path: a plain on-disk warehouse under
    /// `data_root/iceberg` with a co-located SQLite catalog. It is deliberately
    /// independent of Lakekeeper — the REST catalog cannot back a local-FS
    /// warehouse (it has no filesystem storage profile), so local mode keeps its
    /// own single-node catalog.
    pub async fn connect_local(data_root: impl AsRef<Path>) -> Result<Self> {
        let warehouse_root = data_root.as_ref().join("iceberg");
        tokio::fs::create_dir_all(&warehouse_root)
            .await
            .with_context(|| format!("failed to create {}", warehouse_root.display()))?;

        let catalog_db = warehouse_root.join("catalog.db");
        let catalog_uri = format!("sqlite:{}", catalog_db.display());
        if !Sqlite::database_exists(&catalog_uri).await.unwrap_or(false) {
            Sqlite::create_database(&catalog_uri)
                .await
                .with_context(|| {
                    format!("failed to create Iceberg catalog {}", catalog_db.display())
                })?;
        }

        let warehouse = path_to_file_uri(&warehouse_root)?;
        let catalog = SqlCatalogBuilder::default()
            .with_storage_factory(Arc::new(LocalFsStorageFactory))
            .uri(catalog_uri)
            .warehouse_location(warehouse)
            .sql_bind_style(SqlBindStyle::QMark)
            .load(CATALOG_NAME, HashMap::new())
            .await
            .context("failed to load Iceberg SQL catalog")?;

        let store = Self {
            backend: WarehouseBackend::Local {
                root: warehouse_root,
            },
            catalog: Arc::new(catalog),
            namespace: NamespaceIdent::new(NAMESPACE.into()),
            table_contexts: Arc::new(Mutex::new(HashMap::new())),
            partition_indexes: Arc::new(Mutex::new(HashMap::new())),
            custom_partition_indexes: Arc::new(Mutex::new(HashMap::new())),
            table_write_locks: Arc::new(Mutex::new(HashMap::new())),
        };
        store.ensure_tables().await?;
        Ok(store)
    }

    /// Connect to an S3-backed warehouse through the Iceberg REST catalog.
    ///
    /// The REST catalog at `catalog.uri` owns table metadata; `config` supplies
    /// the S3 FileIO props (endpoint/region/credentials, path-style) used both
    /// for the catalog's own FileIO and for the DataFusion object store.
    pub async fn connect_s3(
        config: S3DataStoreConfig,
        catalog: RestCatalogConnection,
    ) -> Result<Self> {
        let s3_props = s3_file_io_props(&config);
        let rest =
            build_rest_catalog(&catalog, Arc::new(s3_storage_factory(&config)), s3_props).await?;

        let store = Self {
            backend: WarehouseBackend::S3 {
                warehouse: config.warehouse.clone(),
                s3: config,
            },
            catalog: Arc::new(rest),
            namespace: NamespaceIdent::new(NAMESPACE.into()),
            table_contexts: Arc::new(Mutex::new(HashMap::new())),
            partition_indexes: Arc::new(Mutex::new(HashMap::new())),
            custom_partition_indexes: Arc::new(Mutex::new(HashMap::new())),
            table_write_locks: Arc::new(Mutex::new(HashMap::new())),
        };
        store.ensure_tables().await?;
        Ok(store)
    }

    /// Local warehouse root, if this store is backed by the local filesystem.
    /// Returns `None` for S3-backed stores (there is no local warehouse dir).
    pub fn warehouse_root(&self) -> Option<&Path> {
        match &self.backend {
            WarehouseBackend::Local { root } => Some(root.as_path()),
            WarehouseBackend::S3 { .. } => None,
        }
    }

    /// Client-assigned Iceberg table `location` for the local (SQLite-catalog)
    /// warehouse. `None` in S3 mode: the Lakekeeper REST catalog owns the
    /// warehouse and assigns each table's location itself.
    fn table_location(&self, table: &str) -> Result<Option<String>> {
        match &self.backend {
            WarehouseBackend::Local { root } => {
                Ok(Some(path_to_file_uri(&root.join(NAMESPACE).join(table))?))
            }
            WarehouseBackend::S3 { .. } => Ok(None),
        }
    }

    /// The Iceberg storage factory matching this store's backend.
    fn iceberg_storage_factory(&self) -> Arc<dyn StorageFactory> {
        match &self.backend {
            WarehouseBackend::Local { .. } => Arc::new(LocalFsStorageFactory),
            WarehouseBackend::S3 { s3, .. } => Arc::new(s3_storage_factory(s3)),
        }
    }

    /// Register an S3 object store on `ctx` so DataFusion can read `s3://`
    /// data-file paths. A no-op for local-backed stores.
    fn register_s3_object_store(&self, ctx: &SessionContext) -> Result<()> {
        let WarehouseBackend::S3 { warehouse, s3 } = &self.backend else {
            return Ok(());
        };
        let bucket = s3_bucket(warehouse)?;
        // Plain-HTTP endpoints (e.g. a local MinIO / Lakekeeper warehouse) are
        // rejected by `object_store` unless HTTP is explicitly allowed; HTTPS
        // endpoints (OCI, AWS) are unaffected.
        let allow_http = s3.endpoint.starts_with("http://");
        let store = AmazonS3Builder::new()
            .with_bucket_name(&bucket)
            .with_endpoint(&s3.endpoint)
            .with_region(&s3.region)
            .with_access_key_id(&s3.access_key)
            .with_secret_access_key(&s3.secret_key)
            .with_virtual_hosted_style_request(false)
            .with_allow_http(allow_http)
            .build()
            .context("failed to build S3 object store for DataFusion")?;
        let url = Url::parse(&format!("s3://{bucket}"))
            .with_context(|| format!("invalid s3 bucket url for '{bucket}'"))?;
        ctx.register_object_store(&url, Arc::new(store));
        Ok(())
    }

    /// Translate an Iceberg data-file path into a path DataFusion's
    /// `read_parquet` can open. Local paths are stripped of their `file://`
    /// scheme; `s3://` paths are returned unchanged (an S3 object store must be
    /// registered on the reading context via [`register_s3_object_store`]).
    fn warehouse_file_path(&self, iceberg_path: &str) -> Result<String> {
        match &self.backend {
            WarehouseBackend::Local { .. } => local_path_from_iceberg_file_path(iceberg_path),
            WarehouseBackend::S3 { .. } => Ok(iceberg_path.to_string()),
        }
    }

    /// Number of persisted table-metadata versions (`*.metadata.json`) for
    /// `table`. iceberg-rust keeps every version and writes a new one on every
    /// commit, so this count grows by one per append and is a direct measure of
    /// the snapshot/manifest bloat that slows query planning. A freshly
    /// compacted (or absent) table reports a small handful.
    pub fn metadata_version_count(&self, table: &str) -> usize {
        // Metadata bloat is only measured (and compacted) for local warehouses;
        // S3 warehouse compaction is out of scope here, so report 0 so the
        // auto-compaction pass simply skips S3-backed tables.
        let WarehouseBackend::Local { root } = &self.backend else {
            return 0;
        };
        let dir = root.join(NAMESPACE).join(table).join("metadata");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return 0;
        };
        entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".metadata.json")
            })
            .count()
    }

    /// Compact every managed cache table whose metadata bloat
    /// ([`IcebergStore::metadata_version_count`]) is at or above `min_versions`.
    ///
    /// Intended for the end of a backtest, once results are safely written:
    /// each append-heavy run leaves the touched tables with hundreds of tiny
    /// snapshots, and rewriting them keeps the next run's query planning fast.
    /// The threshold skips tables that were not (meaningfully) appended to so a
    /// large table like `custom_points` is not rewritten on every run.
    ///
    /// Best-effort: a per-table failure is logged and skipped so the remaining
    /// tables are still compacted. Returns the stats of the tables actually
    /// rewritten.
    pub async fn compact_bloated(&self, min_versions: usize) -> Vec<CompactionStats> {
        let mut compacted = Vec::new();
        for table in CACHE_TABLES {
            let versions = self.metadata_version_count(table);
            if versions < min_versions {
                continue;
            }
            match self.compact_table(table).await {
                Ok(stats) if stats.files_before > 0 => {
                    tracing::info!(
                        "compacted {} ({} metadata versions): {} rows, {} live files -> {} appends",
                        stats.table,
                        versions,
                        stats.rows,
                        stats.files_before,
                        stats.appends,
                    );
                    compacted.push(stats);
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!("skipped compaction of {table}: {error:#}");
                }
            }
        }
        compacted
    }

    /// Rewrite every live row of `table` into a small number of append commits,
    /// collapsing the accumulated snapshot/manifest chain.
    ///
    /// iceberg-rust 0.9.1 has no snapshot expiration or manifest rewrite, so the
    /// append-only ingest path writes a new snapshot + manifest set on every
    /// commit. After tens of thousands of incremental appends a table ends up
    /// with tens of GB of manifests for a few hundred MB of data, and every
    /// query plan pays to read and predicate-prune across that manifest set —
    /// which manifests as a backtest that appears to hang.
    ///
    /// The rewrite streams the current rows (clustered by partition via a
    /// path-sorted single scan) into temporary parquet chunks, drops and
    /// recreates the table, then re-inserts the rows in append commits bounded
    /// by distinct partition count (to cap the fanout writer's simultaneously
    /// open files), a row budget and a byte budget (to cap memory and to keep
    /// any single string column well under Arrow's 2GB 32-bit offset limit).
    /// Row content is preserved exactly; only the physical layout and snapshot
    /// history change.
    ///
    /// If the re-insert half fails, the staged chunks are left in place under
    /// `.compact_tmp_<table>` so the data can be recovered with
    /// [`IcebergStore::restore_from_staging`].
    pub async fn compact_table(&self, table: &str) -> Result<CompactionStats> {
        use futures::StreamExt;
        use parquet::arrow::ArrowWriter;

        if matches!(self.backend, WarehouseBackend::S3 { .. }) {
            anyhow::bail!("compaction not supported for s3 data store yet");
        }

        let lock = self.table_write_lock(table).await;
        let _guard = lock.lock().await;

        let catalog_table = self.catalog.load_table(&self.ident(table)).await?;
        // With identity-transform partitions the spec field name equals the
        // source column name, so these double as the physical sort columns.
        let partition_columns: Vec<String> = catalog_table
            .metadata()
            .default_partition_spec()
            .fields()
            .iter()
            .map(|field| field.name.clone())
            .collect();

        let mut file_paths: Vec<String> = Vec::new();
        let mut tasks = catalog_table.scan().build()?.plan_files().await?;
        while let Some(task) = futures::TryStreamExt::try_next(&mut tasks).await? {
            file_paths.push(task.data_file_path().to_string());
        }
        let files_before = file_paths.len();
        if files_before == 0 {
            return Ok(CompactionStats {
                table: table.to_string(),
                files_before: 0,
                rows: 0,
                appends: 0,
            });
        }
        let mut local_paths = file_paths
            .iter()
            .map(|path| self.warehouse_file_path(path))
            .collect::<Result<Vec<_>>>()?;
        drop(catalog_table);
        // Iceberg writes each identity partition's data files under a distinct
        // `data/<partition-path>/` directory, so sorting paths clusters rows by
        // partition. Reading them in that order with a single scan partition
        // then yields a partition-contiguous stream — everything the re-insert
        // packer needs — without a costly (and memory-hungry) global sort.
        local_paths.sort();

        // --- Stage: stream the live rows into temporary parquet chunks so the
        // reset below cannot lose data and the re-insert sees ordered input.
        let staging = self.staging_dir(table);
        if staging.exists() {
            std::fs::remove_dir_all(&staging).ok();
        }
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("failed to create staging dir {}", staging.display()))?;

        let mut staged: Vec<PathBuf> = Vec::new();
        let mut rows = 0usize;
        {
            // A single scan partition preserves the supplied (path-sorted) file
            // order and reads sequentially, keeping memory to one batch.
            let ctx =
                SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
            self.register_s3_object_store(&ctx)?;
            let mut stream = ctx
                .read_parquet(local_paths, ParquetReadOptions::default())
                .await?
                .execute_stream()
                .await?;
            let mut writer: Option<ArrowWriter<std::fs::File>> = None;
            let mut writer_rows = 0usize;
            let mut writer_bytes = 0usize;
            while let Some(batch) = stream.next().await {
                let batch = batch?;
                if batch.num_rows() == 0 {
                    continue;
                }
                rows += batch.num_rows();
                if writer.is_none() {
                    let path = staging.join(format!("part-{:06}.parquet", staged.len()));
                    let file = std::fs::File::create(&path)
                        .with_context(|| format!("failed to create {}", path.display()))?;
                    writer = Some(ArrowWriter::try_new(file, batch.schema(), None)?);
                    staged.push(path);
                    writer_rows = 0;
                    writer_bytes = 0;
                }
                let active = writer.as_mut().expect("staging writer present");
                active.write(&batch)?;
                writer_rows += batch.num_rows();
                writer_bytes += record_batch_bytes(&batch);
                // Roll chunks by byte size (with a row ceiling) so no chunk — and
                // hence no re-read batch — can hold a >2GB string column.
                if writer_bytes >= COMPACT_CHUNK_BYTES || writer_rows >= COMPACT_CHUNK_ROWS {
                    writer.take().expect("staging writer present").close()?;
                }
            }
            if let Some(active) = writer.take() {
                active.close()?;
            }
        }

        // --- Reset: drop + recreate the table, discarding every old snapshot,
        // manifest and tiny data file.
        self.reset_table(table).await?;

        if rows == 0 {
            std::fs::remove_dir_all(&staging).ok();
            self.invalidate_table_context(table);
            return Ok(CompactionStats {
                table: table.to_string(),
                files_before,
                rows: 0,
                appends: 0,
            });
        }

        // --- Re-insert the staged rows, then discard the staging area.
        staged.sort();
        let appends = self
            .reinsert_staged_chunks(table, &staged, &partition_columns)
            .await?;
        std::fs::remove_dir_all(&staging).ok();
        self.invalidate_table_context(table);
        Ok(CompactionStats {
            table: table.to_string(),
            files_before,
            rows,
            appends,
        })
    }

    /// Recover a table from staged chunks left behind by a
    /// [`IcebergStore::compact_table`] run whose re-insert half failed.
    ///
    /// The table must already exist (recreated empty by the interrupted
    /// compaction) and be empty; the staged chunks under `.compact_tmp_<table>`
    /// are re-inserted and then removed.
    pub async fn restore_from_staging(&self, table: &str) -> Result<CompactionStats> {
        let lock = self.table_write_lock(table).await;
        let _guard = lock.lock().await;

        let staging = self.staging_dir(table);
        if !staging.exists() {
            anyhow::bail!("no staging directory at {}", staging.display());
        }
        let mut staged: Vec<PathBuf> = std::fs::read_dir(&staging)
            .with_context(|| format!("failed to read {}", staging.display()))?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.extension()
                    .map(|ext| ext == "parquet")
                    .unwrap_or(false)
            })
            .collect();
        staged.sort();
        if staged.is_empty() {
            anyhow::bail!("no staged parquet chunks in {}", staging.display());
        }

        let catalog_table = self.catalog.load_table(&self.ident(table)).await?;
        let partition_columns: Vec<String> = catalog_table
            .metadata()
            .default_partition_spec()
            .fields()
            .iter()
            .map(|field| field.name.clone())
            .collect();
        drop(catalog_table);

        let appends = self
            .reinsert_staged_chunks(table, &staged, &partition_columns)
            .await?;
        std::fs::remove_dir_all(&staging).ok();
        self.invalidate_table_context(table);
        Ok(CompactionStats {
            table: table.to_string(),
            files_before: staged.len(),
            rows: 0,
            appends,
        })
    }

    fn staging_dir(&self, table: &str) -> PathBuf {
        match &self.backend {
            WarehouseBackend::Local { root } => root.join(format!(".compact_tmp_{table}")),
            WarehouseBackend::S3 { .. } => {
                std::env::temp_dir().join(format!("rlean_compact_tmp_{table}"))
            }
        }
    }

    /// Total number of live rows visible through the current snapshot of
    /// `table`. Used by maintenance tooling to verify a compaction preserved
    /// every row.
    pub async fn count_rows(&self, table: &str) -> Result<usize> {
        Ok(self.table_df(table).await?.count().await?)
    }

    /// Run an arbitrary SQL query against `table` (registered under its own
    /// name) and return the resulting batches. Intended for maintenance and
    /// debugging tooling only.
    pub async fn query_table(&self, table: &str, sql: &str) -> Result<Vec<RecordBatch>> {
        // Ensure the table is registered in its cached DataFusion context.
        let _ = self.table_df(table).await?;
        let ctx = {
            let contexts = self
                .table_contexts
                .lock()
                .expect("iceberg table context cache poisoned");
            contexts.get(table).map(|context| context.ctx.clone())
        }
        .ok_or_else(|| anyhow!("table context for {table} missing"))?;
        Ok(ctx.sql(sql).await?.collect().await?)
    }

    /// Read the (partition-clustered) staged chunks in order and re-insert them
    /// as append commits bounded by distinct partition count, row count and
    /// byte size. Returns the number of append commits written.
    async fn reinsert_staged_chunks(
        &self,
        table: &str,
        staged: &[PathBuf],
        partition_columns: &[String],
    ) -> Result<usize> {
        use arrow::row::{OwnedRow, RowConverter, SortField};
        use futures::StreamExt;

        let mut appends = 0usize;
        let mut group: Vec<RecordBatch> = Vec::new();
        let mut group_rows = 0usize;
        let mut group_bytes = 0usize;
        let mut group_partitions = 0usize;
        let mut last_key: Option<OwnedRow> = None;
        let mut converter: Option<RowConverter> = None;

        for path in staged {
            // Stream (not collect) each chunk so an oversized staged file cannot
            // pull gigabytes into memory at once.
            let ctx =
                SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
            self.register_s3_object_store(&ctx)?;
            let mut stream = ctx
                .read_parquet(
                    vec![path.to_string_lossy().to_string()],
                    ParquetReadOptions::default(),
                )
                .await?
                .execute_stream()
                .await?;
            while let Some(batch) = stream.next().await {
                let batch = batch?;
                if batch.num_rows() == 0 {
                    continue;
                }
                let partition_arrays = partition_columns
                    .iter()
                    .map(|name| {
                        let idx = batch
                            .schema()
                            .index_of(name)
                            .with_context(|| format!("partition column {name} missing"))?;
                        Ok(batch.column(idx).clone())
                    })
                    .collect::<Result<Vec<ArrayRef>>>()?;
                if converter.is_none() {
                    let fields = partition_arrays
                        .iter()
                        .map(|array| SortField::new(array.data_type().clone()))
                        .collect::<Vec<_>>();
                    converter = Some(RowConverter::new(fields)?);
                }
                let keys = converter
                    .as_ref()
                    .expect("row converter present")
                    .convert_columns(&partition_arrays)?;

                let rows_in_batch = batch.num_rows();
                let mut run_start = 0usize;
                for row in 1..=rows_in_batch {
                    let boundary = row == rows_in_batch || keys.row(row) != keys.row(row - 1);
                    if !boundary {
                        continue;
                    }
                    let slice = batch.slice(run_start, row - run_start);
                    let slice_rows = slice.num_rows();
                    let slice_bytes = record_batch_bytes(&slice);
                    let run_key = keys.row(run_start).owned();
                    let is_new_partition = last_key
                        .as_ref()
                        .is_none_or(|key| key.row() != run_key.row());
                    // Flush before the slice would overflow any budget. The byte
                    // budget also keeps the concatenated batch's string columns
                    // under Arrow's 2GB 32-bit offset limit.
                    let would_exceed = !group.is_empty()
                        && (group_bytes + slice_bytes > COMPACT_APPEND_BYTES
                            || group_rows + slice_rows > COMPACT_APPEND_ROWS
                            || (is_new_partition && group_partitions >= COMPACT_APPEND_PARTITIONS));
                    if would_exceed {
                        let schema = group[0].schema();
                        let combined = compute::concat_batches(&schema, &group)?;
                        self.insert_batch_locked(table, combined).await?;
                        appends += 1;
                        group.clear();
                        group_rows = 0;
                        group_bytes = 0;
                        group_partitions = 0;
                    }
                    if is_new_partition || group.is_empty() {
                        group_partitions += 1;
                    }
                    group_rows += slice_rows;
                    group_bytes += slice_bytes;
                    group.push(slice);
                    last_key = Some(run_key);
                    run_start = row;
                }
            }
        }
        if !group.is_empty() {
            let schema = group[0].schema();
            let combined = compute::concat_batches(&schema, &group)?;
            self.insert_batch_locked(table, combined).await?;
            appends += 1;
        }
        Ok(appends)
    }

    pub async fn reset_table(&self, name: &str) -> Result<()> {
        let ident = self.ident(name);
        if self.catalog.table_exists(&ident).await? {
            self.catalog
                .drop_table(&ident)
                .await
                .with_context(|| format!("failed to drop Iceberg table {NAMESPACE}.{name}"))?;
        }

        if let WarehouseBackend::Local { root } = &self.backend {
            let table_dir = root.join(NAMESPACE).join(name);
            match tokio::fs::remove_dir_all(&table_dir).await {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to remove {}", table_dir.display()));
                }
            }
        }

        self.invalidate_table_context(name);
        self.ensure_tables().await
    }

    pub async fn ensure_tables(&self) -> Result<()> {
        if !self.catalog.namespace_exists(&self.namespace).await? {
            self.catalog
                .create_namespace(&self.namespace, HashMap::new())
                .await
                .context("failed to create lean Iceberg namespace")?;
        }

        self.ensure_table(
            MARKET_TRADE_BARS,
            market_schema(schema::trade_bar_schema()),
            &["security_type", "market", "resolution", "symbol_sid", "day"],
        )
        .await?;
        self.ensure_table(
            MARKET_QUOTE_BARS,
            market_schema(schema::quote_bar_schema()),
            &["security_type", "market", "resolution", "symbol_sid", "day"],
        )
        .await?;
        self.ensure_table(
            MARKET_TICKS,
            market_schema(schema::tick_schema()),
            &["security_type", "market", "resolution", "symbol_sid", "day"],
        )
        .await?;
        self.ensure_table(
            OPTION_EOD_BARS,
            option_schema(schema::option_eod_bar_schema()),
            &["day"],
        )
        .await?;
        self.ensure_table(
            OPTION_UNIVERSE,
            option_schema(schema::option_universe_schema()),
            &["day"],
        )
        .await?;
        self.ensure_table(
            MARGIN_INTEREST,
            market_schema(schema::margin_interest_rate_schema()),
            &["security_type", "market", "day"],
        )
        .await?;
        self.ensure_table(
            PERPETUAL_CONTEXT,
            market_schema(schema::perpetual_context_schema()),
            &["security_type", "market", "day"],
        )
        .await?;
        self.ensure_table(CUSTOM_POINTS, custom_schema(), &["source_type", "ticker"])
            .await?;
        self.ensure_table(FACTOR_FILES, factor_schema(), &["market", "ticker"])
            .await?;
        self.ensure_table(MAP_FILES, map_schema(), &["market", "permtick"])
            .await?;
        Ok(())
    }

    async fn ensure_table(
        &self,
        name: &str,
        schema: Schema,
        partition_columns: &[&str],
    ) -> Result<()> {
        let ident = self.ident(name);
        if self.catalog.table_exists(&ident).await? {
            return Ok(());
        }
        let spec = partition_spec(schema.clone(), partition_columns)?;
        // Local mode (SQLite catalog) assigns the table location client-side.
        // S3 mode's REST catalog (Lakekeeper) owns the warehouse and assigns
        // each table's physical location itself, so no client location is set
        // (`None`).
        let mut creation = TableCreation::builder()
            .name(name.into())
            .schema(schema)
            .partition_spec(spec.into_unbound())
            .build();
        creation.location = self.table_location(name)?;
        if let Err(error) = self.catalog.create_table(&self.namespace, creation).await {
            // Concurrent connects race between the existence check and the
            // create (the catalog enforces uniqueness); losing that race is
            // success as long as the table now exists.
            if self.catalog.table_exists(&ident).await.unwrap_or(false) {
                return Ok(());
            }
            return Err(error)
                .with_context(|| format!("failed to create Iceberg table {NAMESPACE}.{name}"));
        }
        Ok(())
    }

    pub async fn scan_trade_bar_partitions_grouped(
        &self,
        symbols_by_sid: &HashMap<u64, Symbol>,
        resolution: Resolution,
        tick_type: TickType,
        params: &QueryParams,
    ) -> Result<HashMap<u64, Vec<TradeBar>>> {
        let batches = self
            .scan_market_batches(
                MARKET_TRADE_BARS,
                symbols_by_sid,
                resolution,
                tick_type,
                params,
            )
            .await?;
        let mut out: HashMap<u64, Vec<TradeBar>> = HashMap::new();
        for batch in &batches {
            append_trade_batch_grouped(batch, symbols_by_sid, &mut out)?;
        }
        sort_grouped_trade_bars(&mut out);
        Ok(out)
    }

    pub async fn scan_quote_bar_partitions_grouped(
        &self,
        symbols_by_sid: &HashMap<u64, Symbol>,
        resolution: Resolution,
        tick_type: TickType,
        params: &QueryParams,
    ) -> Result<HashMap<u64, Vec<QuoteBar>>> {
        let batches = self
            .scan_market_batches(
                MARKET_QUOTE_BARS,
                symbols_by_sid,
                resolution,
                tick_type,
                params,
            )
            .await?;
        let mut out: HashMap<u64, Vec<QuoteBar>> = HashMap::new();
        for batch in &batches {
            append_quote_batch_grouped(batch, symbols_by_sid, &mut out)?;
        }
        for bars in out.values_mut() {
            bars.sort_by_key(|bar| (bar.time.0, bar.symbol.id.sid));
        }
        Ok(out)
    }

    pub async fn scan_tick_partitions_grouped(
        &self,
        symbols_by_sid: &HashMap<u64, Symbol>,
        params: &QueryParams,
    ) -> Result<HashMap<u64, Vec<Tick>>> {
        let batches = self
            .scan_market_batches(
                MARKET_TICKS,
                symbols_by_sid,
                Resolution::Tick,
                TickType::Trade,
                params,
            )
            .await?;
        let mut out: HashMap<u64, Vec<Tick>> = HashMap::new();
        for batch in &batches {
            append_tick_batch_grouped(batch, symbols_by_sid, &mut out)?;
        }
        for ticks in out.values_mut() {
            ticks.sort_by_key(|tick| (tick.time.0, tick.symbol.id.sid, tick.tick_type as u8));
        }
        Ok(out)
    }

    pub async fn market_partition_days(
        &self,
        query: MarketPartitionDayQuery<'_>,
    ) -> Result<BTreeSet<i32>> {
        let cached_index = {
            let indexes = self
                .partition_indexes
                .lock()
                .expect("iceberg partition index cache poisoned");
            indexes.get(query.table).cloned()
        };
        if let Some(index) = cached_index {
            return Ok(index.days_for(
                query.security_type,
                query.market,
                query.resolution,
                query.symbol_sid,
                query.day_range.start,
                query.day_range.end,
            ));
        }

        let index = self.market_partition_index(query.table).await?;
        Ok(index.days_for(
            query.security_type,
            query.market,
            query.resolution,
            query.symbol_sid,
            query.day_range.start,
            query.day_range.end,
        ))
    }

    pub async fn warm_market_partition_index(&self, table: &str) -> Result<()> {
        self.market_partition_index(table).await?;
        Ok(())
    }

    pub async fn warm_market_partition_indexes<'a>(
        &self,
        tables: impl IntoIterator<Item = &'a str>,
    ) -> Result<()> {
        let mut seen = HashSet::new();
        for table in tables {
            if seen.insert(table.to_string()) {
                self.warm_market_partition_index(table).await?;
            }
        }
        Ok(())
    }

    pub async fn warm_custom_partition_index(&self) -> Result<()> {
        self.custom_partition_index().await?;
        Ok(())
    }

    async fn scan_market_batches(
        &self,
        table: &str,
        symbols_by_sid: &HashMap<u64, Symbol>,
        resolution: Resolution,
        _tick_type: TickType,
        params: &QueryParams,
    ) -> Result<Vec<RecordBatch>> {
        if symbols_by_sid.is_empty() {
            return Ok(Vec::new());
        }
        let first = symbols_by_sid
            .values()
            .next()
            .ok_or_else(|| anyhow!("market scan requires at least one symbol"))?;
        let day_start = params.predicate.start_day.or(params.predicate.start_time);
        let day_end = params.predicate.end_day.or(params.predicate.end_time);
        let index = self.market_partition_index(table).await?;
        let start_day = day_start.map(|start| days_since_epoch(start.0));
        let end_day = day_end.map(|end| days_since_epoch(end.0));
        let pruned_file_paths = symbols_by_sid
            .iter()
            .flat_map(|(sid, symbol)| {
                index.file_paths_for_range(
                    symbol.security_type(),
                    symbol.market().as_str(),
                    resolution,
                    *sid,
                    start_day,
                    end_day,
                )
            })
            .collect::<BTreeSet<_>>();
        if pruned_file_paths.is_empty() {
            return Ok(Vec::new());
        }
        const MAX_ATTEMPTS: usize = 5;
        for attempt in 0..MAX_ATTEMPTS {
            let mut df = self.market_files_df(pruned_file_paths.iter()).await?;
            df = df
                .filter(
                    col("security_type").eq(lit(first.security_type().to_string().to_lowercase())),
                )?
                .filter(col("market").eq(lit(first.market().as_str().to_lowercase())))?
                .filter(col("resolution").eq(lit(resolution.folder_name())))?;
            if let Some(start) = day_start {
                df = df.filter(col("day").gt_eq(lit(days_since_epoch(start.0))))?;
            }
            if let Some(end) = day_end {
                df = df.filter(col("day").lt_eq(lit(days_since_epoch(end.0))))?;
            }
            if let Some(filter) = params.predicate.to_datafusion_expr() {
                df = df.filter(filter)?;
            }
            match df.collect().await {
                Ok(batches) => return Ok(batches),
                Err(error) if attempt + 1 < MAX_ATTEMPTS => {
                    let message = error.to_string();
                    if message.contains("CatalogCommitConflicts")
                        || message.contains("snapshot has changed")
                    {
                        self.invalidate_table_context(table);
                        tokio::time::sleep(std::time::Duration::from_millis(
                            10 * (attempt as u64 + 1),
                        ))
                        .await;
                        continue;
                    }
                    return Err(error.into());
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Vec::new())
    }

    pub async fn append_trade_bars(
        &self,
        bars: &[TradeBar],
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<()> {
        if bars.is_empty() {
            return Ok(());
        }
        let lock = self.table_write_lock(MARKET_TRADE_BARS).await;
        let _guard = lock.lock().await;
        let bars = self
            .dedupe_trade_bars_for_append(bars, security_type, market, resolution, tick_type)
            .await?;
        if bars.is_empty() {
            return Ok(());
        }
        let batch = with_market_partitions(
            convert::trade_bars_to_record_batch(&bars),
            security_type,
            market,
            resolution,
            tick_type,
        )?;
        self.insert_batch_locked(MARKET_TRADE_BARS, batch).await
    }

    pub async fn append_trade_bars_unchecked(
        &self,
        bars: &[TradeBar],
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<()> {
        if bars.is_empty() {
            return Ok(());
        }
        let batch = with_market_partitions(
            convert::trade_bars_to_record_batch(bars),
            security_type,
            market,
            resolution,
            tick_type,
        )?;
        self.insert_batch(MARKET_TRADE_BARS, batch).await
    }

    pub async fn append_quote_bars(
        &self,
        bars: &[QuoteBar],
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<()> {
        if bars.is_empty() {
            return Ok(());
        }
        let lock = self.table_write_lock(MARKET_QUOTE_BARS).await;
        let _guard = lock.lock().await;
        let bars = self
            .dedupe_quote_bars_for_append(bars, security_type, market, resolution, tick_type)
            .await?;
        if bars.is_empty() {
            return Ok(());
        }
        let batch = with_market_partitions(
            convert::quote_bars_to_record_batch(&bars),
            security_type,
            market,
            resolution,
            tick_type,
        )?;
        self.insert_batch_locked(MARKET_QUOTE_BARS, batch).await
    }

    pub async fn append_quote_bars_unchecked(
        &self,
        bars: &[QuoteBar],
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<()> {
        if bars.is_empty() {
            return Ok(());
        }
        let batch = with_market_partitions(
            convert::quote_bars_to_record_batch(bars),
            security_type,
            market,
            resolution,
            tick_type,
        )?;
        self.insert_batch(MARKET_QUOTE_BARS, batch).await
    }

    pub async fn append_ticks(
        &self,
        ticks: &[Tick],
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<()> {
        if ticks.is_empty() {
            return Ok(());
        }
        let lock = self.table_write_lock(MARKET_TICKS).await;
        let _guard = lock.lock().await;
        let ticks = self
            .dedupe_ticks_for_append(ticks, security_type, market, resolution, tick_type)
            .await?;
        if ticks.is_empty() {
            return Ok(());
        }
        let batch = with_market_partitions(
            convert::ticks_to_record_batch(&ticks),
            security_type,
            market,
            resolution,
            tick_type,
        )?;
        self.insert_batch_locked(MARKET_TICKS, batch).await
    }

    pub async fn append_margin_interest_rates_unchecked(
        &self,
        rates: &[MarginInterestRate],
        security_type: SecurityType,
        market: &str,
    ) -> Result<()> {
        if rates.is_empty() {
            return Ok(());
        }
        let batch = with_market_partitions(
            convert::margin_interest_rates_to_record_batch(rates),
            security_type,
            market,
            Resolution::Hour,
            TickType::Trade,
        )?;
        self.insert_batch(MARGIN_INTEREST, batch).await
    }

    pub async fn append_perpetual_contexts_unchecked(
        &self,
        contexts: &[PerpetualContext],
        security_type: SecurityType,
        market: &str,
    ) -> Result<()> {
        if contexts.is_empty() {
            return Ok(());
        }
        let batch = with_market_partitions(
            convert::perpetual_contexts_to_record_batch(contexts),
            security_type,
            market,
            Resolution::Minute,
            TickType::Trade,
        )?;
        self.insert_batch(PERPETUAL_CONTEXT, batch).await
    }

    async fn dedupe_trade_bars_for_append(
        &self,
        bars: &[TradeBar],
        _security_type: SecurityType,
        _market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<Vec<TradeBar>> {
        let Some((start, end, symbols_by_sid)) = trade_bar_append_window(bars) else {
            return Ok(Vec::new());
        };
        let params = QueryParams::new()
            .with_day_range(start, end)
            .with_bar_range(start, end)
            .with_symbols(symbols_by_sid.keys().copied().collect());
        let existing = self
            .scan_trade_bar_partitions_grouped(&symbols_by_sid, resolution, tick_type, &params)
            .await?;
        let mut keys: HashSet<(u64, i64)> = existing
            .values()
            .flat_map(|rows| rows.iter().map(|row| (row.symbol.id.sid, row.end_time.0)))
            .collect();
        let mut out = Vec::new();
        for bar in bars {
            let key = (bar.symbol.id.sid, bar.end_time.0);
            if keys.insert(key) {
                out.push(bar.clone());
            }
        }
        Ok(out)
    }

    async fn dedupe_quote_bars_for_append(
        &self,
        bars: &[QuoteBar],
        _security_type: SecurityType,
        _market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<Vec<QuoteBar>> {
        let Some((start, end, symbols_by_sid)) = quote_bar_append_window(bars) else {
            return Ok(Vec::new());
        };
        let params = QueryParams::new()
            .with_day_range(start, end)
            .with_bar_range(start, end)
            .with_symbols(symbols_by_sid.keys().copied().collect());
        let existing = self
            .scan_quote_bar_partitions_grouped(&symbols_by_sid, resolution, tick_type, &params)
            .await?;
        let mut keys: HashSet<(u64, i64)> = existing
            .values()
            .flat_map(|rows| rows.iter().map(|row| (row.symbol.id.sid, row.end_time.0)))
            .collect();
        let mut out = Vec::new();
        for bar in bars {
            let key = (bar.symbol.id.sid, bar.end_time.0);
            if keys.insert(key) {
                out.push(bar.clone());
            }
        }
        Ok(out)
    }

    async fn dedupe_ticks_for_append(
        &self,
        ticks: &[Tick],
        _security_type: SecurityType,
        _market: &str,
        _resolution: Resolution,
        _tick_type: TickType,
    ) -> Result<Vec<Tick>> {
        let Some((start, end, symbols_by_sid)) = tick_append_window(ticks) else {
            return Ok(Vec::new());
        };
        let params = QueryParams::new()
            .with_time_range(start, end)
            .with_symbols(symbols_by_sid.keys().copied().collect());
        let existing = self
            .scan_tick_partitions_grouped(&symbols_by_sid, &params)
            .await?;
        let mut keys: HashSet<(u64, i64, TickType)> = existing
            .values()
            .flat_map(|rows| {
                rows.iter()
                    .map(|row| (row.symbol.id.sid, row.time.0, row.tick_type))
            })
            .collect();
        let mut out = Vec::new();
        for tick in ticks {
            let key = (tick.symbol.id.sid, tick.time.0, tick.tick_type);
            if keys.insert(key) {
                out.push(tick.clone());
            }
        }
        Ok(out)
    }

    pub async fn scan_margin_interest_rates(
        &self,
        symbol: &Symbol,
        params: &QueryParams,
    ) -> Result<Vec<MarginInterestRate>> {
        let mut df = self
            .table_df(MARGIN_INTEREST)
            .await?
            .filter(
                col("security_type").eq(lit(symbol.security_type().to_string().to_lowercase())),
            )?
            .filter(col("market").eq(lit(symbol.market().as_str().to_lowercase())))?
            .filter(col("symbol_sid").eq(lit(symbol.id.sid as i64)))?;
        if let Some(filter) = params.predicate.to_datafusion_expr() {
            df = df.filter(filter)?;
        }
        let batches = df.collect().await?;
        let mut out = Vec::new();
        for batch in &batches {
            out.extend(convert::record_batch_to_margin_interest_rates(
                batch,
                symbol.clone(),
            ));
        }
        out.sort_by_key(|rate| rate.time.0);
        Ok(out)
    }

    pub async fn scan_perpetual_contexts(
        &self,
        symbol: &Symbol,
        params: &QueryParams,
    ) -> Result<Vec<PerpetualContext>> {
        let mut df = self
            .table_df(PERPETUAL_CONTEXT)
            .await?
            .filter(
                col("security_type").eq(lit(symbol.security_type().to_string().to_lowercase())),
            )?
            .filter(col("market").eq(lit(symbol.market().as_str().to_lowercase())))?
            .filter(col("symbol_sid").eq(lit(symbol.id.sid as i64)))?;
        if let Some(filter) = params.predicate.to_datafusion_expr() {
            df = df.filter(filter)?;
        }
        let batches = df.collect().await?;
        let mut out = Vec::new();
        for batch in &batches {
            out.extend(convert::record_batch_to_perpetual_contexts(
                batch,
                symbol.clone(),
            ));
        }
        out.sort_by_key(|context| context.time.0);
        Ok(out)
    }

    pub async fn scan_custom_points(
        &self,
        source_type: &str,
        ticker: &str,
        date: chrono::NaiveDate,
    ) -> Result<Vec<CustomDataPoint>> {
        self.scan_custom_points_range(source_type, ticker, date, date)
            .await
    }

    pub async fn scan_custom_points_range(
        &self,
        source_type: &str,
        ticker: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
    ) -> Result<Vec<CustomDataPoint>> {
        self.scan_custom_points_range_with_query(source_type, ticker, start, end, None)
            .await
    }

    pub async fn scan_custom_points_range_with_query(
        &self,
        source_type: &str,
        ticker: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
        query: Option<&CustomDataQuery>,
    ) -> Result<Vec<CustomDataPoint>> {
        let start_day = days_since_epoch(schema::date_to_ns(start));
        let end_day = days_since_epoch(schema::date_to_ns(end));
        let batches = self
            .scan_custom_points_batches(source_type, ticker, start_day, end_day, query)
            .await?;
        let mut out = Vec::new();
        let projected_fields = custom_projected_fields(query);
        for batch in &batches {
            append_custom_batch_points(batch, &mut out, projected_fields.as_ref())?;
        }
        out.sort_by_key(|point| {
            point
                .end_time
                .map(|dt| dt.0)
                .unwrap_or_else(|| schema::date_to_ns(point.time))
        });
        Ok(out)
    }

    pub async fn scan_custom_points_raw_batches(
        &self,
        source_type: &str,
        ticker: &str,
        start: chrono::NaiveDate,
        end: chrono::NaiveDate,
        query: Option<&CustomDataQuery>,
    ) -> Result<Vec<RecordBatch>> {
        let start_day = days_since_epoch(schema::date_to_ns(start));
        let end_day = days_since_epoch(schema::date_to_ns(end));
        self.scan_custom_points_batches(source_type, ticker, start_day, end_day, query)
            .await
    }

    pub async fn has_custom_points_dataset(&self, source_type: &str, ticker: &str) -> Result<bool> {
        let index = self.custom_partition_index().await?;
        Ok(index.has_dataset(source_type, ticker))
    }

    pub async fn append_custom_points(
        &self,
        source_type: &str,
        ticker: &str,
        points: &[CustomDataPoint],
    ) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }
        let incoming = points.len();
        let points = self
            .dedupe_custom_points_for_append(source_type, ticker, points)
            .await?;
        if incoming >= 10_000 || points.len() < incoming {
            tracing::info!(
                "append_custom_points {}/{}: {} incoming, {} after dedupe",
                source_type,
                ticker,
                incoming,
                points.len(),
            );
        }
        if points.is_empty() {
            return Ok(());
        }
        // A full-history fetch can hand us millions of points at once (e.g. a
        // 7-year TradeAlert snapshot backfill). Arrow Utf8 arrays use i32
        // offsets, so a single batch whose serialized `fields_json` exceeds
        // 2 GiB panics on offset overflow — split the append into batches
        // bounded by rows and by estimated fields_json bytes.
        const MAX_ROWS_PER_BATCH: usize = 250_000;
        const MAX_FIELDS_BYTES_PER_BATCH: usize = 1 << 30; // 1 GiB
        let mut start = 0usize;
        while start < points.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < points.len() && end - start < MAX_ROWS_PER_BATCH {
                bytes += estimated_fields_json_len(&points[end]);
                end += 1;
                if bytes >= MAX_FIELDS_BYTES_PER_BATCH {
                    break;
                }
            }
            let batch = custom_points_to_record_batch(source_type, ticker, &points[start..end])?;
            self.insert_batch(CUSTOM_POINTS, batch).await?;
            start = end;
        }
        Ok(())
    }

    pub async fn scan_option_universe(
        &self,
        underlyings: &[String],
        date: chrono::NaiveDate,
    ) -> Result<Vec<OptionUniverseRow>> {
        if underlyings.is_empty() {
            return Ok(Vec::new());
        }
        let day = days_since_epoch(schema::date_to_ns(date));
        let underlying_exprs = underlyings
            .iter()
            .map(|ticker| lit(ticker.to_ascii_uppercase()))
            .collect::<Vec<_>>();
        let batches = self
            .table_df(OPTION_UNIVERSE)
            .await?
            .filter(col("day").eq(lit(day)))?
            .filter(col("underlying").in_list(underlying_exprs, false))?
            .collect()
            .await?;
        let mut out = Vec::new();
        for batch in &batches {
            out.extend(convert::record_batch_to_option_universe_rows(batch));
        }
        out.sort_by_key(|row| {
            (
                row.underlying.clone(),
                row.expiration,
                row.symbol_value.clone(),
            )
        });
        Ok(out)
    }

    pub async fn append_option_universe(&self, rows: &[OptionUniverseRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let rows = self.dedupe_option_universe_for_append(rows).await?;
        if rows.is_empty() {
            return Ok(());
        }
        let batch = with_option_partitions(convert::option_universe_rows_to_record_batch(&rows))?;
        self.insert_batch(OPTION_UNIVERSE, batch).await
    }

    pub async fn append_option_eod_bars(&self, bars: &[schema::OptionEodBar]) -> Result<()> {
        if bars.is_empty() {
            return Ok(());
        }
        let bars = self.dedupe_option_eod_bars_for_append(bars).await?;
        if bars.is_empty() {
            return Ok(());
        }
        let batch = with_option_partitions(convert::option_eod_bars_to_record_batch(&bars))?;
        self.insert_batch(OPTION_EOD_BARS, batch).await
    }

    async fn dedupe_custom_points_for_append(
        &self,
        source_type: &str,
        ticker: &str,
        points: &[CustomDataPoint],
    ) -> Result<Vec<CustomDataPoint>> {
        let mut existing = HashSet::new();
        if let (Some(start), Some(end)) = (
            points.iter().map(|point| point.time).min(),
            points.iter().map(|point| point.time).max(),
        ) {
            for point in self
                .scan_custom_points_range(source_type, ticker, start, end)
                .await?
            {
                existing.insert(custom_point_key(&point));
            }
        }
        Ok(points
            .iter()
            .filter(|point| {
                let key = custom_point_key(point);
                if key.1.is_empty() {
                    // Without a provider row id the key cannot distinguish
                    // same-timestamp same-symbol rows, so only dedupe against
                    // already-persisted rows; dropping within-batch "twins"
                    // here would silently discard real data.
                    !existing.contains(&key)
                } else {
                    existing.insert(key)
                }
            })
            .cloned()
            .collect())
    }

    async fn dedupe_option_universe_for_append(
        &self,
        rows: &[OptionUniverseRow],
    ) -> Result<Vec<OptionUniverseRow>> {
        let dates = rows.iter().map(|row| row.date).collect::<HashSet<_>>();
        let underlyings = rows
            .iter()
            .map(|row| row.underlying.to_ascii_uppercase())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut existing = HashSet::new();
        for date in dates {
            for row in self.scan_option_universe(&underlyings, date).await? {
                existing.insert(option_universe_key(&row));
            }
        }
        Ok(rows
            .iter()
            .filter(|row| !existing.contains(&option_universe_key(row)))
            .cloned()
            .collect())
    }

    async fn dedupe_option_eod_bars_for_append(
        &self,
        bars: &[schema::OptionEodBar],
    ) -> Result<Vec<schema::OptionEodBar>> {
        let dates = bars.iter().map(|bar| bar.date).collect::<HashSet<_>>();
        let underlyings = bars
            .iter()
            .map(|bar| bar.underlying.to_ascii_uppercase())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mut existing = HashSet::new();
        for date in dates {
            for bar in self.scan_option_eod_bars(&underlyings, date).await? {
                existing.insert(option_eod_key(&bar));
            }
        }
        Ok(bars
            .iter()
            .filter(|bar| !existing.contains(&option_eod_key(bar)))
            .cloned()
            .collect())
    }

    pub async fn scan_option_eod_bars(
        &self,
        underlyings: &[String],
        date: chrono::NaiveDate,
    ) -> Result<Vec<schema::OptionEodBar>> {
        if underlyings.is_empty() {
            return Ok(Vec::new());
        }
        let day = days_since_epoch(schema::date_to_ns(date));
        let underlying_exprs = underlyings
            .iter()
            .map(|ticker| lit(ticker.to_ascii_uppercase()))
            .collect::<Vec<_>>();
        let batches = self
            .table_df(OPTION_EOD_BARS)
            .await?
            .filter(col("day").eq(lit(day)))?
            .filter(col("underlying").in_list(underlying_exprs, false))?
            .collect()
            .await?;
        let mut out = Vec::new();
        for batch in &batches {
            append_option_eod_batch(batch, &mut out)?;
        }
        out.sort_by_key(|row| {
            (
                row.underlying.clone(),
                row.expiration,
                row.symbol_value.clone(),
            )
        });
        Ok(out)
    }

    pub async fn scan_factor_file(
        &self,
        market: &str,
        ticker: &str,
    ) -> Result<Vec<FactorFileEntry>> {
        let batches = self
            .table_df(FACTOR_FILES)
            .await?
            .filter(col("market").eq(lit(market.to_lowercase())))?
            .filter(col("ticker").eq(lit(ticker.to_lowercase())))?
            .collect()
            .await?;
        let mut out = Vec::new();
        for batch in &batches {
            append_factor_batch(batch, &mut out)?;
        }
        out.sort_by_key(|row| row.date);
        Ok(out)
    }

    pub async fn append_factor_file(
        &self,
        market: &str,
        ticker: &str,
        entries: &[FactorFileEntry],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let batch = factor_entries_to_record_batch(market, ticker, entries)?;
        self.insert_batch(FACTOR_FILES, batch).await
    }

    pub async fn append_factor_files(
        &self,
        rows: &[(String, String, Vec<FactorFileEntry>)],
    ) -> Result<()> {
        let batches = rows
            .iter()
            .filter(|(_, _, entries)| !entries.is_empty())
            .map(|(market, ticker, entries)| {
                factor_entries_to_record_batch(market, ticker, entries)
            })
            .collect::<Result<Vec<_>>>()?;
        let batch = concat_record_batches(&batches)?;
        if batch.num_rows() == 0 {
            return Ok(());
        }
        self.insert_batch(FACTOR_FILES, batch).await
    }

    pub async fn scan_map_file(&self, market: &str, ticker: &str) -> Result<Vec<MapFileEntry>> {
        let batches = self
            .table_df(MAP_FILES)
            .await?
            .filter(col("market").eq(lit(market.to_lowercase())))?
            .filter(col("permtick").eq(lit(ticker.to_lowercase())))?
            .collect()
            .await?;
        let mut out = Vec::new();
        for batch in &batches {
            append_map_batch(batch, &mut out)?;
        }
        out.sort_by_key(|row| row.date);
        Ok(out)
    }

    pub async fn append_map_file(
        &self,
        market: &str,
        ticker: &str,
        entries: &[MapFileEntry],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let batch = map_entries_to_record_batch(market, ticker, entries)?;
        self.insert_batch(MAP_FILES, batch).await
    }

    pub async fn append_map_files(
        &self,
        rows: &[(String, String, Vec<MapFileEntry>)],
    ) -> Result<()> {
        let batches = rows
            .iter()
            .filter(|(_, _, entries)| !entries.is_empty())
            .map(|(market, ticker, entries)| map_entries_to_record_batch(market, ticker, entries))
            .collect::<Result<Vec<_>>>()?;
        let batch = concat_record_batches(&batches)?;
        if batch.num_rows() == 0 {
            return Ok(());
        }
        self.insert_batch(MAP_FILES, batch).await
    }

    pub async fn append_market_record_batch(
        &self,
        batch: RecordBatch,
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let batch = with_market_partitions(batch, security_type, market, resolution, tick_type)?;
        self.insert_batch(MARKET_TRADE_BARS, batch).await
    }

    pub async fn append_trade_record_batch(
        &self,
        batch: RecordBatch,
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<()> {
        self.append_market_record_batch(batch, security_type, market, resolution, tick_type)
            .await
    }

    pub async fn append_quote_record_batch(
        &self,
        batch: RecordBatch,
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let batch = with_market_partitions(batch, security_type, market, resolution, tick_type)?;
        self.insert_batch(MARKET_QUOTE_BARS, batch).await
    }

    pub async fn append_tick_record_batch(
        &self,
        batch: RecordBatch,
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        tick_type: TickType,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let batch = with_market_partitions(batch, security_type, market, resolution, tick_type)?;
        self.insert_batch(MARKET_TICKS, batch).await
    }

    pub async fn append_option_universe_record_batch(&self, batch: RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let rows = convert::record_batch_to_option_universe_rows(&batch);
        self.append_option_universe(&rows).await
    }

    pub async fn append_option_universe_record_batch_unchecked(
        &self,
        batch: RecordBatch,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let batch = with_option_partitions(batch)?;
        self.insert_batch(OPTION_UNIVERSE, batch).await
    }

    pub async fn append_option_eod_record_batch(&self, batch: RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let bars = convert::record_batch_to_option_eod_bars(&batch);
        self.append_option_eod_bars(&bars).await
    }

    pub async fn append_option_eod_record_batch_unchecked(&self, batch: RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let batch = with_option_partitions(batch)?;
        self.insert_batch(OPTION_EOD_BARS, batch).await
    }

    pub async fn append_custom_record_batch(
        &self,
        source_type: &str,
        ticker: &str,
        batch: RecordBatch,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let mut points = Vec::new();
        append_custom_batch_points(&batch, &mut points, None)?;
        self.append_custom_points(source_type, ticker, &points)
            .await
    }

    pub async fn append_custom_record_batch_unchecked(
        &self,
        source_type: &str,
        ticker: &str,
        batch: RecordBatch,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let batch = with_custom_partitions(batch, source_type, ticker)?;
        self.insert_batch(CUSTOM_POINTS, batch).await
    }

    pub async fn append_factor_record_batch(
        &self,
        market: &str,
        ticker: &str,
        batch: RecordBatch,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let rows = batch.num_rows();
        let batch = append_columns(
            batch,
            &[
                Field::new("market", DataType::Utf8, false),
                Field::new("ticker", DataType::Utf8, false),
            ],
            vec![
                Arc::new(StringArray::from(vec![market.to_lowercase(); rows])),
                Arc::new(StringArray::from(vec![ticker.to_lowercase(); rows])),
            ],
        )?;
        self.insert_batch(FACTOR_FILES, batch).await
    }

    pub async fn append_factor_record_batch_with_partitions(
        &self,
        batch: RecordBatch,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        self.insert_batch(FACTOR_FILES, batch).await
    }

    pub async fn append_map_record_batch(
        &self,
        market: &str,
        ticker: &str,
        batch: RecordBatch,
    ) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let rows = batch.num_rows();
        let batch = append_columns(
            batch,
            &[
                Field::new("market", DataType::Utf8, false),
                Field::new("permtick", DataType::Utf8, false),
            ],
            vec![
                Arc::new(StringArray::from(vec![market.to_lowercase(); rows])),
                Arc::new(StringArray::from(vec![ticker.to_lowercase(); rows])),
            ],
        )?;
        self.insert_batch(MAP_FILES, batch).await
    }

    pub async fn scan_option_trade_bars(
        &self,
        symbols_by_value: &HashMap<String, Symbol>,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> Result<Vec<TradeBar>> {
        let batches = self
            .scan_option_market_batches(MARKET_TRADE_BARS, symbols_by_value, resolution, date)
            .await?;
        let mut out = Vec::new();
        for batch in &batches {
            append_trade_batch_by_symbol_value(batch, symbols_by_value, &mut out)?;
        }
        out.sort_by_key(|bar| (bar.time.0, bar.symbol.id.sid));
        Ok(out)
    }

    pub async fn scan_option_quote_bars(
        &self,
        symbols_by_value: &HashMap<String, Symbol>,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> Result<Vec<QuoteBar>> {
        let batches = self
            .scan_option_market_batches(MARKET_QUOTE_BARS, symbols_by_value, resolution, date)
            .await?;
        let mut out = Vec::new();
        for batch in &batches {
            append_quote_batch_by_symbol_value(batch, symbols_by_value, &mut out)?;
        }
        out.sort_by_key(|bar| (bar.time.0, bar.symbol.id.sid));
        Ok(out)
    }

    pub async fn scan_option_ticks(
        &self,
        symbols_by_value: &HashMap<String, Symbol>,
        date: chrono::NaiveDate,
    ) -> Result<Vec<Tick>> {
        let batches = self
            .scan_option_market_batches(MARKET_TICKS, symbols_by_value, Resolution::Tick, date)
            .await?;
        let mut out = Vec::new();
        for batch in &batches {
            append_tick_batch_by_symbol_value(batch, symbols_by_value, &mut out)?;
        }
        out.sort_by_key(|tick| (tick.time.0, tick.symbol.id.sid, tick.tick_type as u8));
        Ok(out)
    }

    async fn scan_option_market_batches(
        &self,
        table: &str,
        symbols_by_value: &HashMap<String, Symbol>,
        resolution: Resolution,
        date: chrono::NaiveDate,
    ) -> Result<Vec<RecordBatch>> {
        if symbols_by_value.is_empty() {
            return Ok(Vec::new());
        }
        let day = days_since_epoch(schema::date_to_ns(date));
        let values = symbols_by_value
            .keys()
            .map(|value| lit(value.clone()))
            .collect::<Vec<_>>();
        Ok(self
            .table_df(table)
            .await?
            .filter(col("security_type").eq(lit("option")))?
            .filter(col("market").eq(lit("usa")))?
            .filter(col("resolution").eq(lit(resolution.folder_name())))?
            .filter(col("day").eq(lit(day)))?
            .filter(col("symbol_value").in_list(values, false))?
            .collect()
            .await?)
    }

    async fn insert_batch(&self, table: &str, batch: RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let lock = self.table_write_lock(table).await;
        let _guard = lock.lock().await;
        self.insert_batch_locked(table, batch).await
    }

    async fn insert_batch_locked(&self, table: &str, batch: RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        const MAX_ATTEMPTS: usize = 8;
        for attempt in 0..MAX_ATTEMPTS {
            match self.insert_batch_once(table, &batch).await {
                Ok(()) => return Ok(()),
                Err(error) if is_catalog_commit_conflict(&error) && attempt + 1 < MAX_ATTEMPTS => {
                    self.invalidate_table_context(table);
                    tokio::time::sleep(std::time::Duration::from_millis(10 * (attempt as u64 + 1)))
                        .await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(anyhow!(
            "Iceberg commit for table {table} failed after {MAX_ATTEMPTS} catalog conflict retries"
        ))
    }

    async fn insert_batch_once(&self, table: &str, batch: &RecordBatch) -> Result<()> {
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let table_ref = self.catalog.load_table(&self.ident(table)).await?;
        let iceberg_schema = table_ref.metadata().current_schema().clone();
        let spec = table_ref
            .metadata()
            .default_partition_spec()
            .as_ref()
            .clone();
        let batch = drop_null_required_rows(batch.clone())?;
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let batch = with_iceberg_field_ids(batch, iceberg_schema.as_ref())?;
        if batch.num_rows() == 0 {
            return Ok(());
        }
        let location_generator = DefaultLocationGenerator::new(table_ref.metadata().clone())
            .map_err(|err| anyhow!(err))?;
        let append_id = APPEND_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let file_name_generator = DefaultFileNameGenerator::new(
            format!(
                "{}-append-{}-{}",
                table,
                chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                append_id
            ),
            None,
            DataFileFormat::Parquet,
        );
        let parquet_writer_builder = ParquetWriterBuilder::new(
            parquet::file::properties::WriterProperties::builder().build(),
            iceberg_schema.clone(),
        );
        let rolling_writer_builder = RollingFileWriterBuilder::new_with_default_file_size(
            parquet_writer_builder,
            table_ref.file_io().clone(),
            location_generator,
            file_name_generator,
        );
        let data_file_writer_builder = DataFileWriterBuilder::new(rolling_writer_builder);
        let mut writer = FanoutWriter::new(data_file_writer_builder);
        let mut start = 0usize;
        let mut current_key = partition_fingerprint(&spec, &batch, 0)?;
        for row in 1..batch.num_rows() {
            let row_key = partition_fingerprint(&spec, &batch, row)?;
            if row_key != current_key {
                let chunk = batch.slice(start, row - start);
                let partition_key =
                    partition_key_from_batch(&spec, iceberg_schema.clone(), &chunk)?;
                writer.write(partition_key, chunk).await?;
                start = row;
                current_key = row_key;
            }
        }
        let chunk = batch.slice(start, batch.num_rows() - start);
        let partition_key = partition_key_from_batch(&spec, iceberg_schema.clone(), &chunk)?;
        writer.write(partition_key, chunk).await?;
        let data_files = writer.close().await?;
        if data_files.is_empty() {
            return Ok(());
        }
        let data_files_for_index = data_files.clone();
        let tx = Transaction::new(&table_ref);
        let tx = tx.fast_append().add_data_files(data_files).apply(tx)?;
        tx.commit(self.catalog.as_ref()).await?;
        self.merge_data_files_into_partition_index(table, &data_files_for_index)
            .await?;
        self.merge_data_files_into_custom_partition_index(table, &data_files_for_index)
            .await?;
        self.table_contexts
            .lock()
            .expect("iceberg table context cache poisoned")
            .remove(table);
        Ok(())
    }

    async fn table_write_lock(&self, table: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .table_write_locks
            .lock()
            .expect("iceberg table write lock cache poisoned");
        locks
            .entry(table.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    async fn table_df(&self, table: &str) -> Result<DataFrame> {
        let cached_context = {
            let contexts = self
                .table_contexts
                .lock()
                .expect("iceberg table context cache poisoned");
            contexts.get(table).cloned()
        };
        if let Some(cached) = cached_context {
            return Ok(cached.ctx.table(table).await?);
        }

        let catalog_table = self.catalog.load_table(&self.ident(table)).await?;
        let metadata_location = catalog_table
            .metadata_location_result()
            .map_err(|err| anyhow!(err))?
            .to_string();
        let mut state = SessionStateBuilder::new().with_default_features().build();
        state.table_factories_mut().insert(
            "ICEBERG".to_string(),
            Arc::new(IcebergTableProviderFactory::new_with_storage_factory(
                self.iceberg_storage_factory(),
            )),
        );
        let ctx = SessionContext::new_with_state(state);
        self.register_s3_object_store(&ctx)?;
        let sql = format!(
            "CREATE EXTERNAL TABLE {table} STORED AS ICEBERG LOCATION '{}'",
            metadata_location.replace('\'', "''")
        );
        ctx.sql(&sql).await?.collect().await?;
        let cached = Arc::new(IcebergTableContext { ctx });
        self.table_contexts
            .lock()
            .expect("iceberg table context cache poisoned")
            .insert(table.to_string(), cached.clone());
        Ok(cached.ctx.table(table).await?)
    }

    async fn market_files_df<'a>(
        &self,
        file_paths: impl IntoIterator<Item = &'a String>,
    ) -> Result<DataFrame> {
        let paths = file_paths
            .into_iter()
            .map(|path| self.warehouse_file_path(path))
            .collect::<Result<Vec<_>>>()?;
        let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
        self.register_s3_object_store(&ctx)?;
        Ok(ctx
            .read_parquet(paths, ParquetReadOptions::default())
            .await?)
    }

    async fn market_partition_index(&self, table: &str) -> Result<Arc<MarketPartitionIndex>> {
        let cached_index = {
            let indexes = self
                .partition_indexes
                .lock()
                .expect("iceberg partition index cache poisoned");
            indexes.get(table).cloned()
        };
        if let Some(cached) = cached_index {
            return Ok(cached);
        }

        let catalog_table = self.catalog.load_table(&self.ident(table)).await?;
        let snapshot_id = catalog_table.metadata().current_snapshot_id();
        let mut index = MarketPartitionIndex::new(snapshot_id);
        let default_spec = catalog_table
            .metadata()
            .default_partition_spec()
            .as_ref()
            .clone();
        let default_fields = MarketPartitionFields::from_spec(&default_spec)?;
        let mut tasks = catalog_table.scan().build()?.plan_files().await?;
        while let Some(task) = futures::TryStreamExt::try_next(&mut tasks).await? {
            index.insert_file_scan_task_with_fields(&task, &default_fields)?;
        }
        let index = Arc::new(index);
        self.partition_indexes
            .lock()
            .expect("iceberg partition index cache poisoned")
            .insert(table.to_string(), index.clone());
        Ok(index)
    }

    async fn custom_partition_index(&self) -> Result<Arc<CustomPartitionIndex>> {
        let cached_index = {
            let indexes = self
                .custom_partition_indexes
                .lock()
                .expect("iceberg custom partition index cache poisoned");
            indexes.get(CUSTOM_POINTS).cloned()
        };
        if let Some(cached) = cached_index {
            return Ok(cached);
        }

        let catalog_table = self.catalog.load_table(&self.ident(CUSTOM_POINTS)).await?;
        let snapshot_id = catalog_table.metadata().current_snapshot_id();
        let mut index = CustomPartitionIndex::new(snapshot_id);
        let default_spec = catalog_table
            .metadata()
            .default_partition_spec()
            .as_ref()
            .clone();
        let default_fields = CustomPartitionFields::from_spec(&default_spec)?;
        let mut tasks = catalog_table.scan().build()?.plan_files().await?;
        while let Some(task) = futures::TryStreamExt::try_next(&mut tasks).await? {
            index.insert_file_scan_task_with_fields(&task, &default_fields)?;
        }
        let index = Arc::new(index);
        self.custom_partition_indexes
            .lock()
            .expect("iceberg custom partition index cache poisoned")
            .insert(CUSTOM_POINTS.to_string(), index.clone());
        Ok(index)
    }

    async fn scan_custom_points_batches(
        &self,
        source_type: &str,
        ticker: &str,
        start_day: i32,
        end_day: i32,
        query: Option<&CustomDataQuery>,
    ) -> Result<Vec<RecordBatch>> {
        let source_type = source_type.to_lowercase();
        let ticker = ticker.to_lowercase();
        let index = self.custom_partition_index().await?;
        let pruned_file_paths =
            index.file_paths_for_range(&source_type, &ticker, Some(start_day), Some(end_day));
        if pruned_file_paths.is_empty() {
            return Ok(Vec::new());
        }
        const MAX_ATTEMPTS: usize = 5;
        for attempt in 0..MAX_ATTEMPTS {
            let mut df = self.market_files_df(pruned_file_paths.iter()).await?;
            df = df
                .filter(col("source_type").eq(lit(source_type.clone())))?
                .filter(col("ticker").eq(lit(ticker.clone())))?
                .filter(col("day").gt_eq(lit(start_day)))?
                .filter(col("day").lt_eq(lit(end_day)))?;
            if let Some(query) = query {
                df = apply_custom_query_filters(df, query)?;
            }
            match df.collect().await {
                Ok(batches) => return Ok(batches),
                Err(error) if attempt + 1 < MAX_ATTEMPTS => {
                    let message = error.to_string();
                    if message.contains("No such file") || message.contains("not found") {
                        self.invalidate_table_context(CUSTOM_POINTS);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(anyhow!(
            "custom points scan for {source_type}:{ticker} failed after {MAX_ATTEMPTS} attempts"
        ))
    }

    async fn merge_data_files_into_partition_index(
        &self,
        table: &str,
        data_files: &[iceberg::spec::DataFile],
    ) -> Result<()> {
        if data_files.is_empty() {
            return Ok(());
        }
        let table_ref = self.catalog.load_table(&self.ident(table)).await?;
        let metadata = table_ref.metadata();
        let snapshot_id = metadata.current_snapshot_id();
        let spec = metadata.default_partition_spec().as_ref().clone();
        let mut indexes = self
            .partition_indexes
            .lock()
            .expect("iceberg partition index cache poisoned");
        if let Some(existing) = indexes.get(table) {
            let mut updated = existing.as_ref().clone();
            updated.snapshot_id = snapshot_id;
            for data_file in data_files {
                updated.insert_data_file(data_file, &spec)?;
            }
            indexes.insert(table.to_string(), Arc::new(updated));
        }
        Ok(())
    }

    async fn merge_data_files_into_custom_partition_index(
        &self,
        table: &str,
        data_files: &[iceberg::spec::DataFile],
    ) -> Result<()> {
        if table != CUSTOM_POINTS || data_files.is_empty() {
            return Ok(());
        }
        let table_ref = self.catalog.load_table(&self.ident(table)).await?;
        let metadata = table_ref.metadata();
        let snapshot_id = metadata.current_snapshot_id();
        let spec = metadata.default_partition_spec().as_ref().clone();
        let mut indexes = self
            .custom_partition_indexes
            .lock()
            .expect("iceberg custom partition index cache poisoned");
        if let Some(existing) = indexes.get(table) {
            let mut updated = existing.as_ref().clone();
            updated.snapshot_id = snapshot_id;
            for data_file in data_files {
                updated.insert_data_file(data_file, &spec)?;
            }
            indexes.insert(table.to_string(), Arc::new(updated));
        }
        Ok(())
    }

    fn invalidate_table_context(&self, table: &str) {
        self.table_contexts
            .lock()
            .expect("iceberg table context cache poisoned")
            .remove(table);
        self.partition_indexes
            .lock()
            .expect("iceberg partition index cache poisoned")
            .remove(table);
        self.custom_partition_indexes
            .lock()
            .expect("iceberg custom partition index cache poisoned")
            .remove(table);
    }

    fn ident(&self, name: &str) -> TableIdent {
        TableIdent::new(self.namespace.clone(), name.into())
    }
}

/// Build the Iceberg REST catalog (Lakekeeper) client.
///
/// `storage_props` are merged into the catalog `load()` props so they reach the
/// FileIO the catalog builds for every table (`iceberg-catalog-rest` 0.9.1
/// forwards catalog props into `FileIOBuilder::with_props`). For S3 this carries
/// the endpoint/region/credentials and path-style flag; the props-carrying
/// `S3ConfiguredStorageFactory` additionally forces them on every FileIO so the
/// store never depends on Lakekeeper vending a complete S3 config. The initial
/// `namespace_exists` probe in [`IcebergStore::ensure_tables`] surfaces an
/// unreachable or unbootstrapped catalog as a clear hard error.
async fn build_rest_catalog(
    connection: &RestCatalogConnection,
    storage_factory: Arc<dyn StorageFactory>,
    storage_props: HashMap<String, String>,
) -> Result<impl Catalog> {
    let mut props = storage_props;
    props.insert(REST_CATALOG_PROP_URI.to_string(), connection.uri.clone());
    props.insert(
        REST_CATALOG_PROP_WAREHOUSE.to_string(),
        connection.warehouse.clone(),
    );
    RestCatalogBuilder::default()
        .with_storage_factory(storage_factory)
        .load(CATALOG_NAME, props)
        .await
        .with_context(|| {
            format!(
                "failed to load Iceberg REST catalog at {} (warehouse '{}'). \
                 Is Lakekeeper running and bootstrapped, with that warehouse created?",
                connection.uri, connection.warehouse
            )
        })
}

/// Storage factory that carries the S3 connection properties itself.
///
/// `iceberg-catalog-rest` 0.9.1 builds its `FileIO` from the catalog props plus
/// whatever storage config Lakekeeper vends. This wrapper additionally merges
/// the configured S3 props into whatever config it is handed before delegating
/// to the OpenDAL S3 factory, so `s3.path-style-access=true` and the
/// endpoint/region/credentials are always present even if the vended config
/// omits or differs on them (a bare `OpenDalStorageFactory::S3` would otherwise
/// fail with "region is missing"). The same applies to the DataFusion
/// `IcebergTableProviderFactory` FileIO path.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct S3ConfiguredStorageFactory {
    props: HashMap<String, String>,
}

#[typetag::serde]
impl StorageFactory for S3ConfiguredStorageFactory {
    fn build(&self, config: &StorageConfig) -> iceberg::Result<Arc<dyn Storage>> {
        let mut props = self.props.clone();
        props.extend(config.props().clone());
        let inner = OpenDalStorageFactory::S3 {
            configured_scheme: "s3".to_string(),
            customized_credential_load: None,
        };
        inner.build(&StorageConfig::from_props(props))
    }
}

/// Build the props-carrying S3 storage factory for `config`.
fn s3_storage_factory(config: &S3DataStoreConfig) -> S3ConfiguredStorageFactory {
    S3ConfiguredStorageFactory {
        props: s3_file_io_props(config),
    }
}

/// The S3 FileIO property map the OpenDAL storage factory reads from the
/// catalog's props. Path-style addressing is mandatory (the bucket name may
/// break virtual-host addressing).
fn s3_file_io_props(config: &S3DataStoreConfig) -> HashMap<String, String> {
    let mut props = HashMap::new();
    props.insert("s3.endpoint".to_string(), config.endpoint.clone());
    props.insert("s3.access-key-id".to_string(), config.access_key.clone());
    props.insert(
        "s3.secret-access-key".to_string(),
        config.secret_key.clone(),
    );
    props.insert("s3.region".to_string(), config.region.clone());
    props.insert("s3.path-style-access".to_string(), "true".to_string());
    props
}

/// Extract the bucket from an `s3://bucket/prefix` warehouse URL.
fn s3_bucket(warehouse: &str) -> Result<String> {
    let rest = warehouse
        .strip_prefix("s3://")
        .ok_or_else(|| anyhow!("s3 warehouse must start with s3://, got '{warehouse}'"))?;
    let bucket = rest.split('/').next().unwrap_or("");
    if bucket.is_empty() {
        return Err(anyhow!("s3 warehouse '{warehouse}' has an empty bucket"));
    }
    Ok(bucket.to_string())
}

fn path_to_file_uri(path: &Path) -> Result<String> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(format!("file://{}", absolute.display()))
}

fn local_path_from_iceberg_file_path(file_path: &str) -> Result<String> {
    if let Some(path) = file_path.strip_prefix("file://") {
        return Ok(path.to_string());
    }
    if Path::new(file_path).is_absolute() {
        return Ok(file_path.to_string());
    }
    Err(anyhow!(
        "market partition index returned non-local Iceberg file path: {file_path}"
    ))
}

fn partition_spec(schema: Schema, columns: &[&str]) -> Result<PartitionSpec> {
    let mut builder = PartitionSpec::builder(schema);
    for column in columns {
        builder = builder.add_partition_field(*column, *column, Transform::Identity)?;
    }
    Ok(builder.build()?)
}

fn market_schema(base: Arc<ArrowSchema>) -> Schema {
    iceberg_schema_from_arrow(
        base,
        &[
            ("security_type", PrimitiveType::String, false),
            ("market", PrimitiveType::String, false),
            ("resolution", PrimitiveType::String, false),
            ("day", PrimitiveType::Date, false),
        ],
    )
}

fn option_schema(base: Arc<ArrowSchema>) -> Schema {
    iceberg_schema_from_arrow(base, &[("day", PrimitiveType::Date, false)])
}

fn custom_schema() -> Schema {
    iceberg_schema_from_arrow(
        schema::custom_data_schema(),
        &[
            ("source_type", PrimitiveType::String, false),
            ("ticker", PrimitiveType::String, false),
            ("day", PrimitiveType::Date, false),
        ],
    )
}

fn factor_schema() -> Schema {
    iceberg_schema_from_arrow(
        schema::factor_file_schema(),
        &[
            ("market", PrimitiveType::String, false),
            ("ticker", PrimitiveType::String, false),
        ],
    )
}

fn map_schema() -> Schema {
    iceberg_schema_from_arrow(
        schema::map_file_schema(),
        &[
            ("market", PrimitiveType::String, false),
            ("permtick", PrimitiveType::String, false),
        ],
    )
}

fn iceberg_schema_from_arrow(
    arrow_schema: Arc<ArrowSchema>,
    extra_fields: &[(&str, PrimitiveType, bool)],
) -> Schema {
    let mut id = 1;
    let mut fields = Vec::new();
    for field in arrow_schema.fields() {
        let iceberg_type = arrow_type_to_iceberg(field.data_type());
        let nested = if field.is_nullable() {
            NestedField::optional(id, field.name(), iceberg_type)
        } else {
            NestedField::required(id, field.name(), iceberg_type)
        };
        fields.push(nested.into());
        id += 1;
    }
    for (name, primitive, nullable) in extra_fields {
        let nested = if *nullable {
            NestedField::optional(id, *name, Type::Primitive(primitive.clone()))
        } else {
            NestedField::required(id, *name, Type::Primitive(primitive.clone()))
        };
        fields.push(nested.into());
        id += 1;
    }
    Schema::builder()
        .with_fields(fields)
        .build()
        .expect("valid Iceberg schema")
}

fn arrow_type_to_iceberg(data_type: &DataType) -> Type {
    let primitive = match data_type {
        DataType::Boolean => PrimitiveType::Boolean,
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::UInt8 => PrimitiveType::Int,
        DataType::Int64 | DataType::UInt64 => PrimitiveType::Long,
        DataType::Float32 => PrimitiveType::Float,
        DataType::Float64 => PrimitiveType::Double,
        DataType::Utf8 | DataType::LargeUtf8 => PrimitiveType::String,
        DataType::Date32 | DataType::Date64 => PrimitiveType::Date,
        DataType::Timestamp(_, _) => PrimitiveType::TimestampNs,
        _ => PrimitiveType::String,
    };
    Type::Primitive(primitive)
}

fn with_iceberg_field_ids(batch: RecordBatch, iceberg_schema: &Schema) -> Result<RecordBatch> {
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    for iceberg_field in iceberg_schema.as_struct().fields() {
        let field_name = iceberg_field.name.as_str();
        let Ok(column_idx) = batch.schema().index_of(field_name) else {
            continue;
        };
        let arrow_type = iceberg_type_to_arrow(iceberg_field.field_type.as_ref())?;
        let mut metadata = batch.schema().field(column_idx).metadata().clone();
        metadata.insert(
            PARQUET_FIELD_ID_META_KEY.to_string(),
            iceberg_field.id.to_string(),
        );
        fields.push(Arc::new(
            Field::new(field_name, arrow_type.clone(), !iceberg_field.required)
                .with_metadata(metadata),
        ));
        columns.push(cast_if_needed(
            field_name,
            batch.column(column_idx).clone(),
            &arrow_type,
        )?);
    }
    Ok(RecordBatch::try_new(
        Arc::new(ArrowSchema::new(fields)),
        columns,
    )?)
}

fn drop_null_required_rows(batch: RecordBatch) -> Result<RecordBatch> {
    let mut keep = vec![true; batch.num_rows()];
    for (field, column) in batch.schema().fields().iter().zip(batch.columns()) {
        if field.is_nullable() {
            continue;
        }
        for (row, keep_row) in keep.iter_mut().enumerate() {
            *keep_row = *keep_row && column.is_valid(row);
        }
    }
    if keep.iter().all(|keep_row| *keep_row) {
        return Ok(batch);
    }
    let mask = BooleanArray::from(keep);
    let columns = batch
        .columns()
        .iter()
        .map(|column| compute::filter(column, &mask).map_err(anyhow::Error::from))
        .collect::<Result<Vec<_>>>()?;
    Ok(RecordBatch::try_new(batch.schema(), columns)?)
}

fn iceberg_type_to_arrow(iceberg_type: &Type) -> Result<DataType> {
    match iceberg_type {
        Type::Primitive(primitive) => match primitive {
            PrimitiveType::Boolean => Ok(DataType::Boolean),
            PrimitiveType::Int => Ok(DataType::Int32),
            PrimitiveType::Long => Ok(DataType::Int64),
            PrimitiveType::Float => Ok(DataType::Float32),
            PrimitiveType::Double => Ok(DataType::Float64),
            PrimitiveType::String => Ok(DataType::Utf8),
            PrimitiveType::Date => Ok(DataType::Date32),
            PrimitiveType::TimestampNs => Ok(DataType::Timestamp(
                arrow_schema::TimeUnit::Nanosecond,
                None,
            )),
            other => Err(anyhow!(
                "unsupported Iceberg primitive type for Arrow write: {other:?}"
            )),
        },
        other => Err(anyhow!(
            "unsupported Iceberg type for Arrow write: {other:?}"
        )),
    }
}

fn cast_if_needed(field_name: &str, column: ArrayRef, data_type: &DataType) -> Result<ArrayRef> {
    if column.data_type() == data_type {
        return Ok(column);
    }
    if field_name == "symbol_sid"
        && column.data_type() == &DataType::UInt64
        && data_type == &DataType::Int64
    {
        let values = column
            .as_any()
            .downcast_ref::<UInt64Array>()
            .ok_or_else(|| anyhow!("symbol_sid must be uint64"))?;
        if values.null_count() == 0 {
            let converted = (0..values.len())
                .map(|row| values.value(row) as i64)
                .collect::<Vec<_>>();
            return Ok(Arc::new(Int64Array::from(converted)));
        }
        let converted = (0..values.len())
            .map(|row| values.is_valid(row).then(|| values.value(row) as i64))
            .collect::<Vec<_>>();
        return Ok(Arc::new(Int64Array::from(converted)));
    }
    Ok(cast(&column, data_type)?)
}

fn with_market_partitions(
    batch: RecordBatch,
    security_type: SecurityType,
    market: &str,
    resolution: Resolution,
    _tick_type: TickType,
) -> Result<RecordBatch> {
    let rows = batch.num_rows();
    let day_values = market_day_values(&batch, resolution)?;
    let columns = vec![
        Arc::new(arrow_array::StringArray::from(vec![
            security_type
                .to_string()
                .to_lowercase();
            rows
        ])) as arrow_array::ArrayRef,
        Arc::new(arrow_array::StringArray::from(vec![
            market.to_lowercase();
            rows
        ])),
        Arc::new(arrow_array::StringArray::from(vec![
            resolution
                .folder_name()
                .to_string();
            rows
        ])),
        Arc::new(arrow_array::Date32Array::from(day_values)),
    ];
    append_columns(
        batch,
        &[
            Field::new("security_type", DataType::Utf8, false),
            Field::new("market", DataType::Utf8, false),
            Field::new("resolution", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
        ],
        columns,
    )
}

fn append_columns(
    batch: RecordBatch,
    fields: &[Field],
    columns: Vec<arrow_array::ArrayRef>,
) -> Result<RecordBatch> {
    let mut new_fields = batch.schema().fields().to_vec();
    new_fields.extend(fields.iter().cloned().map(Arc::new));
    let mut new_columns = batch.columns().to_vec();
    new_columns.extend(columns);
    Ok(RecordBatch::try_new(
        Arc::new(ArrowSchema::new(new_fields)),
        new_columns,
    )?)
}

fn trade_bar_append_window(
    bars: &[TradeBar],
) -> Option<(
    lean_core::DateTime,
    lean_core::DateTime,
    HashMap<u64, Symbol>,
)> {
    let start = bars.iter().map(|bar| bar.time).min()?;
    let end = bars.iter().map(|bar| bar.end_time).max()?;
    let symbols = bars
        .iter()
        .map(|bar| (bar.symbol.id.sid, bar.symbol.clone()))
        .collect();
    Some((start, end, symbols))
}

fn quote_bar_append_window(
    bars: &[QuoteBar],
) -> Option<(
    lean_core::DateTime,
    lean_core::DateTime,
    HashMap<u64, Symbol>,
)> {
    let start = bars.iter().map(|bar| bar.time).min()?;
    let end = bars.iter().map(|bar| bar.end_time).max()?;
    let symbols = bars
        .iter()
        .map(|bar| (bar.symbol.id.sid, bar.symbol.clone()))
        .collect();
    Some((start, end, symbols))
}

fn tick_append_window(
    ticks: &[Tick],
) -> Option<(
    lean_core::DateTime,
    lean_core::DateTime,
    HashMap<u64, Symbol>,
)> {
    let start = ticks.iter().map(|tick| tick.time).min()?;
    let end = ticks.iter().map(|tick| tick.time).max()?;
    let symbols = ticks
        .iter()
        .map(|tick| (tick.symbol.id.sid, tick.symbol.clone()))
        .collect();
    Some((start, end, symbols))
}

fn partition_key_from_batch(
    spec: &PartitionSpec,
    schema: Arc<Schema>,
    batch: &RecordBatch,
) -> Result<PartitionKey> {
    let mut values = Vec::with_capacity(spec.fields().len());
    for field in spec.fields() {
        let column_idx = batch
            .schema()
            .index_of(&field.name)
            .with_context(|| format!("partition column {} missing from batch", field.name))?;
        values.push(Some(literal_from_array(batch.column(column_idx), 0)?));
    }
    Ok(PartitionKey::new(
        spec.clone(),
        schema,
        Struct::from_iter(values),
    ))
}

fn partition_fingerprint(spec: &PartitionSpec, batch: &RecordBatch, row: usize) -> Result<String> {
    let mut values = Vec::with_capacity(spec.fields().len());
    for field in spec.fields() {
        let column_idx = batch
            .schema()
            .index_of(&field.name)
            .with_context(|| format!("partition column {} missing from batch", field.name))?;
        values.push(format!(
            "{:?}",
            literal_from_array(batch.column(column_idx), row)?
        ));
    }
    Ok(values.join("|"))
}

fn literal_from_array(array: &arrow_array::ArrayRef, row: usize) -> Result<Literal> {
    if let Some(values) = array.as_any().downcast_ref::<arrow_array::StringArray>() {
        return Ok(Literal::string(values.value(row)));
    }
    if let Some(values) = array.as_any().downcast_ref::<arrow_array::Int32Array>() {
        return Ok(Literal::int(values.value(row)));
    }
    if let Some(values) = array.as_any().downcast_ref::<arrow_array::Date32Array>() {
        return Ok(Literal::int(values.value(row)));
    }
    if let Some(values) = array.as_any().downcast_ref::<arrow_array::Int64Array>() {
        return Ok(Literal::long(values.value(row)));
    }
    if let Some(values) = array.as_any().downcast_ref::<arrow_array::UInt64Array>() {
        return Ok(Literal::long(values.value(row) as i64));
    }
    Err(anyhow!(
        "unsupported partition column array type: {:?}",
        array.data_type()
    ))
}

/// Cheap upper-bound estimate of a point's serialized `fields_json` length,
/// used to keep each append batch's Utf8 column safely under Arrow's 2 GiB
/// i32-offset limit without serializing every point twice.
fn estimated_fields_json_len(point: &CustomDataPoint) -> usize {
    2 + point
        .fields
        .iter()
        .map(|(key, value)| key.len() + 6 + estimated_json_value_len(value))
        .sum::<usize>()
}

fn estimated_json_value_len(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(_) => 24,
        serde_json::Value::String(text) => text.len() + 2,
        other => serde_json::to_string(other).map(|s| s.len()).unwrap_or(64),
    }
}

fn custom_points_to_record_batch(
    source_type: &str,
    ticker: &str,
    points: &[CustomDataPoint],
) -> Result<RecordBatch> {
    let date_ns: Vec<i64> = points
        .iter()
        .map(|p| {
            p.end_time
                .map(|time| time.0)
                .unwrap_or_else(|| schema::date_to_ns(p.time))
        })
        .collect();
    let value: Vec<f64> = points
        .iter()
        .map(|p| p.value.to_f64().unwrap_or(0.0))
        .collect();
    let fields_json: Vec<String> = points
        .iter()
        .map(|p| serde_json::to_string(&p.fields).unwrap_or_else(|_| "{}".to_string()))
        .collect();
    let symbol: Vec<Option<String>> = points
        .iter()
        .map(|p| p.symbol.as_ref().map(|s| s.to_ascii_uppercase()))
        .collect();
    let day: Vec<i32> = date_ns.iter().map(|ns| days_since_epoch(*ns)).collect();
    let rows = points.len();
    let arrow_schema = Arc::new(ArrowSchema::new(vec![
        Field::new("date_ns", DataType::Int64, false),
        Field::new("value", DataType::Float64, false),
        Field::new("fields_json", DataType::Utf8, false),
        Field::new("symbol", DataType::Utf8, true),
        Field::new("source_type", DataType::Utf8, false),
        Field::new("ticker", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
    ]));
    Ok(RecordBatch::try_new(
        arrow_schema,
        vec![
            Arc::new(Int64Array::from(date_ns)),
            Arc::new(Float64Array::from(value)),
            Arc::new(StringArray::from(fields_json)),
            Arc::new(StringArray::from(symbol)),
            Arc::new(StringArray::from(vec![source_type.to_lowercase(); rows])),
            Arc::new(StringArray::from(vec![ticker.to_lowercase(); rows])),
            Arc::new(arrow_array::Date32Array::from(day)),
        ],
    )?)
}

fn custom_point_key(point: &CustomDataPoint) -> (i64, String, String) {
    let time = point
        .end_time
        .map(|time| time.0)
        .unwrap_or_else(|| schema::date_to_ns(point.time));
    // Distinct events can share a timestamp: two Unusual Whales alerts in the
    // same millisecond, or an EOD snapshot where every ticker's row carries
    // the same 16:00 stamp. The provider row id disambiguates events when
    // present; the canonical symbol separates per-ticker rows (without it, a
    // ~3,900-ticker snapshot day would dedupe down to a single row).
    let id = point
        .fields
        .get("id")
        .map(|value| value.to_string())
        .unwrap_or_default();
    let symbol = point.symbol.clone().unwrap_or_default();
    (time, id, symbol)
}

/// Maximum tolerated distance between a cached row's `date_ns` (which drives
/// the Iceberg `day` partition) and the event time re-derived from its own
/// fields. Rows beyond this are mis-partitioned: an older decoder stamped them
/// with fetch/file-metadata time (e.g. a whole backfill object cached under
/// its upload date), so day-pruned window scans can never surface them at
/// their true event time. Dropping them at read makes the cache report those
/// windows as missing, letting the provider refetch land correctly-partitioned
/// replacements instead of being deduped against unreachable rows.
const CUSTOM_POINT_MAX_PARTITION_SKEW_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

fn option_universe_key(
    row: &OptionUniverseRow,
) -> (String, chrono::NaiveDate, chrono::NaiveDate, String, String) {
    (
        row.underlying.to_ascii_uppercase(),
        row.date,
        row.expiration,
        row.strike.normalize().to_string(),
        row.right.to_ascii_uppercase(),
    )
}

fn option_eod_key(
    row: &schema::OptionEodBar,
) -> (String, chrono::NaiveDate, chrono::NaiveDate, String, String) {
    (
        row.underlying.to_ascii_uppercase(),
        row.date,
        row.expiration,
        row.strike.normalize().to_string(),
        row.right.to_ascii_uppercase(),
    )
}

fn with_option_partitions(batch: RecordBatch) -> Result<RecordBatch> {
    let rows = batch.num_rows();
    let day_values = batch
        .column_by_name("date_ns")
        .ok_or_else(|| anyhow!("date_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("date_ns must be int64"))?;
    let day: Vec<i32> = (0..rows)
        .map(|row| days_since_epoch(day_values.value(row)))
        .collect();
    append_columns(
        batch,
        &[Field::new("day", DataType::Date32, false)],
        vec![Arc::new(arrow_array::Date32Array::from(day))],
    )
}

fn with_custom_partitions(
    batch: RecordBatch,
    source_type: &str,
    ticker: &str,
) -> Result<RecordBatch> {
    let rows = batch.num_rows();
    let day_values = batch
        .column_by_name("date_ns")
        .ok_or_else(|| anyhow!("date_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("date_ns must be int64"))?;
    let day: Vec<i32> = (0..rows)
        .map(|row| days_since_epoch(day_values.value(row)))
        .collect();
    append_columns(
        batch,
        &[
            Field::new("source_type", DataType::Utf8, false),
            Field::new("ticker", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
        ],
        vec![
            Arc::new(StringArray::from(vec![source_type.to_lowercase(); rows])),
            Arc::new(StringArray::from(vec![ticker.to_lowercase(); rows])),
            Arc::new(arrow_array::Date32Array::from(day)),
        ],
    )
}

fn append_custom_batch_points(
    batch: &RecordBatch,
    out: &mut Vec<CustomDataPoint>,
    projected_fields: Option<&HashSet<String>>,
) -> Result<()> {
    let date_ns = batch
        .column_by_name("date_ns")
        .ok_or_else(|| anyhow!("date_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("date_ns must be int64"))?;
    let value = batch
        .column_by_name("value")
        .ok_or_else(|| anyhow!("value column missing"))?
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| anyhow!("value must be float64"))?;
    let fields_json = batch
        .column_by_name("fields_json")
        .ok_or_else(|| anyhow!("fields_json column missing"))?;
    // Tolerate tables/batches written before the `symbol` column existed.
    let symbol_column = batch.column_by_name("symbol");
    for row in 0..batch.num_rows() {
        let mut fields: HashMap<String, serde_json::Value> =
            match optional_string_at(fields_json, row).filter(|raw| !raw.is_empty()) {
                None => HashMap::new(),
                Some(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            };
        let date_ns = date_ns.value(row);
        let end_time_ns = custom_point_end_time(&fields, date_ns).unwrap_or(date_ns);
        if (end_time_ns - date_ns).abs() > CUSTOM_POINT_MAX_PARTITION_SKEW_NS {
            continue;
        }
        if let Some(projected_fields) = projected_fields {
            fields.retain(|key, _| projected_fields.contains(key));
        }
        let symbol = symbol_column
            .and_then(|column| optional_string_at(column, row))
            .map(|value| value.trim().to_ascii_uppercase())
            .filter(|value| !value.is_empty());
        out.push(
            CustomDataPoint::new(
                schema::ns_to_date(end_time_ns),
                Some(lean_core::NanosecondTimestamp(end_time_ns)),
                rust_decimal::Decimal::from_f64(value.value(row)).unwrap_or_default(),
                fields,
            )
            .with_symbol(symbol),
        );
    }
    Ok(())
}

fn optional_string_at(array: &ArrayRef, row: usize) -> Option<String> {
    if array.is_null(row) {
        return None;
    }
    if let Some(values) = array.as_any().downcast_ref::<StringArray>() {
        return Some(values.value(row).to_string());
    }
    if let Some(values) = array.as_any().downcast_ref::<LargeStringArray>() {
        return Some(values.value(row).to_string());
    }
    if let Some(values) = array
        .as_any()
        .downcast_ref::<arrow_array::StringViewArray>()
    {
        return string_view_value(values, row);
    }
    None
}

fn string_view_value(values: &arrow_array::StringViewArray, row: usize) -> Option<String> {
    let view = *values.views().get(row)?;
    let len = (view as u32) as usize;
    let bytes = if len <= MAX_INLINE_VIEW_LEN as usize {
        // SAFETY: Arrow stores <=12 byte Utf8View values inline in the view word.
        unsafe { arrow_array::StringViewArray::inline_value(&view, len) }
    } else {
        let view = ByteView::from(view);
        let buffer = values.data_buffers().get(view.buffer_index as usize)?;
        let start = view.offset as usize;
        let end = start.checked_add(view.length as usize)?;
        buffer.get(start..end)?
    };
    std::str::from_utf8(bytes).ok().map(str::to_string)
}

fn apply_custom_query_filters(mut df: DataFrame, query: &CustomDataQuery) -> Result<DataFrame> {
    if let Some(symbols) = &query.symbols {
        // Only push the symbol filter down when the scanned files actually carry
        // the canonical `symbol` column. Files written before the column existed
        // lack it; row-level `custom_point_matches_query` still filters those.
        let has_symbol_column = df
            .schema()
            .fields()
            .iter()
            .any(|field| field.name() == "symbol");
        if has_symbol_column && !symbols.is_empty() {
            let uppercased = symbols
                .iter()
                .map(|symbol| lit(symbol.to_ascii_uppercase()))
                .collect::<Vec<_>>();
            df = df.filter(col("symbol").in_list(uppercased, false))?;
        }
    }

    for (field, expected) in &query.string_equals {
        df = df.filter(json_string_field_expr(field, expected))?;
    }

    for (field, values) in &query.string_in {
        if values.is_empty() {
            continue;
        }
        let mut expr: Option<Expr> = None;
        for value in values {
            let next = json_string_field_expr(field, value);
            expr = Some(match expr {
                Some(existing) => existing.or(next),
                None => next,
            });
        }
        if let Some(expr) = expr {
            df = df.filter(expr)?;
        }
    }

    Ok(df)
}

fn custom_projected_fields(query: Option<&CustomDataQuery>) -> Option<HashSet<String>> {
    let query = query?;
    let mut fields = HashSet::new();
    if let Some(columns) = &query.columns {
        fields.extend(columns.iter().cloned());
    }
    fields.extend(
        query
            .properties
            .get("columns")
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );
    fields.extend(query.string_equals.keys().cloned());
    fields.extend(query.string_in.keys().cloned());
    fields.extend(query.numeric_min.keys().cloned());
    fields.extend(query.numeric_max.keys().cloned());
    fields.extend(
        query
            .properties
            .get("required_columns")
            .into_iter()
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string),
    );
    if fields.is_empty() {
        return None;
    }
    fields.insert("end_time".to_string());
    fields.insert("time".to_string());
    Some(fields)
}

fn json_string_field_expr(field: &str, value: &str) -> Expr {
    let field = escape_like_value(field);
    let value = escape_like_value(value);
    col("fields_json")
        .like(lit(format!("%\"{field}\":\"{value}\"%")))
        .or(col("fields_json").like(lit(format!("%\"{field}\": \"{value}\"%"))))
}

fn escape_like_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn custom_point_end_time(fields: &HashMap<String, serde_json::Value>, date_ns: i64) -> Option<i64> {
    for key in ["end_time_ns", "time_ns", "timestamp_ns"] {
        if let Some(ns) = fields.get(key).and_then(json_i64) {
            return Some(ns);
        }
    }
    for key in ["end_time", "start_time"] {
        if let Some(raw) = fields.get(key).and_then(json_i64) {
            return Some(crate::custom_ingest::epoch_value_to_ns(raw));
        }
    }
    let fallback_date = schema::ns_to_date(date_ns);
    for key in ["time", "bar_time", "datetime"] {
        if let Some(text) = fields.get(key).and_then(|value| value.as_str()) {
            if let Some(end_time) =
                crate::custom_ingest::parse_tradealert_timestamp(text, fallback_date)
            {
                return Some(end_time.0);
            }
        }
    }
    None
}

fn json_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
}

fn market_day_values(batch: &RecordBatch, resolution: Resolution) -> Result<Vec<i32>> {
    let timestamp_column =
        if resolution == Resolution::Daily && batch.column_by_name("end_time_ns").is_some() {
            "end_time_ns"
        } else {
            "time_ns"
        };
    let time_ns = batch
        .column_by_name(timestamp_column)
        .ok_or_else(|| anyhow!("{timestamp_column} column missing"))?
        .as_any()
        .downcast_ref::<arrow_array::Int64Array>()
        .ok_or_else(|| anyhow!("{timestamp_column} column must be int64"))?;
    Ok((0..batch.num_rows())
        .map(|row| days_since_epoch(time_ns.value(row)))
        .collect())
}

fn days_since_epoch(ns: i64) -> i32 {
    (ns / 1_000_000_000 / 86_400) as i32
}

fn append_trade_batch_grouped(
    batch: &RecordBatch,
    symbols_by_sid: &HashMap<u64, Symbol>,
    out: &mut HashMap<u64, Vec<TradeBar>>,
) -> Result<()> {
    for row in 0..batch.num_rows() {
        let sid = sid_value(batch, row)?;
        if let Some(symbol) = symbols_by_sid.get(&sid) {
            let single = batch.slice(row, 1);
            out.entry(sid)
                .or_default()
                .extend(convert::record_batch_to_trade_bars(&single, symbol.clone()));
        }
    }
    Ok(())
}

fn append_quote_batch_grouped(
    batch: &RecordBatch,
    symbols_by_sid: &HashMap<u64, Symbol>,
    out: &mut HashMap<u64, Vec<QuoteBar>>,
) -> Result<()> {
    for row in 0..batch.num_rows() {
        let sid = sid_value(batch, row)?;
        if let Some(symbol) = symbols_by_sid.get(&sid) {
            let single = batch.slice(row, 1);
            out.entry(sid)
                .or_default()
                .extend(convert::record_batch_to_quote_bars(&single, symbol.clone()));
        }
    }
    Ok(())
}

fn append_tick_batch_grouped(
    batch: &RecordBatch,
    symbols_by_sid: &HashMap<u64, Symbol>,
    out: &mut HashMap<u64, Vec<Tick>>,
) -> Result<()> {
    for row in 0..batch.num_rows() {
        let sid = sid_value(batch, row)?;
        if let Some(symbol) = symbols_by_sid.get(&sid) {
            let single = batch.slice(row, 1);
            out.entry(sid)
                .or_default()
                .extend(convert::record_batch_to_ticks(&single, symbol.clone()));
        }
    }
    Ok(())
}

fn sid_value(batch: &RecordBatch, row: usize) -> Result<u64> {
    let column = batch
        .column_by_name("symbol_sid")
        .ok_or_else(|| anyhow!("symbol_sid column missing"))?;
    if let Some(values) = column.as_any().downcast_ref::<arrow_array::UInt64Array>() {
        return Ok(values.value(row));
    }
    if let Some(values) = column.as_any().downcast_ref::<Int64Array>() {
        return Ok(values.value(row) as u64);
    }
    Err(anyhow!("symbol_sid must be uint64 or int64"))
}

fn append_trade_batch_by_symbol_value(
    batch: &RecordBatch,
    symbols_by_value: &HashMap<String, Symbol>,
    out: &mut Vec<TradeBar>,
) -> Result<()> {
    let symbol_col = batch
        .column_by_name("symbol_value")
        .ok_or_else(|| anyhow!("symbol_value column missing"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("symbol_value must be utf8"))?;
    for row in 0..batch.num_rows() {
        if let Some(symbol) = symbols_by_value.get(symbol_col.value(row)) {
            out.extend(convert::record_batch_to_trade_bars(
                &batch.slice(row, 1),
                symbol.clone(),
            ));
        }
    }
    Ok(())
}

fn append_quote_batch_by_symbol_value(
    batch: &RecordBatch,
    symbols_by_value: &HashMap<String, Symbol>,
    out: &mut Vec<QuoteBar>,
) -> Result<()> {
    let symbol_col = batch
        .column_by_name("symbol_value")
        .ok_or_else(|| anyhow!("symbol_value column missing"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("symbol_value must be utf8"))?;
    for row in 0..batch.num_rows() {
        if let Some(symbol) = symbols_by_value.get(symbol_col.value(row)) {
            out.extend(convert::record_batch_to_quote_bars(
                &batch.slice(row, 1),
                symbol.clone(),
            ));
        }
    }
    Ok(())
}

fn append_tick_batch_by_symbol_value(
    batch: &RecordBatch,
    symbols_by_value: &HashMap<String, Symbol>,
    out: &mut Vec<Tick>,
) -> Result<()> {
    let symbol_col = batch
        .column_by_name("symbol_value")
        .ok_or_else(|| anyhow!("symbol_value column missing"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("symbol_value must be utf8"))?;
    for row in 0..batch.num_rows() {
        if let Some(symbol) = symbols_by_value.get(symbol_col.value(row)) {
            out.extend(convert::record_batch_to_ticks(
                &batch.slice(row, 1),
                symbol.clone(),
            ));
        }
    }
    Ok(())
}

fn append_factor_batch(batch: &RecordBatch, out: &mut Vec<FactorFileEntry>) -> Result<()> {
    let date_ns = batch
        .column_by_name("date_ns")
        .ok_or_else(|| anyhow!("date_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("date_ns must be int64"))?;
    let price_factor = batch
        .column_by_name("price_factor")
        .ok_or_else(|| anyhow!("price_factor column missing"))?
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| anyhow!("price_factor must be float64"))?;
    let split_factor = batch
        .column_by_name("split_factor")
        .ok_or_else(|| anyhow!("split_factor column missing"))?
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| anyhow!("split_factor must be float64"))?;
    let reference_price = batch
        .column_by_name("reference_price")
        .ok_or_else(|| anyhow!("reference_price column missing"))?
        .as_any()
        .downcast_ref::<Float64Array>()
        .ok_or_else(|| anyhow!("reference_price must be float64"))?;
    for row in 0..batch.num_rows() {
        out.push(FactorFileEntry {
            date: schema::ns_to_date(date_ns.value(row)),
            price_factor: price_factor.value(row),
            split_factor: split_factor.value(row),
            reference_price: reference_price.value(row),
        });
    }
    Ok(())
}

fn factor_entries_to_record_batch(
    market: &str,
    ticker: &str,
    entries: &[FactorFileEntry],
) -> Result<RecordBatch> {
    let rows = entries.len();
    let arrow_schema = Arc::new(ArrowSchema::new(vec![
        Field::new("date_ns", DataType::Int64, false),
        Field::new("price_factor", DataType::Float64, false),
        Field::new("split_factor", DataType::Float64, false),
        Field::new("reference_price", DataType::Float64, false),
        Field::new("market", DataType::Utf8, false),
        Field::new("ticker", DataType::Utf8, false),
    ]));
    Ok(RecordBatch::try_new(
        arrow_schema,
        vec![
            Arc::new(Int64Array::from(
                entries
                    .iter()
                    .map(|entry| entry.date_ns())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                entries
                    .iter()
                    .map(|entry| entry.price_factor)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                entries
                    .iter()
                    .map(|entry| entry.split_factor)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(Float64Array::from(
                entries
                    .iter()
                    .map(|entry| entry.reference_price)
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(vec![market.to_lowercase(); rows])),
            Arc::new(StringArray::from(vec![ticker.to_lowercase(); rows])),
        ],
    )?)
}

fn append_map_batch(batch: &RecordBatch, out: &mut Vec<MapFileEntry>) -> Result<()> {
    let date_ns = batch
        .column_by_name("date_ns")
        .ok_or_else(|| anyhow!("date_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("date_ns must be int64"))?;
    let ticker = batch
        .column_by_name("ticker")
        .ok_or_else(|| anyhow!("ticker column missing"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("ticker must be utf8"))?;
    for row in 0..batch.num_rows() {
        out.push(MapFileEntry {
            date: schema::ns_to_date(date_ns.value(row)),
            ticker: ticker.value(row).to_ascii_uppercase(),
        });
    }
    Ok(())
}

fn map_entries_to_record_batch(
    market: &str,
    _ticker: &str,
    entries: &[MapFileEntry],
) -> Result<RecordBatch> {
    let rows = entries.len();
    let arrow_schema = Arc::new(ArrowSchema::new(vec![
        Field::new("date_ns", DataType::Int64, false),
        Field::new("ticker", DataType::Utf8, false),
        Field::new("market", DataType::Utf8, false),
        Field::new("permtick", DataType::Utf8, false),
    ]));
    Ok(RecordBatch::try_new(
        arrow_schema,
        vec![
            Arc::new(Int64Array::from(
                entries
                    .iter()
                    .map(|entry| entry.date_ns())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(
                entries
                    .iter()
                    .map(|entry| entry.ticker.to_ascii_uppercase())
                    .collect::<Vec<_>>(),
            )),
            Arc::new(StringArray::from(vec![market.to_lowercase(); rows])),
            Arc::new(StringArray::from(vec![_ticker.to_lowercase(); rows])),
        ],
    )?)
}

fn append_option_eod_batch(batch: &RecordBatch, out: &mut Vec<schema::OptionEodBar>) -> Result<()> {
    let date_ns = batch
        .column_by_name("date_ns")
        .ok_or_else(|| anyhow!("date_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("date_ns must be int64"))?;
    let symbol_value = batch
        .column_by_name("symbol_value")
        .ok_or_else(|| anyhow!("symbol_value column missing"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("symbol_value must be utf8"))?;
    let underlying = batch
        .column_by_name("underlying")
        .ok_or_else(|| anyhow!("underlying column missing"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("underlying must be utf8"))?;
    let expiration_ns = batch
        .column_by_name("expiration_ns")
        .ok_or_else(|| anyhow!("expiration_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("expiration_ns must be int64"))?;
    let strike = batch
        .column_by_name("strike")
        .ok_or_else(|| anyhow!("strike column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("strike must be int64"))?;
    let right = batch
        .column_by_name("right")
        .ok_or_else(|| anyhow!("right column missing"))?
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| anyhow!("right must be utf8"))?;
    let open = batch
        .column_by_name("open")
        .ok_or_else(|| anyhow!("open column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("open must be int64"))?;
    let high = batch
        .column_by_name("high")
        .ok_or_else(|| anyhow!("high column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("high must be int64"))?;
    let low = batch
        .column_by_name("low")
        .ok_or_else(|| anyhow!("low column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("low must be int64"))?;
    let close = batch
        .column_by_name("close")
        .ok_or_else(|| anyhow!("close column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("close must be int64"))?;
    let volume = batch
        .column_by_name("volume")
        .ok_or_else(|| anyhow!("volume column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("volume must be int64"))?;
    let bid = batch
        .column_by_name("bid")
        .ok_or_else(|| anyhow!("bid column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("bid must be int64"))?;
    let ask = batch
        .column_by_name("ask")
        .ok_or_else(|| anyhow!("ask column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("ask must be int64"))?;
    let bid_size = batch
        .column_by_name("bid_size")
        .ok_or_else(|| anyhow!("bid_size column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("bid_size must be int64"))?;
    let ask_size = batch
        .column_by_name("ask_size")
        .ok_or_else(|| anyhow!("ask_size column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("ask_size must be int64"))?;

    for row in 0..batch.num_rows() {
        out.push(schema::OptionEodBar {
            date: schema::ns_to_date(date_ns.value(row)),
            symbol_value: symbol_value.value(row).to_string(),
            underlying: underlying.value(row).to_string(),
            expiration: schema::ns_to_date(expiration_ns.value(row)),
            strike: schema::i64_to_price(strike.value(row)),
            right: right.value(row).to_string(),
            open: schema::i64_to_price(open.value(row)),
            high: schema::i64_to_price(high.value(row)),
            low: schema::i64_to_price(low.value(row)),
            close: schema::i64_to_price(close.value(row)),
            volume: volume.value(row),
            bid: schema::i64_to_price(bid.value(row)),
            ask: schema::i64_to_price(ask.value(row)),
            bid_size: bid_size.value(row),
            ask_size: ask_size.value(row),
        });
    }
    Ok(())
}

fn sort_grouped_trade_bars(out: &mut HashMap<u64, Vec<TradeBar>>) {
    for bars in out.values_mut() {
        bars.sort_by_key(|bar| (bar.time.0, bar.end_time.0, bar.symbol.id.sid));
        bars.dedup_by_key(|bar| (bar.time.0, bar.end_time.0, bar.symbol.id.sid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{Float64Array, Int64Array, StringArray};
    use arrow_schema::{DataType, Field, Schema as ArrowSchema};

    #[test]
    fn custom_projected_fields_includes_requested_and_filter_columns() {
        let mut query = CustomDataQuery {
            columns: Some(vec!["usymbol".to_string(), "norm_edge".to_string()]),
            ..Default::default()
        };
        query.numeric_min.insert("adv".to_string(), 1.0);
        query
            .properties
            .insert("columns".to_string(), "size,flag".to_string());
        query
            .properties
            .insert("required_columns".to_string(), "bid, ask".to_string());

        let fields = custom_projected_fields(Some(&query)).expect("projected fields");

        assert!(fields.contains("usymbol"));
        assert!(fields.contains("norm_edge"));
        assert!(fields.contains("size"));
        assert!(fields.contains("flag"));
        assert!(fields.contains("adv"));
        assert!(fields.contains("bid"));
        assert!(fields.contains("ask"));
        assert!(fields.contains("end_time"));
    }

    #[test]
    fn custom_projected_fields_keeps_all_fields_without_projection_request() {
        assert!(custom_projected_fields(Some(&CustomDataQuery::default())).is_none());
    }

    #[test]
    fn append_custom_batch_points_retains_only_projected_fields() {
        let schema = Arc::new(ArrowSchema::new(vec![
            Field::new("date_ns", DataType::Int64, false),
            Field::new("value", DataType::Float64, false),
            Field::new("fields_json", DataType::Utf8, false),
        ]));
        let date_ns = schema::date_to_ns(chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap());
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![date_ns])),
                Arc::new(Float64Array::from(vec![42.0])),
                Arc::new(StringArray::from(vec![serde_json::json!({
                    "usymbol": "SPY",
                    "norm_edge": 1.25,
                    "unused_payload": "large"
                })
                .to_string()])),
            ],
        )
        .unwrap();
        let projected = HashSet::from(["usymbol".to_string()]);
        let mut out = Vec::new();

        append_custom_batch_points(&batch, &mut out, Some(&projected)).unwrap();

        assert_eq!(out.len(), 1);
        assert!(out[0].fields.contains_key("usymbol"));
        assert!(!out[0].fields.contains_key("norm_edge"));
        assert!(!out[0].fields.contains_key("unused_payload"));
    }
}
