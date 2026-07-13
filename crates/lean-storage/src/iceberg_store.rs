use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use arrow::compute;
use arrow_array::{
    Array, ArrayRef, BooleanArray, Float64Array, Int64Array, LargeStringArray, RecordBatch,
    StringArray, UInt64Array,
};
use arrow_cast::cast;
use arrow_data::{ByteView, MAX_INLINE_VIEW_LEN};
use arrow_schema::{DataType, Field, Schema as ArrowSchema};
use aws_config::default_provider::credentials::DefaultCredentialsChain;
use aws_config::Region;
use aws_credential_types::provider::{ProvideCredentials, SharedCredentialsProvider};
use datafusion::execution::session_state::SessionStateBuilder;
use datafusion::prelude::ParquetReadOptions;
use datafusion::prelude::*;
use iceberg::io::{Storage, StorageConfig, StorageFactory};
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
use iceberg_datafusion::IcebergTableProviderFactory;
use iceberg_storage_opendal::OpenDalStorageFactory;
use lean_core::{Resolution, SecurityType, Symbol, TickType};
use lean_data::{
    CustomDataPoint, CustomDataQuery, MarginInterestRate, PerpetualContext, QuoteBar, Tick,
    TradeBar,
};
use object_store::aws::{AmazonS3Builder, AwsCredential};
use object_store::CredentialProvider;
use parquet::arrow::PARQUET_FIELD_ID_META_KEY;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::prelude::ToPrimitive;
use url::Url;

use crate::sigv4_proxy::SigV4Proxy;

static APPEND_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

use crate::convert;
use crate::partition_index::{
    CustomPartitionFields, CustomPartitionIndex, MarketPartitionDayQuery, MarketPartitionFields,
    MarketPartitionIndex,
};
use crate::schema::{self, FactorFileEntry, MapFileEntry, OptionUniverseRow};
use crate::QueryParams;

const CATALOG_NAME: &str = "rlean";
/// Conventional default Iceberg namespace for rlean's cache tables. Callers set
/// [`RestCatalogConfig::namespace`] explicitly; this is the value used when they
/// want the default.
pub const DEFAULT_NAMESPACE: &str = "lean";

/// Default staleness recheck interval for cached table snapshots, in seconds.
///
/// Before reusing a cached DataFusion context or partition index, the store
/// re-reads the table's current metadata location from the catalog and rebuilds
/// if another process committed. To avoid a catalog round-trip on every scan,
/// that check runs at most once per this many seconds per table; within the
/// window the cached snapshot is trusted. `0` rechecks on every read.
pub const DEFAULT_DATA_REFRESH_SECS: u64 = 30;

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

fn concat_record_batches(batches: &[RecordBatch]) -> Result<RecordBatch> {
    let Some(first) = batches.first() else {
        return Ok(RecordBatch::new_empty(Arc::new(ArrowSchema::empty())));
    };
    Ok(compute::concat_batches(&first.schema(), batches)?)
}

/// Connection to a REST Iceberg catalog. This is the ONLY way to build an
/// [`IcebergStore`].
#[derive(Clone, Debug)]
pub struct RestCatalogConfig {
    /// REST catalog base URI, e.g. `https://s3tables.us-west-2.amazonaws.com/iceberg`.
    pub uri: String,
    /// Warehouse identifier. For S3 Tables: the table-bucket ARN.
    pub warehouse: String,
    /// SigV4 signing settings. `Some` => sign catalog requests with SigV4
    /// (AWS S3 Tables / Glue). `None` => no signing (plain / OAuth REST catalogs).
    pub sigv4: Option<SigV4Config>,
    /// Iceberg namespace that holds rlean's cache tables. Defaults to
    /// [`DEFAULT_NAMESPACE`] (`"lean"`) via [`RestCatalogConfig::default`];
    /// tests and scratch environments point this at an isolated namespace so
    /// they never touch the production tables.
    pub namespace: String,
    /// How often (seconds) a cached table snapshot is rechecked against the
    /// catalog for commits made by other processes. See
    /// [`DEFAULT_DATA_REFRESH_SECS`]. `0` rechecks on every read.
    pub data_refresh_secs: u64,
}

impl Default for RestCatalogConfig {
    fn default() -> Self {
        Self {
            uri: String::new(),
            warehouse: String::new(),
            sigv4: None,
            namespace: DEFAULT_NAMESPACE.to_string(),
            data_refresh_secs: DEFAULT_DATA_REFRESH_SECS,
        }
    }
}

/// SigV4 signing settings for an AWS-backed REST catalog.
#[derive(Clone, Debug)]
pub struct SigV4Config {
    /// SigV4 signing region, e.g. `us-west-2`.
    pub region: String,
    /// SigV4 signing name / service, e.g. `s3tables`.
    pub signing_name: String,
}

/// The temporary AWS credentials resolved once at [`IcebergStore::connect`]
/// time and fed explicitly into the iceberg S3 FileIO props. `object_store`'s
/// native AWS providers cannot read the local SSO/login cache, so the resolved
/// keys must be passed in directly. The DataFusion object store does NOT use
/// this snapshot — it uses [`RefreshingS3CredentialProvider`] so it can outlive
/// the vend TTL (see below).
#[derive(Clone)]
struct ResolvedS3Credentials {
    region: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
}

/// An `object_store` credential provider that re-resolves AWS credentials from
/// the ambient credential chain on every call, so a long-running backtest keeps
/// signing S3 requests with fresh credentials.
///
/// The DataFusion object stores registered for `s3://` data-file buckets must
/// stay valid for the whole run. AWS S3 Tables (and any SSO/`credential_process`
/// profile) vends *temporary* credentials with a short TTL — commonly 15 minutes
/// to an hour. If the object store is built with a one-time static snapshot of
/// those keys (as `AmazonS3Builder::with_access_key_id` does), every S3 `HEAD`
/// and `GET` after the TTL is rejected (`400 Bad Request` / `ExpiredToken`),
/// killing data-heavy backtests part-way through while light ones that finish
/// inside the TTL pass.
///
/// The ambient [`SharedCredentialsProvider`] caches credentials and refreshes
/// them before they expire, so resolving through it on each request yields
/// valid credentials for the store's lifetime. This mirrors what the SigV4
/// signing proxy already does for catalog requests.
#[derive(Debug)]
struct RefreshingS3CredentialProvider {
    provider: SharedCredentialsProvider,
}

#[async_trait::async_trait]
impl CredentialProvider for RefreshingS3CredentialProvider {
    type Credential = AwsCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<AwsCredential>> {
        let credentials = self
            .provider
            .provide_credentials()
            .await
            .map_err(|source| object_store::Error::Generic {
                store: "S3",
                source: Box::new(source),
            })?;
        Ok(Arc::new(AwsCredential {
            key_id: credentials.access_key_id().to_string(),
            secret_key: credentials.secret_access_key().to_string(),
            token: credentials.session_token().map(str::to_string),
        }))
    }
}

#[derive(Clone)]
pub struct IcebergStore {
    catalog: Arc<dyn Catalog>,
    namespace: NamespaceIdent,
    /// Resolved S3 credentials for the iceberg FileIO props (catalog + manifest
    /// reads). `None` for a non-AWS (unsigned) REST catalog.
    s3_credentials: Option<ResolvedS3Credentials>,
    /// Ambient AWS credential provider used to build a refreshing credential
    /// provider for the DataFusion `s3://` data-file object stores. Held so the
    /// object stores can re-resolve credentials on every request and survive the
    /// vend TTL. `None` for a non-AWS (unsigned) REST catalog.
    s3_credentials_provider: Option<SharedCredentialsProvider>,
    /// Held purely to keep the SigV4 signing proxy (and its localhost listener)
    /// alive for the store's lifetime; the proxy aborts its task on drop. The
    /// leading underscore marks it as an RAII guard that is never read directly.
    /// `None` when no signing was requested.
    _sigv4_proxy: Option<Arc<SigV4Proxy>>,
    /// Purge-drop endpoint for AWS S3 Tables. `iceberg-catalog-rest`'s
    /// `drop_table` sends a plain `DELETE` with no `purgeRequested` flag, which
    /// S3 Tables rejects ("S3 Tables only supports dropping tables with purge
    /// enabled"). When SigV4 signing is on, [`reset_table`] instead issues a
    /// signed `DELETE .../tables/{name}?purgeRequested=true` through the proxy.
    /// `None` for a non-AWS (unsigned) REST catalog, where the trait
    /// `drop_table` works.
    purge_dropper: Option<PurgeDropper>,
    /// Staleness recheck interval for cached table snapshots. See
    /// [`RestCatalogConfig::data_refresh_secs`].
    data_refresh: Duration,
    table_contexts: Arc<Mutex<HashMap<String, Arc<IcebergTableContext>>>>,
    partition_indexes: Arc<Mutex<HashMap<String, CachedMarketIndex>>>,
    custom_partition_indexes: Arc<Mutex<HashMap<String, CachedCustomIndex>>>,
    table_write_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

struct IcebergTableContext {
    ctx: SessionContext,
    /// Catalog metadata location this context's external table was built from.
    /// A cached context is reused only while the catalog still reports this same
    /// location; a different location means another process committed and the
    /// context is rebuilt.
    metadata_location: String,
    /// Last time this entry's `metadata_location` was checked against the
    /// catalog. Throttles the staleness recheck to at most once per
    /// [`IcebergStore::data_refresh`] window. `Mutex` so the throttle timestamp
    /// can be bumped through the `Arc`-shared cached context without rebuilding.
    last_checked: Mutex<Instant>,
}

/// A cached [`MarketPartitionIndex`] plus the metadata location it was built
/// from and the last time that location was rechecked against the catalog.
struct CachedMarketIndex {
    index: Arc<MarketPartitionIndex>,
    metadata_location: String,
    last_checked: Instant,
}

/// A cached [`CustomPartitionIndex`] plus its build-time metadata location and
/// last recheck instant.
struct CachedCustomIndex {
    index: Arc<CustomPartitionIndex>,
    metadata_location: String,
    last_checked: Instant,
}

/// Issues purge-enabled table drops against an AWS S3 Tables REST catalog.
///
/// The store points this at the SigV4 signing proxy's local base URI, so a
/// plain `reqwest` request here is transparently signed and forwarded to the
/// real catalog. `prefix` is the catalog's `overrides.prefix` from `/v1/config`
/// (S3 Tables scopes every path under the warehouse), captured once at connect.
#[derive(Clone)]
struct PurgeDropper {
    /// Signing-proxy base URI, already including the catalog path prefix, e.g.
    /// `http://127.0.0.1:<port>/iceberg`.
    proxy_base: String,
    /// Catalog request prefix from `/v1/config` `overrides.prefix`; empty when
    /// the catalog vends none.
    prefix: String,
}

impl PurgeDropper {
    /// Resolve the catalog request prefix by calling
    /// `/v1/config?warehouse=<warehouse>` through the signing proxy, mirroring
    /// how `iceberg-catalog-rest` bootstraps its own request prefix. The prefix
    /// comes from `defaults.prefix`, with `overrides.prefix` taking precedence
    /// (the same merge the crate applies). For S3 Tables it is the
    /// percent-encoded warehouse ARN and is inserted into request paths verbatim
    /// (already URL-encoded).
    async fn new(proxy_base: &str, warehouse: &str) -> Result<Self> {
        let config_url = format!("{proxy_base}/v1/config");
        let response = reqwest::Client::new()
            .get(&config_url)
            .query(&[("warehouse", warehouse)])
            .send()
            .await
            .with_context(|| format!("failed to GET catalog config at {config_url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read catalog config response body")?;
        if !status.is_success() {
            return Err(anyhow!(
                "catalog config request to {config_url} failed: {status}: {body}"
            ));
        }
        let config: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| format!("catalog config response was not JSON: {body}"))?;
        let prefix_from = |section: &str| {
            config
                .get(section)
                .and_then(|section| section.get("prefix"))
                .and_then(|prefix| prefix.as_str())
                .map(str::to_string)
        };
        let prefix = prefix_from("overrides")
            .or_else(|| prefix_from("defaults"))
            .unwrap_or_default();
        Ok(Self {
            proxy_base: proxy_base.to_string(),
            prefix,
        })
    }

    /// Signed `DELETE .../namespaces/{ns}/tables/{name}?purgeRequested=true`.
    async fn purge_drop(&self, namespace: &NamespaceIdent, name: &str) -> Result<()> {
        let mut segments = vec![self.proxy_base.trim_end_matches('/').to_string()];
        segments.push("v1".to_string());
        if !self.prefix.is_empty() {
            segments.push(self.prefix.clone());
        }
        segments.push("namespaces".to_string());
        segments.push(namespace.to_url_string());
        segments.push("tables".to_string());
        segments.push(name.to_string());
        let url = segments.join("/");
        let response = reqwest::Client::new()
            .delete(&url)
            .query(&[("purgeRequested", "true")])
            .send()
            .await
            .with_context(|| format!("failed to send purge drop DELETE to {url}"))?;
        let status = response.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(anyhow!(
            "purge drop DELETE to {url} failed: {status}: {body}"
        ))
    }
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
    /// Connect to a REST Iceberg catalog. This is the single constructor.
    ///
    /// When `config.sigv4` is set, temporary AWS credentials are resolved from
    /// the ambient credential chain (which reads the SSO/login cache) and an
    /// in-process SigV4 signing proxy is started; the REST catalog is pointed at
    /// the proxy so every catalog request is signed. The ambient credential
    /// provider is also kept on the store so DataFusion object stores for
    /// `s3://` data files can re-resolve credentials on every request and
    /// survive the vend TTL. A one-time snapshot of the credentials is pushed
    /// into the S3 FileIO props (catalog + manifest reads only). When
    /// `config.sigv4` is `None`, the catalog is used unsigned and no S3
    /// credentials are attached.
    pub async fn connect(config: RestCatalogConfig) -> Result<Self> {
        let (
            catalog_uri,
            s3_credentials,
            s3_credentials_provider,
            proxy_guard,
            purge_dropper,
            storage_props,
        ) = match &config.sigv4 {
            Some(sigv4) => {
                let region = sigv4.region.clone();
                let credentials_provider = shared_aws_credentials(&region).await?;
                let resolved = resolve_s3_credentials(&region, &credentials_provider).await?;
                let proxy = SigV4Proxy::start(
                    &config.uri,
                    &region,
                    &sigv4.signing_name,
                    credentials_provider.clone(),
                )
                .await?;
                // Resolve the purge-drop endpoint through the same signing
                // proxy so drops carry `purgeRequested=true` (required by S3
                // Tables).
                let purge_dropper = PurgeDropper::new(proxy.local_uri(), &config.warehouse).await?;
                let props = s3_file_io_props(&resolved);
                (
                    proxy.local_uri().to_string(),
                    Some(resolved),
                    Some(credentials_provider),
                    Some(Arc::new(proxy)),
                    Some(purge_dropper),
                    props,
                )
            }
            None => (config.uri.clone(), None, None, None, None, HashMap::new()),
        };

        let storage_factory: Arc<dyn StorageFactory> = if s3_credentials.is_some() {
            Arc::new(S3ConfiguredStorageFactory {
                props: storage_props.clone(),
                credentials_provider: s3_credentials_provider.clone(),
            })
        } else {
            Arc::new(iceberg::io::LocalFsStorageFactory)
        };

        let catalog = build_rest_catalog(
            &catalog_uri,
            &config.warehouse,
            storage_factory,
            storage_props,
        )
        .await?;

        let store = Self {
            catalog: Arc::new(catalog),
            namespace: NamespaceIdent::new(config.namespace.clone()),
            s3_credentials,
            s3_credentials_provider,
            _sigv4_proxy: proxy_guard,
            purge_dropper,
            data_refresh: Duration::from_secs(config.data_refresh_secs),
            table_contexts: Arc::new(Mutex::new(HashMap::new())),
            partition_indexes: Arc::new(Mutex::new(HashMap::new())),
            custom_partition_indexes: Arc::new(Mutex::new(HashMap::new())),
            table_write_locks: Arc::new(Mutex::new(HashMap::new())),
        };
        store.ensure_tables().await?;
        Ok(store)
    }

    /// The iceberg storage factory matching this store's backend, used when the
    /// DataFusion `IcebergTableProviderFactory` builds a FileIO for a table.
    fn iceberg_storage_factory(&self) -> Arc<dyn StorageFactory> {
        match &self.s3_credentials {
            Some(resolved) => Arc::new(S3ConfiguredStorageFactory {
                props: s3_file_io_props(resolved),
                credentials_provider: self.s3_credentials_provider.clone(),
            }),
            None => Arc::new(iceberg::io::LocalFsStorageFactory),
        }
    }

    /// Register an S3 object store on `ctx` for every distinct `s3://<bucket>`
    /// referenced by `paths`, using a credential provider that re-resolves the
    /// ambient AWS credentials on every request.
    ///
    /// S3 Tables data files live under an AWS-managed bucket whose name is not
    /// the warehouse ARN, so the bucket is parsed from each data-file path
    /// returned by the catalog rather than known ahead of time. A no-op when the
    /// store has no S3 credentials (unsigned catalog).
    ///
    /// The object store is built with a [`RefreshingS3CredentialProvider`]
    /// rather than a static key snapshot: a cached context's object store, and
    /// any object store registered here, must keep working past the vend TTL of
    /// the temporary S3 Tables credentials, which a static snapshot cannot.
    fn register_object_stores_for_paths<'a>(
        &self,
        ctx: &SessionContext,
        paths: impl IntoIterator<Item = &'a String>,
    ) -> Result<()> {
        let (Some(resolved), Some(provider)) =
            (&self.s3_credentials, &self.s3_credentials_provider)
        else {
            return Ok(());
        };
        let credential_provider = Arc::new(RefreshingS3CredentialProvider {
            provider: provider.clone(),
        });
        let mut registered: HashSet<String> = HashSet::new();
        for path in paths {
            let Some(bucket) = s3_bucket_from_path(path) else {
                continue;
            };
            if !registered.insert(bucket.clone()) {
                continue;
            }
            let store = AmazonS3Builder::new()
                .with_bucket_name(&bucket)
                .with_region(&resolved.region)
                .with_credentials(credential_provider.clone())
                .with_virtual_hosted_style_request(true)
                .build()
                .with_context(|| {
                    format!("failed to build S3 object store for bucket '{bucket}'")
                })?;
            let url = Url::parse(&format!("s3://{bucket}"))
                .with_context(|| format!("invalid s3 bucket url for '{bucket}'"))?;
            ctx.register_object_store(&url, Arc::new(store));
        }
        Ok(())
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

    pub async fn reset_table(&self, name: &str) -> Result<()> {
        let ident = self.ident(name);
        if self.catalog.table_exists(&ident).await? {
            // S3 Tables rejects a plain drop ("only supports dropping tables
            // with purge enabled"), and `iceberg-catalog-rest`'s `drop_table`
            // sends no purge flag. On the signed (AWS) path issue a purge-drop
            // through the proxy; on an unsigned catalog the trait drop is fine.
            match &self.purge_dropper {
                Some(dropper) => dropper.purge_drop(&self.namespace, name).await?,
                None => self.catalog.drop_table(&ident).await.with_context(|| {
                    format!(
                        "failed to drop Iceberg table {}.{name}",
                        self.namespace_display()
                    )
                })?,
            }
            // S3 Tables processes the drop asynchronously: the DELETE returns
            // success while the table lingers briefly. If `ensure_tables` runs
            // its `table_exists` check before the drop settles it sees the old
            // table, skips the create, and leaves the stale rows in place. Wait
            // for the table to actually disappear before recreating.
            self.await_table_absent(&ident, name).await?;
        }

        self.invalidate_table_context(name);
        self.ensure_tables().await
    }

    /// Poll the catalog until `ident` no longer exists, so a following
    /// `create_table` starts from a clean slate. S3 Tables drops settle within a
    /// few catalog round-trips.
    async fn await_table_absent(&self, ident: &TableIdent, name: &str) -> Result<()> {
        const MAX_ATTEMPTS: usize = 40;
        for attempt in 0..MAX_ATTEMPTS {
            if !self.catalog.table_exists(ident).await? {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(
                250 * (attempt as u64 + 1).min(4),
            ))
            .await;
        }
        Err(anyhow!(
            "table {}.{name} still exists after purge drop; catalog did not settle",
            self.namespace_display()
        ))
    }

    pub async fn ensure_tables(&self) -> Result<()> {
        if !self.catalog.namespace_exists(&self.namespace).await? {
            if let Err(error) = self
                .catalog
                .create_namespace(&self.namespace, HashMap::new())
                .await
            {
                // Two concurrent connects race between the existence check and
                // the create; the catalog enforces uniqueness so the loser sees
                // "already exists". Losing that race is success as long as the
                // namespace now exists.
                if !self.catalog.namespace_exists(&self.namespace).await? {
                    return Err(error).context("failed to create lean Iceberg namespace");
                }
            }
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
        self.ensure_table(CUSTOM_POINTS, custom_schema(), &["provider", "feed"])
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
        // The REST catalog (S3 Tables) owns the warehouse and assigns each
        // table's storage location itself, so no client-chosen location is set.
        let creation = TableCreation::builder()
            .name(name.into())
            .schema(schema)
            .partition_spec(spec.into_unbound())
            .build();
        if let Err(error) = self.catalog.create_table(&self.namespace, creation).await {
            // Concurrent connects race between the existence check and the
            // create (the catalog enforces uniqueness); losing that race is
            // success as long as the table now exists.
            if self.catalog.table_exists(&ident).await.unwrap_or(false) {
                return Ok(());
            }
            return Err(error).with_context(|| {
                format!(
                    "failed to create Iceberg table {}.{name}",
                    self.namespace_display()
                )
            });
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
        // Route through `market_partition_index` so the staleness recheck runs;
        // it returns the cached index cheaply inside the TTL window and rebuilds
        // when another process has committed.
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
        out.sort_by_key(|point| point.end_time.0);
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
        // Rows partition by their emission gate (`end_time`), and `custom_point_key`
        // keys on it too, so the dedupe scan window is over end_time dates.
        if let (Some(start), Some(end)) = (
            points.iter().map(|point| point.end_time.date_utc()).min(),
            points.iter().map(|point| point.end_time.date_utc()).max(),
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

    /// Whether a cached entry last checked at `last_checked` is still inside the
    /// staleness recheck window and may be trusted without a catalog round-trip.
    /// A zero `data_refresh` always forces a recheck.
    fn within_refresh_window(&self, last_checked: Instant) -> bool {
        !self.data_refresh.is_zero() && last_checked.elapsed() < self.data_refresh
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
            // Within the TTL window the cached snapshot is trusted outright.
            // Copy the timestamp out and drop the guard before any `.await` so
            // the future stays `Send`.
            let last_checked = *cached
                .last_checked
                .lock()
                .expect("iceberg table context timestamp poisoned");
            if self.within_refresh_window(last_checked) {
                return Ok(cached.ctx.table(table).await?);
            }
            // TTL elapsed: recheck the catalog. Unchanged metadata location means
            // no other process committed, so bump the timestamp and reuse the
            // cached context; a changed location falls through to a rebuild.
            let catalog_table = self.catalog.load_table(&self.ident(table)).await?;
            let metadata_location = catalog_table
                .metadata_location_result()
                .map_err(|err| anyhow!(err))?
                .to_string();
            if metadata_location == cached.metadata_location {
                *cached
                    .last_checked
                    .lock()
                    .expect("iceberg table context timestamp poisoned") = Instant::now();
                return Ok(cached.ctx.table(table).await?);
            }
            return self
                .build_and_cache_table_context(table, metadata_location)
                .await;
        }

        let catalog_table = self.catalog.load_table(&self.ident(table)).await?;
        let metadata_location = catalog_table
            .metadata_location_result()
            .map_err(|err| anyhow!(err))?
            .to_string();
        self.build_and_cache_table_context(table, metadata_location)
            .await
    }

    /// Build a fresh DataFusion external table at `metadata_location`, cache it
    /// keyed by `table`, and return a `DataFrame` for it. Called on a cold cache
    /// and when a staleness recheck finds the catalog moved to a new snapshot.
    async fn build_and_cache_table_context(
        &self,
        table: &str,
        metadata_location: String,
    ) -> Result<DataFrame> {
        let mut state = SessionStateBuilder::new().with_default_features().build();
        state.table_factories_mut().insert(
            "ICEBERG".to_string(),
            Arc::new(IcebergTableProviderFactory::new_with_storage_factory(
                self.iceberg_storage_factory(),
            )),
        );
        let ctx = SessionContext::new_with_state(state);
        // S3 Tables keeps metadata and data files in the same AWS-managed
        // bucket, so registering the object store for the metadata location's
        // bucket lets DataFusion resolve the `s3://` data-file paths the Iceberg
        // provider hands it during the scan.
        self.register_object_stores_for_paths(&ctx, std::iter::once(&metadata_location))?;
        let sql = format!(
            "CREATE EXTERNAL TABLE {table} STORED AS ICEBERG LOCATION '{}'",
            metadata_location.replace('\'', "''")
        );
        ctx.sql(&sql).await?.collect().await?;
        let cached = Arc::new(IcebergTableContext {
            ctx,
            metadata_location,
            last_checked: Mutex::new(Instant::now()),
        });
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
            .map(|path| warehouse_file_path(path))
            .collect::<Result<Vec<_>>>()?;
        let ctx = SessionContext::new_with_config(SessionConfig::new().with_target_partitions(1));
        // Register an S3 object store for each distinct bucket in the pruned
        // file list before reading; S3 Tables data files live under an
        // AWS-managed bucket whose name is discovered from these paths.
        self.register_object_stores_for_paths(&ctx, paths.iter())?;
        Ok(ctx
            .read_parquet(paths, ParquetReadOptions::default())
            .await?)
    }

    async fn market_partition_index(&self, table: &str) -> Result<Arc<MarketPartitionIndex>> {
        let cached = {
            let indexes = self
                .partition_indexes
                .lock()
                .expect("iceberg partition index cache poisoned");
            indexes.get(table).map(|entry| {
                (
                    entry.index.clone(),
                    entry.metadata_location.clone(),
                    entry.last_checked,
                )
            })
        };
        if let Some((index, metadata_location, last_checked)) = cached {
            // Trust the cached index inside the TTL window.
            if self.within_refresh_window(last_checked) {
                return Ok(index);
            }
            // TTL elapsed: recheck the catalog. An unchanged metadata location
            // means the index is still current, so bump the timestamp and reuse
            // it; a moved snapshot forces a full rebuild below.
            let catalog_table = self.catalog.load_table(&self.ident(table)).await?;
            let current_location = catalog_table
                .metadata_location_result()
                .map_err(|err| anyhow!(err))?
                .to_string();
            if current_location == metadata_location {
                let mut indexes = self
                    .partition_indexes
                    .lock()
                    .expect("iceberg partition index cache poisoned");
                if let Some(entry) = indexes.get_mut(table) {
                    entry.last_checked = Instant::now();
                }
                return Ok(index);
            }
            return self
                .build_and_cache_market_index(table, catalog_table, current_location)
                .await;
        }

        let catalog_table = self.catalog.load_table(&self.ident(table)).await?;
        let metadata_location = catalog_table
            .metadata_location_result()
            .map_err(|err| anyhow!(err))?
            .to_string();
        self.build_and_cache_market_index(table, catalog_table, metadata_location)
            .await
    }

    /// Fully rebuild the market partition index for `table` from the loaded
    /// catalog snapshot and cache it with `metadata_location`. Called on a cold
    /// cache and when a staleness recheck finds the snapshot moved. A full
    /// rebuild (re-planning every file) keeps correctness simple; see the PR
    /// note on an incremental follow-up for the large market tables.
    async fn build_and_cache_market_index(
        &self,
        table: &str,
        catalog_table: iceberg::table::Table,
        metadata_location: String,
    ) -> Result<Arc<MarketPartitionIndex>> {
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
            .insert(
                table.to_string(),
                CachedMarketIndex {
                    index: index.clone(),
                    metadata_location,
                    last_checked: Instant::now(),
                },
            );
        Ok(index)
    }

    async fn custom_partition_index(&self) -> Result<Arc<CustomPartitionIndex>> {
        let cached = {
            let indexes = self
                .custom_partition_indexes
                .lock()
                .expect("iceberg custom partition index cache poisoned");
            indexes.get(CUSTOM_POINTS).map(|entry| {
                (
                    entry.index.clone(),
                    entry.metadata_location.clone(),
                    entry.last_checked,
                )
            })
        };
        if let Some((index, metadata_location, last_checked)) = cached {
            if self.within_refresh_window(last_checked) {
                return Ok(index);
            }
            let catalog_table = self.catalog.load_table(&self.ident(CUSTOM_POINTS)).await?;
            let current_location = catalog_table
                .metadata_location_result()
                .map_err(|err| anyhow!(err))?
                .to_string();
            if current_location == metadata_location {
                let mut indexes = self
                    .custom_partition_indexes
                    .lock()
                    .expect("iceberg custom partition index cache poisoned");
                if let Some(entry) = indexes.get_mut(CUSTOM_POINTS) {
                    entry.last_checked = Instant::now();
                }
                return Ok(index);
            }
            return self
                .build_and_cache_custom_index(catalog_table, current_location)
                .await;
        }

        let catalog_table = self.catalog.load_table(&self.ident(CUSTOM_POINTS)).await?;
        let metadata_location = catalog_table
            .metadata_location_result()
            .map_err(|err| anyhow!(err))?
            .to_string();
        self.build_and_cache_custom_index(catalog_table, metadata_location)
            .await
    }

    /// Fully rebuild the custom-points partition index from the loaded catalog
    /// snapshot and cache it with `metadata_location`.
    async fn build_and_cache_custom_index(
        &self,
        catalog_table: iceberg::table::Table,
        metadata_location: String,
    ) -> Result<Arc<CustomPartitionIndex>> {
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
            .insert(
                CUSTOM_POINTS.to_string(),
                CachedCustomIndex {
                    index: index.clone(),
                    metadata_location,
                    last_checked: Instant::now(),
                },
            );
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
                .filter(col("provider").eq(lit(source_type.clone())))?
                .filter(col("feed").eq(lit(ticker.clone())))?
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
        let metadata_location = table_ref
            .metadata_location_result()
            .map_err(|err| anyhow!(err))?
            .to_string();
        let metadata = table_ref.metadata();
        let snapshot_id = metadata.current_snapshot_id();
        let spec = metadata.default_partition_spec().as_ref().clone();
        let mut indexes = self
            .partition_indexes
            .lock()
            .expect("iceberg partition index cache poisoned");
        if let Some(existing) = indexes.get(table) {
            let mut updated = existing.index.as_ref().clone();
            updated.snapshot_id = snapshot_id;
            for data_file in data_files {
                updated.insert_data_file(data_file, &spec)?;
            }
            indexes.insert(
                table.to_string(),
                CachedMarketIndex {
                    index: Arc::new(updated),
                    metadata_location,
                    last_checked: Instant::now(),
                },
            );
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
        let metadata_location = table_ref
            .metadata_location_result()
            .map_err(|err| anyhow!(err))?
            .to_string();
        let metadata = table_ref.metadata();
        let snapshot_id = metadata.current_snapshot_id();
        let spec = metadata.default_partition_spec().as_ref().clone();
        let mut indexes = self
            .custom_partition_indexes
            .lock()
            .expect("iceberg custom partition index cache poisoned");
        if let Some(existing) = indexes.get(table) {
            let mut updated = existing.index.as_ref().clone();
            updated.snapshot_id = snapshot_id;
            for data_file in data_files {
                updated.insert_data_file(data_file, &spec)?;
            }
            indexes.insert(
                table.to_string(),
                CachedCustomIndex {
                    index: Arc::new(updated),
                    metadata_location,
                    last_checked: Instant::now(),
                },
            );
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

    /// The configured namespace rendered for diagnostics, e.g. `lean`.
    fn namespace_display(&self) -> String {
        self.namespace.as_ref().join(".")
    }
}

/// Translate an Iceberg data-file path into a path DataFusion's `read_parquet`
/// can open. For the REST/S3 warehouse this is an `s3://<bucket>/<key>` URL,
/// returned unchanged (an S3 object store must be registered on the reading
/// context for its bucket, via
/// [`IcebergStore::register_object_stores_for_paths`]). Any other scheme is an
/// error — the store speaks only to an S3-backed REST catalog.
fn warehouse_file_path(iceberg_path: &str) -> Result<String> {
    if iceberg_path.starts_with("s3://") || iceberg_path.starts_with("s3a://") {
        return Ok(iceberg_path.to_string());
    }
    Err(anyhow!(
        "expected an s3:// Iceberg data-file path, got: {iceberg_path}"
    ))
}

/// The bucket portion of an `s3://<bucket>/<key>` (or `s3a://`) path, if any.
fn s3_bucket_from_path(path: &str) -> Option<String> {
    let rest = path
        .strip_prefix("s3://")
        .or_else(|| path.strip_prefix("s3a://"))?;
    let bucket = rest.split('/').next().unwrap_or("");
    if bucket.is_empty() {
        None
    } else {
        Some(bucket.to_string())
    }
}

/// Resolve a shared AWS credentials provider from the ambient credential chain,
/// scoped to `region`. The `DefaultCredentialsChain` reads the SSO/login cache,
/// which `object_store`'s native providers do not.
///
/// The chain is wrapped in [`CachedChainCredentials`]: `DefaultCredentialsChain`
/// has no internal cache, so a bare chain re-runs full resolution — including
/// spawning any `credential_process` — on every `provide_credentials` call.
/// Every consumer of this provider (the SigV4 proxy, the DataFusion object
/// stores, the OpenDAL FileIO loader) resolves per request, so without the
/// cache a data-heavy backtest spawns hundreds of concurrent subprocesses and
/// eventually dies with "an error occurred while loading credentials".
async fn shared_aws_credentials(region: &str) -> Result<SharedCredentialsProvider> {
    let chain = DefaultCredentialsChain::builder()
        .region(Region::new(region.to_string()))
        .build()
        .await;
    Ok(SharedCredentialsProvider::new(CachedChainCredentials::new(
        SharedCredentialsProvider::new(chain),
    )))
}

/// How long before a credential's stated expiry the cache starts trying to
/// replace it.
const CREDENTIAL_REFRESH_BUFFER: Duration = Duration::from_secs(60);

/// Minimum spacing between chain hits while inside the refresh buffer, so a
/// credential source that keeps returning the same about-to-expire credential
/// (the AWS CLI serves its cached session until hard expiry) is not hammered
/// once per S3 request.
const CREDENTIAL_RETRY_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// Expiry-aware, single-flight cache over the ambient AWS credential chain.
///
/// Serves the cached credential until it is within
/// [`CREDENTIAL_REFRESH_BUFFER`] of its expiry, then re-resolves through the
/// chain (at most once per [`CREDENTIAL_RETRY_MIN_INTERVAL`]). The `Mutex`
/// makes refreshes single-flight: concurrent callers wait for one resolution
/// instead of each spawning their own `credential_process`. Credentials with
/// no expiry (static keys) are resolved once and reused forever. If a refresh
/// fails while the cached credential is still valid, the cached credential
/// keeps being served.
#[derive(Debug)]
struct CachedChainCredentials {
    chain: SharedCredentialsProvider,
    refresh_buffer: Duration,
    retry_min_interval: Duration,
    state: tokio::sync::Mutex<CachedCredentialState>,
}

#[derive(Debug, Default)]
struct CachedCredentialState {
    credentials: Option<aws_credential_types::Credentials>,
    /// When the chain was last hit, successfully or not. Throttles refresh
    /// attempts inside the buffer window.
    last_resolved: Option<Instant>,
}

impl CachedChainCredentials {
    fn new(chain: SharedCredentialsProvider) -> Self {
        Self::with_intervals(
            chain,
            CREDENTIAL_REFRESH_BUFFER,
            CREDENTIAL_RETRY_MIN_INTERVAL,
        )
    }

    fn with_intervals(
        chain: SharedCredentialsProvider,
        refresh_buffer: Duration,
        retry_min_interval: Duration,
    ) -> Self {
        Self {
            chain,
            refresh_buffer,
            retry_min_interval,
            state: tokio::sync::Mutex::new(CachedCredentialState::default()),
        }
    }

    async fn resolve(&self) -> aws_credential_types::provider::Result {
        let mut state = self.state.lock().await;
        if let Some(credentials) = &state.credentials {
            let now = std::time::SystemTime::now();
            let fresh = match credentials.expiry() {
                // Static keys never expire.
                None => true,
                Some(expiry) => now + self.refresh_buffer < expiry,
            };
            // The throttle may only serve credentials that are still hard-valid;
            // once the actual expiry passes, every caller goes to the chain.
            let hard_valid = credentials.expiry().is_none_or(|expiry| now < expiry);
            let recently_tried = state
                .last_resolved
                .is_some_and(|at| at.elapsed() < self.retry_min_interval);
            if fresh || (recently_tried && hard_valid) {
                return Ok(credentials.clone());
            }
        }
        let attempt = self.chain.provide_credentials().await;
        state.last_resolved = Some(Instant::now());
        match attempt {
            Ok(credentials) => {
                state.credentials = Some(credentials.clone());
                Ok(credentials)
            }
            Err(error) => {
                // Serve the cached credential through transient chain failures
                // as long as it has not actually expired.
                if let Some(credentials) = &state.credentials {
                    let still_valid = credentials
                        .expiry()
                        .is_none_or(|expiry| std::time::SystemTime::now() < expiry);
                    if still_valid {
                        return Ok(credentials.clone());
                    }
                }
                Err(error)
            }
        }
    }
}

impl ProvideCredentials for CachedChainCredentials {
    fn provide_credentials<'a>(
        &'a self,
    ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        aws_credential_types::provider::future::ProvideCredentials::new(self.resolve())
    }
}

/// Resolve the concrete temporary credentials once, to feed explicitly into the
/// S3 FileIO props (catalog + manifest reads). The DataFusion object stores use
/// [`RefreshingS3CredentialProvider`] instead of this snapshot. Credential
/// material is never logged.
async fn resolve_s3_credentials(
    region: &str,
    provider: &SharedCredentialsProvider,
) -> Result<ResolvedS3Credentials> {
    let credentials = provider
        .provide_credentials()
        .await
        .context("failed to resolve AWS credentials from the default chain")?;
    Ok(ResolvedS3Credentials {
        region: region.to_string(),
        access_key_id: credentials.access_key_id().to_string(),
        secret_access_key: credentials.secret_access_key().to_string(),
        session_token: credentials.session_token().map(|token| token.to_string()),
    })
}

/// Load the Iceberg REST catalog, merging `storage_props` into the catalog
/// `load()` props so they reach the FileIO the catalog builds for every table
/// (`iceberg-catalog-rest` 0.9.1 forwards catalog props into
/// `FileIOBuilder::with_props`). The initial `namespace_exists` probe in
/// [`IcebergStore::ensure_tables`] surfaces an unreachable catalog as a hard
/// error.
async fn build_rest_catalog(
    uri: &str,
    warehouse: &str,
    storage_factory: Arc<dyn StorageFactory>,
    storage_props: HashMap<String, String>,
) -> Result<impl Catalog> {
    let mut props = storage_props;
    props.insert(REST_CATALOG_PROP_URI.to_string(), uri.to_string());
    props.insert(
        REST_CATALOG_PROP_WAREHOUSE.to_string(),
        warehouse.to_string(),
    );
    RestCatalogBuilder::default()
        .with_storage_factory(storage_factory)
        .load(CATALOG_NAME, props)
        .await
        .with_context(|| {
            format!("failed to load Iceberg REST catalog at {uri} (warehouse '{warehouse}')")
        })
}

/// Storage factory that carries the S3 connection properties itself.
///
/// `iceberg-catalog-rest` 0.9.1 builds its `FileIO` from the catalog props plus
/// whatever storage config the catalog vends. This wrapper additionally merges
/// the configured S3 props into whatever config it is handed before delegating
/// to the OpenDAL S3 factory, so the resolved region/credentials and
/// `s3.path-style-access=false` are always present even if the vended config
/// omits or differs on them. The same applies to the DataFusion
/// `IcebergTableProviderFactory` FileIO path.
///
/// The FileIO built here reads Iceberg metadata and manifests from S3 whenever
/// a table is (re)loaded — including mid-run rebuilds after another process
/// commits — so it must survive the vend TTL of temporary credentials just
/// like the data-plane object stores. When `credentials_provider` is set, the
/// OpenDAL S3 backend loads credentials through it (reqsign re-invokes the
/// loader shortly before the current credential expires) instead of pinning
/// the connect-time keys from `props`. The provider is `#[serde(skip)]`
/// because live credential chains cannot be serialized; a deserialized factory
/// falls back to the static props.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct S3ConfiguredStorageFactory {
    props: HashMap<String, String>,
    #[serde(skip)]
    credentials_provider: Option<SharedCredentialsProvider>,
}

#[typetag::serde]
impl StorageFactory for S3ConfiguredStorageFactory {
    fn build(&self, config: &StorageConfig) -> iceberg::Result<Arc<dyn Storage>> {
        let mut props = self.props.clone();
        props.extend(config.props().clone());
        let inner = OpenDalStorageFactory::S3 {
            configured_scheme: "s3".to_string(),
            customized_credential_load: self.credentials_provider.clone().map(|provider| {
                iceberg_storage_opendal::CustomAwsCredentialLoader::new(Arc::new(
                    RefreshingOpendalCredentialLoader { provider },
                ))
            }),
        };
        inner.build(&StorageConfig::from_props(props))
    }
}

/// [`iceberg_storage_opendal::AwsCredentialLoad`] implementation over the
/// ambient AWS credential chain, mirroring [`RefreshingS3CredentialProvider`]
/// for the OpenDAL FileIO path. `expires_in` is forwarded from the resolved
/// credential so reqsign reloads shortly before expiry.
#[derive(Debug)]
struct RefreshingOpendalCredentialLoader {
    provider: SharedCredentialsProvider,
}

#[async_trait::async_trait]
impl iceberg_storage_opendal::AwsCredentialLoad for RefreshingOpendalCredentialLoader {
    async fn load_credential(
        &self,
        _client: reqwest::Client,
    ) -> anyhow::Result<Option<iceberg_storage_opendal::AwsCredential>> {
        let credentials = self
            .provider
            .provide_credentials()
            .await
            .context("failed to resolve AWS credentials for the S3 FileIO")?;
        Ok(Some(iceberg_storage_opendal::AwsCredential {
            access_key_id: credentials.access_key_id().to_string(),
            secret_access_key: credentials.secret_access_key().to_string(),
            session_token: credentials.session_token().map(str::to_string),
            expires_in: credentials
                .expiry()
                .map(chrono::DateTime::<chrono::Utc>::from),
        }))
    }
}

/// The S3 FileIO property map the OpenDAL storage factory reads from the
/// catalog's props. Uses the resolved (temporary) credentials; virtual-host
/// addressing (path-style FALSE) is required for AWS S3 Tables.
fn s3_file_io_props(resolved: &ResolvedS3Credentials) -> HashMap<String, String> {
    let mut props = HashMap::new();
    props.insert("s3.region".to_string(), resolved.region.clone());
    props.insert(
        "s3.access-key-id".to_string(),
        resolved.access_key_id.clone(),
    );
    props.insert(
        "s3.secret-access-key".to_string(),
        resolved.secret_access_key.clone(),
    );
    if let Some(token) = &resolved.session_token {
        props.insert("s3.session-token".to_string(), token.clone());
    }
    props.insert("s3.path-style-access".to_string(), "false".to_string());
    props
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
            ("provider", PrimitiveType::String, false),
            ("feed", PrimitiveType::String, false),
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
    provider: &str,
    feed: &str,
    points: &[CustomDataPoint],
) -> Result<RecordBatch> {
    let time_ns: Vec<i64> = points.iter().map(|p| p.time.0).collect();
    let end_time_ns: Vec<i64> = points.iter().map(|p| p.end_time.0).collect();
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
    // The `day` partition tracks the emission gate (`end_time_ns`): window scans
    // prune on it, and the engine only surfaces a point once the frontier reaches
    // its end_time, so a point must be reachable in the day partition of the date
    // it becomes visible.
    let day: Vec<i32> = end_time_ns.iter().map(|ns| days_since_epoch(*ns)).collect();
    let rows = points.len();
    let arrow_schema = Arc::new(ArrowSchema::new(vec![
        Field::new("time_ns", DataType::Int64, false),
        Field::new("end_time_ns", DataType::Int64, false),
        Field::new("value", DataType::Float64, false),
        Field::new("fields_json", DataType::Utf8, false),
        Field::new("symbol", DataType::Utf8, true),
        Field::new("provider", DataType::Utf8, false),
        Field::new("feed", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
    ]));
    Ok(RecordBatch::try_new(
        arrow_schema,
        vec![
            Arc::new(Int64Array::from(time_ns)),
            Arc::new(Int64Array::from(end_time_ns)),
            Arc::new(Float64Array::from(value)),
            Arc::new(StringArray::from(fields_json)),
            Arc::new(StringArray::from(symbol)),
            Arc::new(StringArray::from(vec![provider.to_lowercase(); rows])),
            Arc::new(StringArray::from(vec![feed.to_lowercase(); rows])),
            Arc::new(arrow_array::Date32Array::from(day)),
        ],
    )?)
}

fn custom_point_key(point: &CustomDataPoint) -> (i64, String, String) {
    let time = point.end_time.0;
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

fn with_custom_partitions(batch: RecordBatch, provider: &str, feed: &str) -> Result<RecordBatch> {
    let rows = batch.num_rows();
    let day_values = batch
        .column_by_name("end_time_ns")
        .ok_or_else(|| anyhow!("end_time_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("end_time_ns must be int64"))?;
    let day: Vec<i32> = (0..rows)
        .map(|row| days_since_epoch(day_values.value(row)))
        .collect();
    append_columns(
        batch,
        &[
            Field::new("provider", DataType::Utf8, false),
            Field::new("feed", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
        ],
        vec![
            Arc::new(StringArray::from(vec![provider.to_lowercase(); rows])),
            Arc::new(StringArray::from(vec![feed.to_lowercase(); rows])),
            Arc::new(arrow_array::Date32Array::from(day)),
        ],
    )
}

fn append_custom_batch_points(
    batch: &RecordBatch,
    out: &mut Vec<CustomDataPoint>,
    projected_fields: Option<&HashSet<String>>,
) -> Result<()> {
    let time_ns = batch
        .column_by_name("time_ns")
        .ok_or_else(|| anyhow!("time_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("time_ns must be int64"))?;
    let end_time_ns = batch
        .column_by_name("end_time_ns")
        .ok_or_else(|| anyhow!("end_time_ns column missing"))?
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| anyhow!("end_time_ns must be int64"))?;
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
        let time_ns = time_ns.value(row);
        let end_time_ns = end_time_ns.value(row);
        if let Some(projected_fields) = projected_fields {
            fields.retain(|key, _| projected_fields.contains(key));
        }
        let symbol = symbol_column
            .and_then(|column| optional_string_at(column, row))
            .map(|value| value.trim().to_ascii_uppercase())
            .filter(|value| !value.is_empty());
        out.push(
            CustomDataPoint::new(
                lean_core::NanosecondTimestamp(time_ns),
                lean_core::NanosecondTimestamp(end_time_ns),
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
            Field::new("time_ns", DataType::Int64, false),
            Field::new("end_time_ns", DataType::Int64, false),
            Field::new("value", DataType::Float64, false),
            Field::new("fields_json", DataType::Utf8, false),
        ]));
        let time_ns = schema::date_to_ns(chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap());
        let end_time_ns = time_ns + 86_400 * 1_000_000_000;
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(vec![time_ns])),
                Arc::new(Int64Array::from(vec![end_time_ns])),
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

    /// A credential source whose keys rotate on every resolve, standing in for
    /// the ambient AWS chain vending a fresh temporary credential after the
    /// previous one expired. `ttl` sets each vended credential's expiry
    /// relative to now (`None` => never expires).
    #[derive(Debug)]
    struct RotatingCredentials {
        next: std::sync::atomic::AtomicUsize,
        ttl: Option<std::time::Duration>,
    }

    impl RotatingCredentials {
        fn shared(ttl: Option<std::time::Duration>) -> SharedCredentialsProvider {
            SharedCredentialsProvider::new(Self {
                next: std::sync::atomic::AtomicUsize::new(0),
                ttl,
            })
        }
    }

    impl aws_credential_types::provider::ProvideCredentials for RotatingCredentials {
        fn provide_credentials<'a>(
            &'a self,
        ) -> aws_credential_types::provider::future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            let seq = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            aws_credential_types::provider::future::ProvideCredentials::ready(Ok(
                aws_credential_types::Credentials::new(
                    format!("AKID{seq}"),
                    format!("SECRET{seq}"),
                    Some(format!("TOKEN{seq}")),
                    self.ttl.map(|ttl| std::time::SystemTime::now() + ttl),
                    "rotating-test",
                ),
            ))
        }
    }

    #[tokio::test]
    async fn refreshing_provider_reresolves_on_every_call() {
        let provider = RefreshingS3CredentialProvider {
            provider: RotatingCredentials::shared(None),
        };

        let first = provider.get_credential().await.expect("first credential");
        assert_eq!(first.key_id, "AKID0");
        assert_eq!(first.secret_key, "SECRET0");
        assert_eq!(first.token.as_deref(), Some("TOKEN0"));

        // A second call must re-resolve through the given provider rather than
        // return the first snapshot, so an expired credential is replaced by a
        // freshly vended one for the next S3 request.
        let second = provider.get_credential().await.expect("second credential");
        assert_eq!(second.key_id, "AKID1");
        assert_eq!(second.secret_key, "SECRET1");
        assert_eq!(second.token.as_deref(), Some("TOKEN1"));
    }

    #[tokio::test]
    async fn cached_chain_serves_unexpired_credentials_without_rehitting_the_chain() {
        // Credentials valid for an hour: every resolve inside the refresh
        // buffer must come from the cache, not spawn another chain resolution.
        let cached = CachedChainCredentials::new(RotatingCredentials::shared(Some(
            std::time::Duration::from_secs(3600),
        )));

        let first = cached.resolve().await.expect("first credential");
        let second = cached.resolve().await.expect("second credential");
        assert_eq!(first.access_key_id(), "AKID0");
        assert_eq!(second.access_key_id(), "AKID0");
    }

    #[tokio::test]
    async fn cached_chain_refreshes_credentials_inside_the_expiry_buffer() {
        // Credentials that are already inside the refresh buffer (1s TTL vs
        // 60s buffer) must be replaced on the next resolve. A zero retry
        // interval disables the throttle so the test does not sleep.
        let cached = CachedChainCredentials::with_intervals(
            RotatingCredentials::shared(Some(std::time::Duration::from_secs(1))),
            CREDENTIAL_REFRESH_BUFFER,
            Duration::ZERO,
        );

        let first = cached.resolve().await.expect("first credential");
        let second = cached.resolve().await.expect("second credential");
        assert_eq!(first.access_key_id(), "AKID0");
        assert_eq!(second.access_key_id(), "AKID1");
    }

    #[tokio::test]
    async fn cached_chain_never_expiring_credentials_are_resolved_once() {
        let cached = CachedChainCredentials::with_intervals(
            RotatingCredentials::shared(None),
            CREDENTIAL_REFRESH_BUFFER,
            Duration::ZERO,
        );

        let first = cached.resolve().await.expect("first credential");
        let second = cached.resolve().await.expect("second credential");
        assert_eq!(first.access_key_id(), "AKID0");
        assert_eq!(second.access_key_id(), "AKID0");
    }
}
