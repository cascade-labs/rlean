# Running Lakekeeper locally for the S3 data store

`data_store = s3` resolves the market-data Iceberg tables through a
[Lakekeeper](https://lakekeeper.io) REST catalog. This directory holds a
docker-compose stack to run one locally (Lakekeeper + its Postgres, plus a MinIO
for a local-S3 warehouse) and the commands to bootstrap it.

`data_store = local` does **not** use Lakekeeper — it keeps a plain
local-filesystem warehouse with a co-located SQLite catalog. This is only for S3
mode.

## Bring the stack up

```sh
docker compose -f docs/lakekeeper/docker-compose.yaml up -d
```

`lakekeeper-migrate` runs the DB migration once and exits; `lakekeeper` then
serves the REST catalog on `http://localhost:8181`. MinIO listens on
`http://localhost:9100` (console `:9101`), with a `rlean-data` bucket created by
the `minio-createbucket` job. Auth is `allow-all` in this local config (no OIDC).

## Bootstrap + create the warehouse

```sh
# 1. Bootstrap the server (once).
curl -X POST http://localhost:8181/management/v1/bootstrap \
  -H 'Content-Type: application/json' -d '{"accept-terms-of-use": true}'   # -> 204

# 2. Create the warehouse that `data_warehouse` names (default: rlean).
#    Bucket names must be hyphenated, not underscored — Lakekeeper rejects
#    underscores (e.g. `rlean_data`) with HTTP 400 InvalidLocation.
curl -X POST http://localhost:8181/management/v1/warehouse \
  -H 'Content-Type: application/json' -d '{
    "warehouse-name": "rlean",
    "storage-profile": {
      "type": "s3", "bucket": "rlean-data", "key-prefix": "iceberg-lk",
      "endpoint": "http://minio:9000", "region": "us-east-1",
      "path-style-access": true, "flavor": "s3-compat", "sts-enabled": false
    },
    "storage-credential": {
      "type": "s3", "credential-type": "access-key",
      "aws-access-key-id": "minioadmin", "aws-secret-access-key": "minioadmin"
    }
  }'                                                                        # -> 201
```

For a real S3-compatible endpoint (e.g. OCI), swap `endpoint`, the
credentials, and the bucket in the storage profile above, and set the matching
`data_s3*` keys below to the same endpoint/creds.

## Point rlean at it

```sh
rlean config set data_store s3
rlean config set data_s3 s3://rlean-data/iceberg-lk
rlean config set data_s3_endpoint http://localhost:9100   # MinIO; or the real S3 endpoint
rlean config set data_s3_region us-east-1
rlean config set data_s3_access_key minioadmin
rlean config set data_s3_secret_key minioadmin
rlean config set data_catalog http://localhost:8181/catalog   # default
rlean config set data_warehouse rlean                         # default
```

`data_catalog` (env `RLEAN_DATA_CATALOG`) and `data_warehouse` (env
`RLEAN_DATA_WAREHOUSE`) apply only in s3 mode. The `data_s3*` endpoint/region/
credentials are forced onto every FileIO the catalog builds, so rlean does not
depend on Lakekeeper vending a complete S3 config.

## Tear down

```sh
docker compose -f docs/lakekeeper/docker-compose.yaml down -v
```
