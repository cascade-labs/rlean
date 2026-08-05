# rlean

[![Tests](https://github.com/cascade-labs/rlean/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/cascade-labs/rlean/actions/workflows/test.yml)
[![Lint](https://github.com/cascade-labs/rlean/actions/workflows/lint.yml/badge.svg?branch=main)](https://github.com/cascade-labs/rlean/actions/workflows/lint.yml)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue)](./LICENSE)

rlean is a Rust implementation of the [QuantConnect LEAN](https://github.com/QuantConnect/Lean) engine's spec. It is not a fork or a line-for-line port of LEAN's C# code. It implements the LEAN spec from scratch in Rust and aims to cover the full spec over time.

You write strategies the same way you write them for LEAN. Python strategies use the same `QCAlgorithm` API surface, so most LEAN Python strategies run with little or no change. There is also a native Rust `IAlgorithm` trait for writing strategies directly in Rust.

## What "LEAN-compatible" means here

The Python API is kept LEAN-compatible. Some concrete examples:

- snake_case accessors: `c.bid_price + c.ask_price`, `portfolio.total_portfolio_value`
- enum comparisons, not string comparisons: `c.right == OptionRight.Put`
- iterable option chains: `for c in chain`
- nested bar access in research: `qb.bid.close`, `qb.ask.close`

If a strategy uses a corner of the LEAN API that rlean has not implemented yet, that corner is missing rather than different. The goal is that anything rlean does implement behaves the way LEAN does.

## Workspace layout

rlean is a Cargo workspace. Each crate owns one part of the engine.

| Crate | Purpose |
|---|---|
| `rlean-core` | Shared types: `Symbol`, `DateTime`, `Resolution`, `Market` |
| `rlean-data` | `Slice`, bar/quote/tick types, custom data, live data queue |
| `rlean-data-tables` | Canonical provider-neutral Arrow row and table contracts |
| `rlean-engine` | Backtest and live runners, `EngineConfig` |
| `rlean-sdk` | Convenience re-exports for building on rlean |
| `rlean-algorithm` | `IAlgorithm` trait, `QcAlgorithm` base, portfolio |
| `rlean-indicators` | SMA, EMA, RSI, Bollinger Bands, and more |
| `rlean-brokerages` | Built-in brokerage models |
| `rlean-orders` | Order types, fills, fee models |
| `rlean-risk` | Risk management framework |
| `rlean-portfolio-construction` | Portfolio construction models |
| `rlean-universe` | Universe selection |
| `rlean-consolidators` | Data consolidators |
| `rlean-alpha` | Alpha model framework |
| `rlean-scheduling` | Scheduled events |
| `rlean-statistics` | Backtest result statistics |
| `rlean-python-runtime` | PyO3 bindings — embeds Python strategies in a Rust process |
| `rlean-data-providers` | LEAN-style historical/live providers and cache-first Verglas storage |
| `rlean-options` | Option chain, greeks, exercise models |
| `rlean-execution` | Order routing / execution models |
| `rlean-optimization` | Parameter optimization |
| `rlean-live` | Live execution infrastructure |
| `rlean-forex` | Forex support |
| `rlean-crypto` | Crypto support |
| `rlean-futures` | Futures support |
| `rlean` | The CLI binary |

## Data backend

Historical providers, live providers, and brokerages are selected independently.
Historical reads are cache-first through the Verglas Rust SDK: rlean queries the
canonical tables, requests uncovered ranges from the selected provider, and
persists provider-neutral Arrow batches before consuming them. A single SDK
connection discovers the catalog, query, and write services from the configured
Verglas gateway.

Strategy SDK calls remain the source of subscription intent. Provider and
brokerage credentials such as `tradier.access_token` are stored once per
machine in `~/.rlean/integration-configs.json`.

Configure Verglas once per machine. The token is stored masked in CLI output
and is applied by the SDK to every service discovered from the gateway:

```sh
rlean config set verglas_endpoint http://127.0.0.1:8334
rlean config set verglas_token <token>
```

rlean never receives object-store credentials and does not configure catalog,
query, or write endpoints separately.

## Tables

`rlean-data-tables` defines eleven canonical Arrow table contracts. Use
`rlean data tables` and `rlean data schema <table>` as the executable source of
truth.

Prices, quantities, rates, and custom values use `decimal(38,18)`. Timestamps
are `i64` nanoseconds since the Unix epoch (UTC), and partition days are logical
calendar dates. Run `rlean data schema <table>` for the authoritative current schema.

### Market tables

`market_trade_bars`, `market_quote_bars`, and `market_ticks` share the same partitioning:

**Partition spec:** `identity(security_type)`, `identity(market)`,
`identity(resolution)`, `month(day)`.

`market_trade_bars` columns:

| Column | Type | Meaning |
|---|---|---|
| `time_ns` | i64 | Bar start, ns since epoch |
| `end_time_ns` | i64 | Bar end, ns since epoch |
| `symbol_sid` | i64 | Security identifier |
| `symbol_value` | string | Ticker string |
| `venue` | string (nullable during prototype backfill) | Physical data/execution venue; distinct from LEAN `market` |
| `open` / `high` / `low` / `close` | decimal(38,18) | Prices |
| `volume` | decimal(38,18) | Raw volume |
| `period_ns` | i64 | Bar period in ns |

`market_quote_bars` columns: `time_ns`, `end_time_ns`, `symbol_sid`, `symbol_value`, `venue`, nullable `bid_open`/`bid_high`/`bid_low`/`bid_close` and `ask_open`/`ask_high`/`ask_low`/`ask_close`, `last_bid_size`, `last_ask_size`, `period_ns`.

`market_ticks` columns: `time_ns`, `symbol_sid`, `symbol_value`, `venue`, `tick_type` (u8), `value`, `quantity`, `bid_price`/`ask_price`, `bid_size`/`ask_size`, nullable `exchange` and `sale_condition`, `suspicious` (bool). `exchange` remains the upstream per-tick exchange code; it is not the dataset-level `venue`.

### Rate tables

`margin_interest` columns: `time_ns`, `symbol_sid`, `symbol_value`, `venue`, `interest_rate`. Holds margin-interest / funding-rate rows.

### Option tables

`option_universe` stores the daily contract listing. Option prices and sizes use
the standard trade-bar, quote-bar, and tick tables at every resolution.

`option_universe` lists which contracts existed for an underlying on a date.
Inspect its complete current identity, OHLCV, open-interest, IV, and Greek
columns with `rlean data schema option_universe`.

### Custom data

`custom_points` holds custom / alternative data (FRED series, VIX, and so on).

**Partition spec:** `identity(provider)`, `identity(resolution)`, `month(day)`.

| Column | Type | Meaning |
|---|---|---|
| `time_ns` | i64 | Period start, ns since epoch (LEAN `BaseData.Time`) |
| `end_time_ns` | i64 | Period end / emission gate (LEAN `BaseData.EndTime`). NOT NULL — a point is never surfaced before this instant |
| `value` | decimal(38,18) | Primary scalar value |
| `fields_json` | string | JSON map of extra named fields |
| `venue` | string (nullable during prototype backfill) | Provider-defined venue or series origin |
| `symbol_sid` | i64 (nullable) | LEAN SID for symbol-bound points |
| `symbol_value` | string (nullable) | Display ticker for symbol-bound points |
| `provider` / `feed` / `resolution` | string | Stable routing and cadence fields |

The `end_time_ns` gate matches LEAN's EndTime semantics: a custom point is only visible to the algorithm at or after its end time, so daily data does not leak a day early (see #81 and #85).

### Corporate-action tables

`factor_files` (split/dividend adjustments) is partitioned by `market`. Its
factor values use the canonical decimal type.

`map_files` (ticker rename history) is partitioned by `market`.

## Live trading

`rlean live <strategy>` submits a strategy to the persistent `rleand` supervisor. Install it once with `rlean daemon install`; launchd on macOS or systemd on Linux keeps it running across logouts and reboots. You inspect or control deployments with subcommands.

Only long-running live deployments belong to `rleand`. Backtests, data and
configuration commands, research setup, and cloud probes run directly and exit.
Both `rlean` and `rleand` must be installed beside one another; `rlean daemon
status|start|stop|uninstall` manages the per-user service (or the machine-wide
service when `--system` is explicit).

- `rlean live list` — list local live deployments
- `rlean live status` / `portfolio` / `orders` / `logs` — inspect one deployment
- `rlean live pause` / `resume` / `upgrade` / `remove` — control a deployment

Pass `--brokerage <name>` (or `paper` for simulated fills) and select a
`--live-data-feed`. For brokerages with multiple accounts, pass
`--brokerage-account <account-id>`; the selected account is persisted with that
deployment and restored by `rleand`. Live data and brokerage operations use
independent native connections. Strategy SDK calls create and remove symbol
subscriptions; live events are pushed by the selected provider.

Historical market data can be selected independently with
`--data-provider-historical massive`. Massive TradeBars and QuoteBars are
converted to the provider-neutral contracts, queried cache-first through
Verglas, and synchronously persisted through the Verglas write role before the
engine consumes a newly fetched range. Configure the provider credential as
`massive.api_key`; rlean does not receive object-store credentials or embed an
Iceberg query engine.

### Cloud fleet

`rlean cloud` manages a fleet of remote nodes reachable over SSH. The control machine only syncs code and submits deployments; each node's `rleand` supervises its live strategy processes after that.

- `rlean cloud add-node` / `list` / `remove` — manage the node registry (`~/.rlean/nodes.json`)
- `rlean cloud exec` — run a command on a node
- `rlean cloud install` — install `rlean`, `rleand`, and configuration onto a node from a GitHub release bundle
- `rlean cloud deploy` — snapshot a strategy to a node and launch `rlean live` there
- `rlean cloud status` / `logs` / `portfolio` — monitor node deployments

`rlean cloud list` probes each node over SSH, reports both the installed rlean
version and rleand control-socket health, and refreshes `last_seen`. Pass
`--offline` for a registry-only view without network calls.
Re-running `rlean cloud install` with a release tag is the node binary-upgrade
flow: release checksums are verified, both binaries are atomically replaced,
configuration and `integration-configs.json` are copied with mode `0600`, and
the node's user `rleand` service is installed/started. `rlean live upgrade`, by
contrast, only refreshes a paused/stopped deployment's strategy snapshot.

The repository Justfile exposes local development and macOS daemon deployment:

```sh
just format        # apply rustfmt
just lint          # run the PR formatting, check, and clippy gates
just test          # run all workspace tests
just ci            # run every PR gate
just deploy-local  # validate, install rlean+rleand, and restart launchd
```

Cloud upgrades remain first-class rlean operations and are intentionally not
wrapped by Just: `rlean cloud install <name> --release-tag <tag>`.

## Build and install

Requires a Rust toolchain and Python 3.10+ (for Python strategies).

Build the CLI:

```sh
cargo build --release -p rlean
cp target/release/rlean ~/.local/bin/
```

Build everything, or run the tests:

```sh
cargo build --release
cargo test
```

### Releases

Releases are script-driven and then automatic:

1. `./scripts/bump-version.sh [patch|minor|major]` (default `patch`) bumps the workspace version and opens a `Release v<version>` PR against `main`.
2. Merge the PR. `.github/workflows/auto-tag.yml` tags `v<version>` and dispatches the release workflow.

The release workflow builds one `rlean-<version>-<triple>.tar.gz` per supported platform plus a top-level `manifest.json`. Supported triples: `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`. Tags containing a `-` (e.g. `v1.2.3-rc1`) publish as prereleases. `rlean cloud install` pulls the tarball matching each node's triple.

## Quick start

### 1. Configure data services

Configure the one Verglas gateway used by cache-first historical providers and
durable run results:

```sh
rlean config set verglas_endpoint http://127.0.0.1:8334
rlean config set verglas_token <token>
```

### 2. Create a workspace and project

```sh
rlean init
rlean create-project my_first_strategy
```

### 3. Write a strategy

Python (`my_first_strategy/main.py`):

```python
from AlgorithmImports import *

class MyStrategy(QCAlgorithm):
    def initialize(self):
        self.set_start_date(2020, 1, 1)
        self.set_end_date(2024, 1, 1)
        self.set_cash(100_000)
        self.spy = self.add_equity("SPY", Resolution.DAILY)

    def on_data(self, data):
        if not self.portfolio.invested:
            self.set_holdings(self.spy.symbol, 1.0)
```

Rust (`src/my_strategy.rs`):

```rust
use lean_algorithm::{algorithm::IAlgorithm, qc_algorithm::QcAlgorithm};
use lean_core::{Resolution, Symbol};
use lean_data::Slice;
use lean_orders::OrderEvent;
use rust_decimal_macros::dec;

struct MyStrategy {
    algo: QcAlgorithm,
    spy: Option<Symbol>,
}

impl IAlgorithm for MyStrategy {
    fn initialize(&mut self) -> lean_core::Result<()> {
        self.algo.set_start_date(2020, 1, 1);
        self.algo.set_end_date(2024, 1, 1);
        self.algo.set_cash(dec!(100_000));
        self.spy = Some(self.algo.add_equity("SPY", Resolution::Daily));
        Ok(())
    }

    fn on_data(&mut self, slice: &Slice) {
        if !self.algo.portfolio().invested() {
            if let Some(spy) = &self.spy {
                self.algo.set_holdings(spy, dec!(1.0));
            }
        }
    }

    fn on_order_event(&mut self, _event: &OrderEvent) {}
}
```

### 4. Run a backtest

```sh
rlean backtest my_first_strategy/main.py
```

### 5. Run live

```sh
rlean live my_first_strategy/main.py \
  --data-provider-historical massive \
  --live-data-feed tradier \
  --brokerage paper
```

## Known rough edges

Remote reads for very wide backtests (many symbols over long ranges) can be slow against a remote catalog. This is an active workstream.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md).

## License

Apache 2.0 — see [LICENSE](./LICENSE).
