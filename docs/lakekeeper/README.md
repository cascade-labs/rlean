# Running Lakekeeper locally for the S3 data store

`data_store = s3` resolves the market-data Iceberg tables through a
[Lakekeeper](https://lakekeeper.io) REST catalog. This directory holds a
docker-compose stack to run the catalog locally (Lakekeeper + its Postgres
metadata DB) and the commands to bootstrap it and point it at your S3 bucket.

The catalog runs locally, but the warehouse storage it manages is a real
S3-compatible object store (e.g. the OCI `rlean-data` bucket) — the compose
stack does **not** bundle any object store.

`data_store = local` needs **none** of this — no catalog server, no S3. Local
mode keeps a plain local-filesystem warehouse with a co-located SQLite catalog.
Everything below is for S3 mode only.

## Bring the catalog up

```sh
docker compose -f docs/lakekeeper/docker-compose.yaml up -d
```

`lakekeeper-migrate` runs the DB migration once and exits; `lakekeeper` then
serves the REST catalog on `http://localhost:8181`. Auth is `allow-all` in this
local config (no OIDC).

## Bootstrap + create the warehouse

The warehouse's storage profile points at your real S3 bucket. The example
below uses the OCI `rlean-data` bucket; swap the `endpoint`, `region`,
`bucket`, and credentials for whichever S3-compatible store you use.

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
      "endpoint": "https://<your-s3-endpoint>", "region": "us-ashburn-1",
      "path-style-access": true, "flavor": "s3-compat", "sts-enabled": false
    },
    "storage-credential": {
      "type": "s3", "credential-type": "access-key",
      "aws-access-key-id": "<key>", "aws-secret-access-key": "<secret>"
    }
  }'                                                                        # -> 201
```

## Point rlean at it

```sh
rlean config set data_store s3
rlean config set data_s3 s3://rlean-data/iceberg-lk
rlean config set data_s3_endpoint https://<your-s3-endpoint>
rlean config set data_s3_region us-ashburn-1
rlean config set data_s3_access_key <key>
rlean config set data_s3_secret_key <secret>
rlean config set data_catalog http://localhost:8181/catalog   # default
rlean config set data_warehouse rlean                         # default
```

Use the **same** endpoint / region / credentials / bucket here as in the
warehouse storage profile above.

`data_catalog` (env `RLEAN_DATA_CATALOG`) and `data_warehouse` (env
`RLEAN_DATA_WAREHOUSE`) apply only in s3 mode. The `data_s3*` endpoint/region/
credentials are forced onto every FileIO the catalog builds, so rlean does not
depend on Lakekeeper vending a complete S3 config.

## Tear down

```sh
docker compose -f docs/lakekeeper/docker-compose.yaml down -v
```
