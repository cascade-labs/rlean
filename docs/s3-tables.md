# Market data store: AWS S3 Tables (REST Iceberg catalog)

rlean reads and writes all market data through a REST Iceberg catalog. There is
no local or filesystem data store — a run cannot start without a catalog URI and
warehouse. In production the catalog is AWS S3 Tables.

## Configuration

Set these with `rlean config set <key> <value>`. Environment variables can
override a setting for one process. The catalog and all four data-S3 settings
are required before rlean can read or cache market data; there is no local store
or direct-AWS fallback.

| Config key          | Env var                   | CLI flag              | Required | Meaning |
|---------------------|---------------------------|-----------------------|----------|---------|
| `data_catalog`      | `RLEAN_DATA_CATALOG`      | `--data-catalog`      | yes      | REST catalog base URI |
| `data_warehouse`    | `RLEAN_DATA_WAREHOUSE`    | `--data-warehouse`    | yes      | Warehouse id / S3 Tables table-bucket ARN |
| `data_sigv4_region` | `RLEAN_DATA_SIGV4_REGION` | `--data-sigv4-region` | no       | SigV4 signing region (turns on signing) |
| `data_sigv4_name`   | `RLEAN_DATA_SIGV4_NAME`   | `--data-sigv4-name`   | no       | SigV4 signing name (default `s3tables`) |
| `data_namespace`    | `RLEAN_DATA_NAMESPACE`    | `--data-namespace`    | no       | Iceberg namespace (default `lean`) |
| `data_s3_endpoint`  | `RLEAN_DATA_S3_ENDPOINT`  | —  | yes | S3-compatible endpoint for every Iceberg metadata, manifest, and Parquet request |
| `data_s3_region`    | `RLEAN_DATA_S3_REGION`    | —  | yes | Region expected when signing requests to `data_s3_endpoint` |
| `data_s3_access_key_id`     | `RLEAN_DATA_S3_ACCESS_KEY_ID`     | —  | yes | Access key id issued by `data_s3_endpoint` |
| `data_s3_secret_access_key` | `RLEAN_DATA_S3_SECRET_ACCESS_KEY` | —  | yes | Secret access key issued by `data_s3_endpoint` |

When `data_sigv4_region` is unset the catalog is used unsigned (plain / OAuth
REST catalog, e.g. a local mock). When it is set, catalog requests are signed
with SigV4 and `data_sigv4_name` defaults to `s3tables`.

## Working AWS S3 Tables values

```
data_catalog=https://s3tables.us-west-2.amazonaws.com/iceberg
data_warehouse=arn:aws:s3tables:us-west-2:<acct>:bucket/<name>
data_sigv4_region=us-west-2
data_sigv4_name=s3tables
```

## AWS credentials

Catalog credentials come from the ambient AWS credential chain, resolved
in-process at connect time (`aws-config`). This includes AWS SSO / `aws sso
login` sessions. They are used only to sign AWS catalog requests.

The SigV4 proxy is an in-process localhost forwarder: `iceberg-catalog-rest` has
no per-request signing hook, so rlean points the catalog at a local proxy that
signs each request with SigV4 and forwards it to the real endpoint. It is
started at connect time and lives for the whole run.

## Maintenance and compaction

Compaction and snapshot expiry are **AWS-managed** (S3 Tables managed
maintenance). rlean no longer runs end-of-run compaction. The
`iceberg_maintenance` binary (in `rlean-storage`) can still `report`, `count`,
`query`, and `reset` tables; it requires the same catalog and four
`RLEAN_DATA_S3_*` settings (with `RLEAN_DATA_NAMESPACE` defaulting to `lean`).

## Tests

Store-backed tests require a live REST catalog and endpoint and are `#[ignore]`d.
Set `RLEAN_TEST_CATALOG`, `RLEAN_TEST_WAREHOUSE`, and all four
`RLEAN_TEST_S3_*` settings. `RLEAN_TEST_SIGV4_REGION` /
`RLEAN_TEST_SIGV4_NAME` are optional. `RLEAN_TEST_NAMESPACE` selects the
namespace and defaults to `lean_dev` — an isolated scratch namespace that never
touches the production `lean` tables.

## Data S3 endpoint

The configured endpoint serves all Iceberg metadata, manifest, and Parquet I/O.
For example, with a local [Verglas](https://github.com/cascade-labs/verglas)
endpoint, configure the URL, its signing region, and the credentials it issued:

```
data_s3_endpoint=http://127.0.0.1:8333
data_s3_region=us-west-2
data_s3_access_key_id=<verglas endpoint access key id>
data_s3_secret_access_key=<verglas endpoint secret>
```

These are endpoint credentials, not AWS catalog credentials. rlean supplies
them to the Iceberg FileIO and every DataFusion object store, using path-style
addressing and allowing HTTP only for an `http://` endpoint. It never falls back
to a derived AWS endpoint. A remote node must have a reachable endpoint of its
own configured; do not use a loopback URL unless the endpoint runs on that node.
