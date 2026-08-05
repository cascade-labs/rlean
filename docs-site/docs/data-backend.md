---
sidebar_position: 4
title: Data Providers and Contract
---

# Data Providers and Contract

rlean uses separate LEAN-style interfaces for historical providers and live
providers. Historical reads are cache-first through the Verglas Rust SDK;
uncovered ranges are fetched from the selected provider and persisted as
canonical Arrow batches before the engine consumes them.

## Canonical tables

`rlean-data-tables` is the authoritative contract shared by the engine,
providers, and Verglas persistence. It currently defines:

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
