---
sidebar_position: 4
title: Data Contract and Sidecar
---

# Data Contract and Sidecar

Strategy processes do not load an in-process data provider or storage backend.
They receive canonical Arrow record batches through the persistent Flight
session described in [Sidecar Data Plane](./sidecar-data-plane.md).

The sidecar owns provider and persistence decisions. Those decisions do not
change the contract seen by rlean.

## Canonical tables

`rlean-data-tables` is the authoritative contract shared by the engine and
sidecars. It currently defines:

| Table | Partition specification |
|---|---|
| `market_trade_bars` | `identity(security_type), identity(market), identity(resolution), month(day)` |
| `market_quote_bars` | `identity(security_type), identity(market), identity(resolution), month(day)` |
| `market_ticks` | `identity(security_type), identity(market), identity(resolution), month(day)` |
| `margin_interest` | `identity(security_type), identity(market), identity(resolution), month(day)` |
| `custom_points` | `identity(provider), identity(resolution), month(day)` |
| `option_universe` | `identity(market), month(day)` |
| `future_universe` | `identity(market), month(day)` |
| `fundamental_universe` | `identity(market), month(day)` |
| `etf_constituents` | `identity(market), month(day)` |
| `factor_files` | `identity(market)` |
| `map_files` | `identity(market)` |

Use the CLI for the exact current fields, nullability, descriptions, and
partition transforms:

```sh
rlean data tables
rlean data schema <table>
```

Prices, quantities, rates, and custom values use `decimal(38,18)`. Timestamps
use signed 64-bit Unix epoch nanoseconds in UTC. `day` is a logical calendar
date used for query pruning.

Market rows contain `venue` separately from `market`. `market` participates in
the LEAN symbol identity; `venue` identifies the physical dataset or exchange.
Ticks may additionally carry an upstream per-tick `exchange` code. Custom rows
also have an optional venue so two valid observations from different exchanges
or provider-defined series origins remain distinct. Venue remains nullable
while existing prototype data is backfilled.

Custom rows preserve provider-specific fields in `fields_json` and route by
`provider`, `feed`, and `resolution`. `end_time_ns` is the availability
frontier: a point must not be emitted to a strategy before that instant.

## Sidecar discovery and queries

rlean can inspect the sidecar-owned manifest and run bounded tooling queries
without direct storage access:

```sh
rlean data manifest --json
rlean data query custom_points SPY --provider unusual_whales \
  --feed flow_alerts --resolution tick \
  --start 2026-07-01 --end 2026-07-15 --json
```

The query uses the same temporary subscription and Arrow `DataBatch` exchange
as a backtest. JSON output is an optional CLI rendering of those Arrow rows.
