---
sidebar_position: 1
title: Overview
---

# Overview

rlean is a Rust algorithmic trading engine inspired by
[QuantConnect LEAN](https://github.com/QuantConnect/Lean). It is not a
line-for-line port of LEAN. Instead it aims for API parity with LEAN's strategy
interface — the goal is for existing `QCAlgorithm`-based Python strategies to run
with little or no modification — while adding a native Rust library for writing
high-performance strategies directly.

All market data is backed by [Apache Parquet](https://parquet.apache.org/), in
place of LEAN's CSV-based data layer.

## Features

- **Python strategy compatibility** — targets API parity with LEAN's
  `QCAlgorithm`, so most strategies written for LEAN run with little or no
  modification.
- **Rust strategy library** — implement `IAlgorithm` in Rust for zero-overhead
  backtests and live execution.
- **Parquet data layer** — trade bars, factor files, map files, and option
  chains are all stored in Parquet. No CSV.
- **Plugin system** — brokerages and data providers are runtime plugins,
  installed and managed via `rlean plugin`.
- **Research mode** — launches a Jupyter environment wired to the same engine
  used in backtests.

## Workspace crates

rlean is a Cargo workspace. The crates you are most likely to use as a library:

| Crate | Purpose |
|---|---|
| `lean-core` | Shared types: `Symbol`, `DateTime`, `Resolution`, `Market` |
| `lean-algorithm` | `IAlgorithm` trait, `QcAlgorithm` base, portfolio |
| `lean-engine` | `BacktestEngine`, `EngineConfig`, runner |
| `lean-data` | `Slice`, bar types, `IHistoricalDataProvider` |
| `lean-storage` | Parquet reader/writer for trade bars, factor files, option chains |
| `lean-options` | Option chain, greeks, exercise models |
| `lean-python` | PyO3 bindings — embed Python strategies in a Rust process |
| `lean-indicators` | SMA, EMA, RSI, Bollinger Bands, and more |
| `lean-orders` | Order types, fills, fee models |
| `lean-plugin` | Plugin ABI: descriptor, kind, factory function contracts |

The `rlean` crate is the CLI binary (`backtest`, `live`, `init`,
`create-project`, `plugin`, `config`, `cloud`).

## Where to go next

- [Getting Started](./getting-started.md) — install the CLI, create a workspace,
  write and run your first strategy.
- [Data Backend](./data-backend.md) — the Iceberg / REST catalog data store.
- [Python Strategy API](./python-strategy-api.md) — the `QCAlgorithm` surface.
- [Live Trading](./live-trading.md) — live deployments and the cloud fleet.
- [Plugin Development](./plugin-development.md) — write a brokerage or data
  provider plugin.
