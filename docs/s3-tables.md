# Market data store: AWS S3 Tables (REST Iceberg catalog)

rlean reads and writes all market data through a REST Iceberg catalog. There is
no local or filesystem data store — a run cannot start without a catalog URI and
warehouse. In production the catalog is AWS S3 Tables.

## Configuration

Set these with `rlean config set <key> <value>`, or the matching env var, or the
matching CLI flag. Precedence is **CLI flag > env var > config file**.

| Config key          | Env var                   | CLI flag              | Required | Meaning |
|---------------------|---------------------------|-----------------------|----------|---------|
| `data_catalog`      | `RLEAN_DATA_CATALOG`      | `--data-catalog`      | yes      | REST catalog base URI |
| `data_warehouse`    | `RLEAN_DATA_WAREHOUSE`    | `--data-warehouse`    | yes      | Warehouse id / S3 Tables table-bucket ARN |
| `data_sigv4_region` | `RLEAN_DATA_SIGV4_REGION` | `--data-sigv4-region` | no       | SigV4 signing region (turns on signing) |
| `data_sigv4_name`   | `RLEAN_DATA_SIGV4_NAME`   | `--data-sigv4-name`   | no       | SigV4 signing name (default `s3tables`) |
| `data_namespace`    | `RLEAN_DATA_NAMESPACE`    | `--data-namespace`    | no       | Iceberg namespace (default `lean`) |

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

Credentials come from the ambient AWS credential chain, resolved in-process at
connect time (`aws-config`). This includes AWS SSO / `aws sso login` sessions:
the resolved temporary credentials are fed explicitly into the Iceberg S3 FileIO
props, the DataFusion object store, and the in-process SigV4 signing proxy. You
do not need to export static keys — `aws sso login` (or any provider the default
chain understands) is enough.

The SigV4 proxy is an in-process localhost forwarder: `iceberg-catalog-rest` has
no per-request signing hook, so rlean points the catalog at a local proxy that
signs each request with SigV4 and forwards it to the real endpoint. It is
started at connect time and lives for the whole run.

## Maintenance and compaction

Compaction and snapshot expiry are **AWS-managed** (S3 Tables managed
maintenance). rlean no longer runs end-of-run compaction. The
`iceberg_maintenance` binary (in `rlean-storage`) can still `report`, `count`,
`query`, and `reset` tables; it reads the same `RLEAN_DATA_*` environment
(including `RLEAN_DATA_NAMESPACE`, default `lean`).

## Tests

Store-backed tests require a live REST catalog and are `#[ignore]`d. Enable them
by setting `RLEAN_TEST_CATALOG` (+ `RLEAN_TEST_WAREHOUSE`, optional
`RLEAN_TEST_SIGV4_REGION` / `RLEAN_TEST_SIGV4_NAME`). `RLEAN_TEST_NAMESPACE`
selects the namespace and defaults to `lean_dev` — an isolated scratch namespace
that never touches the production `lean` tables.

## Future work

Verglas read-through caching for the catalog is a planned follow-up (#49).
