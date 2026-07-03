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
use iceberg::io::LocalFsStorageFactory;
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
use iceberg_catalog_sql::{SqlBindStyle, SqlCatalogBuilder};
use iceberg_datafusion::IcebergTableProviderFactory;
use lean_core::{Resolution, SecurityType, Symbol, TickType};
use lean_data::{
    CustomDataPoint, CustomDataQuery, MarginInterestRate, PerpetualContext, QuoteBar, Tick,
    TradeBar,
};
use parquet::arrow::PARQUET_FIELD_ID_META_KEY;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::prelude::ToPrimitive;
use sqlx::migrate::MigrateDatabase;
use sqlx::sqlite::Sqlite;

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

#[derive(Clone)]
pub struct IcebergStore {
    warehouse_root: PathBuf,
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
            warehouse_root,
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

    pub fn warehouse_root(&self) -> &Path {
        &self.warehouse_root
    }

    pub async fn reset_table(&self, name: &str) -> Result<()> {
        let ident = self.ident(name);
        if self.catalog.table_exists(&ident).await? {
            self.catalog
                .drop_table(&ident)
                .await
                .with_context(|| format!("failed to drop Iceberg table {NAMESPACE}.{name}"))?;
        }

        let table_dir = self.warehouse_root.join(NAMESPACE).join(name);
        match tokio::fs::remove_dir_all(&table_dir).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("failed to remove {}", table_dir.display()));
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
        let location = path_to_file_uri(&self.warehouse_root.join(NAMESPACE).join(name))?;
        let creation = TableCreation::builder()
            .name(name.into())
            .schema(schema)
            .partition_spec(spec.into_unbound())
            .location(location)
            .build();
        self.catalog
            .create_table(&self.namespace, creation)
            .await
            .with_context(|| format!("failed to create Iceberg table {NAMESPACE}.{name}"))?;
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
        for batch in &batches {
            append_custom_batch_points(batch, &mut out)?;
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
        let points = self
            .dedupe_custom_points_for_append(source_type, ticker, points)
            .await?;
        if points.is_empty() {
            return Ok(());
        }
        let batch = custom_points_to_record_batch(source_type, ticker, &points)?;
        self.insert_batch(CUSTOM_POINTS, batch).await
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
        let dates = points
            .iter()
            .map(|point| point.time)
            .collect::<HashSet<_>>();
        let mut existing = HashSet::new();
        for date in dates {
            for point in self.scan_custom_points(source_type, ticker, date).await? {
                existing.insert(custom_point_key(&point));
            }
        }
        Ok(points
            .iter()
            .filter(|point| !existing.contains(&custom_point_key(point)))
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
        append_custom_batch_points(&batch, &mut points)?;
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
                Arc::new(LocalFsStorageFactory),
            )),
        );
        let ctx = SessionContext::new_with_state(state);
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
            .map(|path| local_path_from_iceberg_file_path(path))
            .collect::<Result<Vec<_>>>()?;
        let ctx = SessionContext::new();
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
    let day: Vec<i32> = date_ns.iter().map(|ns| days_since_epoch(*ns)).collect();
    let rows = points.len();
    let arrow_schema = Arc::new(ArrowSchema::new(vec![
        Field::new("date_ns", DataType::Int64, false),
        Field::new("value", DataType::Float64, false),
        Field::new("fields_json", DataType::Utf8, false),
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
            Arc::new(StringArray::from(vec![source_type.to_lowercase(); rows])),
            Arc::new(StringArray::from(vec![ticker.to_lowercase(); rows])),
            Arc::new(arrow_array::Date32Array::from(day)),
        ],
    )?)
}

fn custom_point_key(point: &CustomDataPoint) -> i64 {
    point
        .end_time
        .map(|time| time.0)
        .unwrap_or_else(|| schema::date_to_ns(point.time))
}

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

fn append_custom_batch_points(batch: &RecordBatch, out: &mut Vec<CustomDataPoint>) -> Result<()> {
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
    for row in 0..batch.num_rows() {
        let fields = match optional_string_at(fields_json, row).filter(|raw| !raw.is_empty()) {
            None => HashMap::new(),
            Some(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        };
        let date_ns = date_ns.value(row);
        let end_time = custom_point_end_time(&fields, date_ns).unwrap_or(date_ns);
        out.push(CustomDataPoint {
            time: schema::ns_to_date(date_ns),
            end_time: Some(lean_core::NanosecondTimestamp(end_time)),
            value: rust_decimal::Decimal::from_f64(value.value(row)).unwrap_or_default(),
            fields,
        });
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
        if !symbols.is_empty() {
            let mut expr: Option<Expr> = None;
            for symbol in symbols {
                let next = json_string_field_expr("usymbol", symbol);
                expr = Some(match expr {
                    Some(existing) => existing.or(next),
                    None => next,
                });
            }
            if let Some(expr) = expr {
                df = df.filter(expr)?;
            }
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
    let fallback_date = schema::ns_to_date(date_ns);
    for key in [
        "current_time",
        "time",
        "bar_time",
        "datetime",
        "end_time",
        "timestamp",
    ] {
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
