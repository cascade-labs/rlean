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

All market and custom data enters the engine through a versioned Apache Arrow
Flight sidecar contract.

## Features

- **Python strategy compatibility** — targets API parity with LEAN's
  `QCAlgorithm`, so most strategies written for LEAN run with little or no
  modification.
- **Rust strategy library** — implement `IAlgorithm` in Rust for zero-overhead
  backtests and live execution.
- **Provider-neutral data plane** — trade bars, quotes, ticks, auxiliary files,
  universes, and custom data use canonical Arrow schemas.
- **Sidecar data plane** — backtest queries and pushed live data share a
  persistent, versioned Arrow Flight subscription session.
- **Sidecar integrations** — market data and brokerages run out of process over
  the Arrow Flight contract.
- **Research mode** — launches a Jupyter environment wired to the same engine
  used in backtests.

## Workspace crates

rlean is a Cargo workspace. The crates you are most likely to use as a library:

| Crate | Purpose |
|---|---|
| `rlean-core` | Shared types: `Symbol`, `DateTime`, `Resolution`, `Market` |
| `rlean-algorithm` | `IAlgorithm` trait, `QcAlgorithm` base, portfolio |
| `rlean-engine` | Backtest/live runners and subscription flow control |
| `rlean-data` | `Slice` and subscription definitions |
| `rlean-data-tables` | Canonical provider-neutral Arrow row and table contracts |
| `rlean-data-sidecar` | Versioned Arrow Flight protocol and client |
| `rlean-options` | Option chain, greeks, exercise models |
| `rlean-python-runtime` | PyO3 bindings — embed Python strategies in a Rust process |
| `rlean-indicators` | SMA, EMA, RSI, Bollinger Bands, and more |
| `rlean-orders` | Order types, fills, fee models |

The `rlean` crate is the CLI binary (`backtest`, `live`, `init`,
`create-project`, `config`, `cloud`).

## Where to go next

- [Getting Started](./getting-started.md) — install the CLI, create a workspace,
  write and run your first strategy.
- [Sidecar Data Plane](./sidecar-data-plane.md) — subscriptions, bounded
  backtest pulls, pushed live batches, integrations, and relays.
- [Data Contract and Sidecar](./data-backend.md) — canonical tables, discovery,
  and bounded queries.
- [Python Strategy API](./python-strategy-api.md) — the `QCAlgorithm` surface.
- [Live Trading](./live-trading.md) — live deployments and the cloud fleet.
