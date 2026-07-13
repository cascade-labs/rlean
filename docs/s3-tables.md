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
| `data_s3_endpoint`  | `RLEAN_DATA_S3_ENDPOINT`  | `--data-s3-endpoint`  | no       | Data-plane S3 endpoint for data-file reads (e.g. a local Verglas cache `http://127.0.0.1:8333`). Catalog traffic still goes to AWS |
| `data_s3_access_key_id`     | `RLEAN_DATA_S3_ACCESS_KEY_ID`     | —  | no | Access key id for `data_s3_endpoint` (endpoint-issued key, NOT an AWS key) |
| `data_s3_secret_access_key` | `RLEAN_DATA_S3_SECRET_ACCESS_KEY` | —  | no | Secret access key for `data_s3_endpoint` (endpoint-issued key, NOT an AWS key) |

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
`iceberg_maintenance` binary (in `lean-storage`) can still `report`, `count`,
`query`, and `reset` tables; it reads the same `RLEAN_DATA_*` environment
(including `RLEAN_DATA_NAMESPACE`, default `lean`).

## Tests

Store-backed tests require a live REST catalog and are `#[ignore]`d. Enable them
by setting `RLEAN_TEST_CATALOG` (+ `RLEAN_TEST_WAREHOUSE`, optional
`RLEAN_TEST_SIGV4_REGION` / `RLEAN_TEST_SIGV4_NAME`). `RLEAN_TEST_NAMESPACE`
selects the namespace and defaults to `lean_dev` — an isolated scratch namespace
that never touches the production `lean` tables.

## Local Verglas cache (data-file read-through)

Data files (the Parquet the catalog points at) can be read through a local
[Verglas](https://github.com/cascade-labs/verglas) read-through cache instead of
straight from AWS S3. Set `data_s3_endpoint` to the daemon's S3 endpoint plus
the two endpoint keys it issued:

```
data_s3_endpoint=http://127.0.0.1:8333
data_s3_access_key_id=<verglas endpoint access key id>
data_s3_secret_access_key=<verglas endpoint secret>
```

The endpoint keys are Verglas-issued loopback keys, not AWS credentials. When
the endpoint is set, rlean points **data-file I/O only** at it — the Iceberg S3
FileIO and every DataFusion object store get the endpoint, the endpoint keys,
path-style addressing, and (for an `http://` loopback endpoint) plain HTTP —
while catalog requests keep going, SigV4-signed, to AWS S3 Tables through the
in-process proxy. The catalog's AWS credential resolution is unchanged.

The override is host-local: it is deliberately **not** propagated to remote
nodes by `rlean cloud install` (a node's loopback is a different machine with no
such cache), so fleet nodes read data files directly from AWS.

When `data_s3_endpoint` is unset (the production default) behavior is identical
to reading data files directly from AWS. Precedence for all three keys is the
usual **CLI flag > env var > config file** (only the endpoint has a CLI flag;
the keys are env/config only). The override activates only when the endpoint and
both keys resolve; a missing key leaves it off.
