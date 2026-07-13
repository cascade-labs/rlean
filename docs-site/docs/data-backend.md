---
sidebar_position: 3
title: Data Backend
---

# Data Backend

All market data is Parquet — there is no CSV anywhere. The `lean-storage` crate
owns all persisted data I/O through a single type, `IcebergStore`. Data
providers only return rows to the engine; they do not write files or query local
storage.

## REST Iceberg catalog (AWS S3 Tables)

rlean reads and writes all market data through a REST Iceberg catalog. There is
no local or filesystem data store — a run cannot start without a catalog URI and
warehouse. In production the catalog is AWS S3 Tables.

### Configuration

Set these with `rlean config set <key> <value>`. Environment variables can
override a setting for one process. The catalog and all four data-S3 values are
required: without them rlean has no market-data caching mode.

| Config key          | Env var                   | CLI flag              | Required | Meaning |
|---------------------|---------------------------|-----------------------|----------|---------|
| `data_catalog`      | `RLEAN_DATA_CATALOG`      | `--data-catalog`      | yes      | REST catalog base URI |
| `data_warehouse`    | `RLEAN_DATA_WAREHOUSE`    | `--data-warehouse`    | yes      | Warehouse id / S3 Tables table-bucket ARN |
| `data_sigv4_region` | `RLEAN_DATA_SIGV4_REGION` | `--data-sigv4-region` | no       | SigV4 signing region (turns on signing) |
| `data_sigv4_name`   | `RLEAN_DATA_SIGV4_NAME`   | `--data-sigv4-name`   | no       | SigV4 signing name (default `s3tables`) |
| `data_namespace`    | `RLEAN_DATA_NAMESPACE`    | `--data-namespace`    | no       | Iceberg namespace (default `lean`) |
| `data_s3_endpoint`  | `RLEAN_DATA_S3_ENDPOINT`  | —                     | yes      | S3-compatible endpoint for all Iceberg metadata, manifests, and Parquet files |
| `data_s3_region`    | `RLEAN_DATA_S3_REGION`    | —                     | yes      | Region expected when signing requests to the data endpoint |
| `data_s3_access_key_id` | `RLEAN_DATA_S3_ACCESS_KEY_ID` | —                 | yes      | Access key issued by the data endpoint |
| `data_s3_secret_access_key` | `RLEAN_DATA_S3_SECRET_ACCESS_KEY` | —             | yes      | Secret issued by the data endpoint |

When `data_sigv4_region` is unset the catalog is used unsigned (plain / OAuth
REST catalog, e.g. a local mock). When it is set, catalog requests are signed
with SigV4 and `data_sigv4_name` defaults to `s3tables`.

Working AWS S3 Tables values:

```
data_catalog=https://s3tables.us-west-2.amazonaws.com/iceberg
data_warehouse=arn:aws:s3tables:us-west-2:<acct>:bucket/<name>
data_sigv4_region=us-west-2
data_sigv4_name=s3tables
```

### AWS credentials

Credentials for the AWS catalog come from the ambient AWS credential chain,
resolved in-process at connect time (`aws-config`). This includes AWS SSO / `aws
sso login` sessions. They are used only for catalog traffic. Because
`iceberg-catalog-rest` has no per-request signing hook, rlean starts an
in-process localhost SigV4 proxy that signs each catalog request and forwards
it to the real endpoint.

### Maintenance and compaction

Compaction and snapshot expiry are **AWS-managed** (S3 Tables managed
maintenance); rlean does not run end-of-run compaction. The
`iceberg_maintenance` binary in `rlean-storage` can `report`, `count`, `query`,
and `reset` tables. It requires the same catalog and four `RLEAN_DATA_S3_*`
environment values (with `RLEAN_DATA_NAMESPACE` defaulting to `lean`).

## Tables

`IcebergStore` maintains the following tables in the configured namespace. Each
row below lists the table name and its Iceberg partition columns, exactly as
created in `ensure_tables`.

| Table | Partition columns |
|---|---|
| `market_trade_bars` | `security_type`, `market`, `resolution`, `symbol_sid`, `day` |
| `market_quote_bars` | `security_type`, `market`, `resolution`, `symbol_sid`, `day` |
| `market_ticks` | `security_type`, `market`, `resolution`, `symbol_sid`, `day` |
| `option_eod_bars` | `day` |
| `option_universe` | `day` |
| `margin_interest` | `security_type`, `market`, `day` |
| `perpetual_context` | `security_type`, `market`, `day` |
| `custom_points` | `provider`, `feed` |
| `factor_files` | `market`, `ticker` |
| `map_files` | `market`, `permtick` |

## Column schemas

Prices are stored as `i64` scaled by `PRICE_SCALE` (1e8) to preserve eight
decimal places. Timestamps are `i64` nanoseconds since the Unix epoch (UTC).

### `market_trade_bars`

| Column | Type |
|---|---|
| `time_ns` | int64 |
| `end_time_ns` | int64 |
| `symbol_sid` | int64 |
| `symbol_value` | utf8 |
| `open` `high` `low` `close` | int64 (×1e8) |
| `volume` | int64 |
| `period_ns` | int64 |

### `market_quote_bars`

Trade-bar identity columns (`time_ns`, `end_time_ns`, `symbol_sid`,
`symbol_value`) plus:

| Column | Type |
|---|---|
| `bid_open` `bid_high` `bid_low` `bid_close` | int64 (×1e8, nullable) |
| `ask_open` `ask_high` `ask_low` `ask_close` | int64 (×1e8, nullable) |
| `last_bid_size` `last_ask_size` | int64 |
| `period_ns` | int64 |

### `market_ticks`

| Column | Type |
|---|---|
| `time_ns` | int64 |
| `symbol_sid` | int64 |
| `symbol_value` | utf8 |
| `tick_type` | uint8 |
| `value` `quantity` | int64 |
| `bid_price` `ask_price` `bid_size` `ask_size` | int64 |
| `exchange` `sale_condition` | utf8 (nullable) |
| `suspicious` | bool |

### `option_eod_bars`

| Column | Type |
|---|---|
| `date_ns` | int64 |
| `symbol_value` | utf8 (full OSI ticker) |
| `underlying` | utf8 |
| `expiration_ns` | int64 |
| `strike` | int64 (×1e8) |
| `right` | utf8 (`"C"` or `"P"`) |
| `open` `high` `low` `close` `bid` `ask` | int64 (×1e8) |
| `volume` | int64 (raw contracts) |
| `bid_size` `ask_size` | int64 (raw contracts) |

### `option_universe`

| Column | Type |
|---|---|
| `date_ns` | int64 |
| `symbol_value` | utf8 (full OSI ticker) |
| `underlying` | utf8 |
| `expiration_ns` | int64 |
| `strike` | int64 (×1e8) |
| `right` | utf8 (`"C"` or `"P"`) |

### `custom_points`

| Column | Type |
|---|---|
| `time_ns` | int64 (period start) |
| `end_time_ns` | int64 (period end / emission gate) |
| `value` | float64 |
| `fields_json` | utf8 (JSON-encoded extra fields) |
| `symbol` | utf8 (nullable, uppercase underlying) |

### `factor_files`

| Column | Type |
|---|---|
| `date_ns` | int64 |
| `price_factor` | float64 |
| `split_factor` | float64 |
| `reference_price` | float64 |

Factor and map files drive split/dividend adjustment, exactly as in LEAN.

### `map_files`

| Column | Type |
|---|---|
| `date_ns` | int64 |
| `ticker` | utf8 |

## Data S3 endpoint

The configured endpoint serves all Iceberg metadata, manifests, and Parquet
files. It may be a local [Verglas](https://github.com/cascade-labs/verglas)
endpoint or another compatible service, but it must be configured explicitly
with `data_s3_endpoint`, `data_s3_region`, and its access-key pair. rlean routes
both Iceberg FileIO and DataFusion through that endpoint using path-style
addressing; it never derives an AWS endpoint from the bucket name or falls back
to direct AWS reads. Use a loopback endpoint only on a machine that runs it.
