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
| `lean-core` | Shared types: `Symbol`, `DateTime`, `Resolution`, `Market` |
| `lean-data` | `Slice`, bar/quote/tick types, custom data, live data queue |
| `lean-storage` | Iceberg-backed data I/O: all table schemas and reads/writes |
| `lean-engine` | Backtest and live runners, `EngineConfig` |
| `lean-sdk` | Convenience re-exports for building on rlean |
| `lean-algorithm` | `IAlgorithm` trait, `QcAlgorithm` base, portfolio |
| `lean-indicators` | SMA, EMA, RSI, Bollinger Bands, and more |
| `lean-brokerages` | Built-in brokerage models |
| `lean-orders` | Order types, fills, fee models |
| `lean-risk` | Risk management framework |
| `lean-portfolio-construction` | Portfolio construction models |
| `lean-universe` | Universe selection |
| `lean-consolidators` | Data consolidators |
| `lean-alpha` | Alpha model framework |
| `lean-scheduling` | Scheduled events |
| `lean-statistics` | Backtest result statistics |
| `lean-python-runtime` | PyO3 bindings — embeds Python strategies in a Rust process |
| `lean-data-providers` | Data provider traits (`IHistoryProvider`, custom data) |
| `lean-options` | Option chain, greeks, exercise models |
| `lean-execution` | Order routing / execution models |
| `lean-optimization` | Parameter optimization |
| `lean-live` | Live execution infrastructure |
| `lean-forex` | Forex support |
| `lean-crypto` | Crypto support |
| `lean-futures` | Futures support |
| `lean-plugin` | Plugin ABI: `PluginDescriptor`, `PluginKind`, factory contracts |
| `rlean` | The CLI binary |

## Data backend

All market data is Apache Parquet stored in [Apache Iceberg](https://iceberg.apache.org/) tables, read and written through an Iceberg REST catalog. There is no CSV anywhere, and there is no local file warehouse — the SQLite/`file://` warehouse was removed. In production the catalog is AWS S3 Tables.

`lean-storage` owns all persisted data I/O. Data providers only return rows to the engine; the engine is the single writer. Providers never write files or query storage.

### Catalog config

Set these in `~/.rlean/config` with `rlean config set <key> <value>`. The
catalog, warehouse, and all four `data_s3_*` settings are required before rlean
can read or cache market data. Environment variables can override them for one
process; catalog flags are available for one-off catalog overrides. The
authoritative set lives in `crates/rlean/src/config.rs`.

| Key | Meaning |
|---|---|
| `data_catalog` | REST catalog base URI, e.g. `https://s3tables.us-west-2.amazonaws.com/iceberg`. Required at run time. |
| `data_warehouse` | Warehouse identifier. For S3 Tables this is the table-bucket ARN, e.g. `arn:aws:s3tables:us-west-2:<acct>:bucket/<name>`. Required at run time. |
| `data_sigv4_region` | SigV4 signing region (e.g. `us-west-2`). When set, catalog requests are signed; when unset the catalog is used unsigned. |
| `data_sigv4_name` | SigV4 signing name / service. Defaults to `s3tables` when a region is set but the name is not. |
| `data_namespace` | Iceberg namespace holding the tables. Defaults to `lean`. |
| `data_refresh_secs` | How often (seconds) a long-running process rechecks the catalog for snapshots committed by other processes. Defaults to 30; `0` rechecks on every read. |
| `data_s3_endpoint` | Required S3-compatible endpoint for every Iceberg metadata, manifest, and Parquet request, e.g. `http://127.0.0.1:8333`. |
| `data_s3_region` | Required region expected when signing requests to `data_s3_endpoint`. |
| `data_s3_access_key_id` | Required access key issued by `data_s3_endpoint`. |
| `data_s3_secret_access_key` | Required secret issued by `data_s3_endpoint`. |

The catalog and data endpoint are different things. The catalog identifies
Iceberg tables; the data endpoint serves the metadata, manifests, and Parquet
objects those tables reference. rlean does not derive an AWS endpoint from a
bucket name or use one as a fallback. Catalog SigV4 credentials are only for
catalog requests; data endpoint credentials are the explicit `data_s3_*` pair.

## Tables

`lean-storage` defines eleven Iceberg tables (see `crates/lean-storage/src/schema.rs` and `crates/lean-storage/src/iceberg_store.rs`). Every partition field uses the Iceberg identity transform.

Prices are stored as scaled `i64` (multiply the decimal price by `1e8`). Timestamps are `i64` nanoseconds since the Unix epoch (UTC). Dates are `i64` nanoseconds at midnight UTC unless noted.

### Market tables

`market_trade_bars`, `market_quote_bars`, and `market_ticks` share the same partitioning:

**Partition spec:** `security_type`, `market`, `resolution`, `symbol_sid`, `day`.

`market_trade_bars` columns:

| Column | Type | Meaning |
|---|---|---|
| `time_ns` | i64 | Bar start, ns since epoch |
| `end_time_ns` | i64 | Bar end, ns since epoch |
| `symbol_sid` | i64 | Security identifier |
| `symbol_value` | string | Ticker string |
| `open` / `high` / `low` / `close` | i64 | Prices, ×1e8 |
| `volume` | i64 | Raw volume |
| `period_ns` | i64 | Bar period in ns |

`market_quote_bars` columns: `time_ns`, `end_time_ns`, `symbol_sid`, `symbol_value`, nullable `bid_open`/`bid_high`/`bid_low`/`bid_close` and `ask_open`/`ask_high`/`ask_low`/`ask_close` (all ×1e8), `last_bid_size`, `last_ask_size`, `period_ns`.

`market_ticks` columns: `time_ns`, `symbol_sid`, `symbol_value`, `tick_type` (u8), `value` (×1e8), `quantity`, `bid_price`/`ask_price` (×1e8), `bid_size`/`ask_size`, nullable `exchange` and `sale_condition`, `suspicious` (bool).

### Rate and context tables

`margin_interest` and `perpetual_context` are both partitioned by `security_type`, `market`, `day`.

`margin_interest` columns: `time_ns`, `symbol_sid`, `symbol_value`, `interest_rate` (×1e8). Holds margin-interest / funding-rate rows.

`perpetual_context` columns: `time_ns`, `end_time_ns`, `symbol_sid`, `symbol_value`, `period_ns`, plus perpetual-future context values (all ×1e8): `funding`, `open_interest`, `prev_day_px`, `day_ntl_vlm`, `premium`, `oracle_px`, `mark_px`, `mid_px`, `impact_bid_px`, `impact_ask_px`.

### Option tables

`option_eod_bars` and `option_universe` are both partitioned by `day`.

`option_eod_bars` holds one end-of-day bar per contract. Columns: `date_ns`, `symbol_value` (full OSI ticker), `underlying`, `expiration_ns`, `strike` (×1e8), `right` (`"C"` or `"P"`), `open`/`high`/`low`/`close`/`bid`/`ask` (×1e8), `volume` (raw contracts), `bid_size`/`ask_size` (raw contracts).

`option_universe` lists which contracts existed for an underlying on a date. Columns: `date_ns`, `symbol_value`, `underlying`, `expiration_ns`, `strike` (×1e8), `right`.

### Custom data

`custom_points` holds custom / alternative data (FRED series, VIX, and so on).

**Partition spec:** `provider`, `feed`.

| Column | Type | Meaning |
|---|---|---|
| `time_ns` | i64 | Period start, ns since epoch (LEAN `BaseData.Time`) |
| `end_time_ns` | i64 | Period end / emission gate (LEAN `BaseData.EndTime`). NOT NULL — a point is never surfaced before this instant |
| `value` | f64 | Primary scalar value |
| `fields_json` | string | JSON map of extra named fields |
| `symbol` | string (nullable) | Canonical uppercase underlying ticker |

The `end_time_ns` gate matches LEAN's EndTime semantics: a custom point is only visible to the algorithm at or after its end time, so daily data does not leak a day early (see #81 and #85).

### Corporate-action tables

`factor_files` (split/dividend adjustments) is partitioned by `market`, `ticker`. Columns: `date_ns`, `price_factor` (f64), `split_factor` (f64), `reference_price` (f64).

`map_files` (ticker rename history) is partitioned by `market`, `permtick`. Columns: `date_ns`, `ticker`.

## Live trading

`rlean live <strategy>` runs a strategy against a live brokerage and data feed. A live run is a detached deployment: the engine keeps running after the command returns, and you inspect or control it with subcommands.

- `rlean live list` — list local live deployments
- `rlean live status` / `portfolio` / `orders` / `logs` — inspect one deployment
- `rlean live pause` / `resume` / `upgrade` / `remove` — control a deployment

Brokerages and live data feeds are plugins. Pass `--brokerage <name>` (or `paper` for simulated fills) and `--data-provider-live <name>`. Plugins are loaded at run time from `~/.rlean/plugins/`.

### Cloud fleet

`rlean cloud` manages a fleet of remote nodes reachable over SSH. The control machine only syncs code and launches; each node records its own deployment via `rlean live` and manages itself after that.

- `rlean cloud add-node` / `list` / `remove` / `probe` — manage the node registry (`~/.rlean/nodes.json`)
- `rlean cloud exec` — run a command on a node
- `rlean cloud install` — install the rlean binary and plugins onto a node from GitHub release bundles
- `rlean cloud deploy` — snapshot a strategy to a node and launch `rlean live` there
- `rlean cloud status` / `logs` / `portfolio` — monitor node deployments

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

### 1. Configure the data catalog

A backtest reads and caches market data through both an Iceberg catalog and a
configured S3-compatible data endpoint. Set both up first. The example uses
AWS S3 Tables for the catalog and a local Verglas endpoint for data I/O; use
the endpoint URL, signing region, and credentials supplied by your provider.

```sh
rlean config set data_catalog https://s3tables.us-west-2.amazonaws.com/iceberg
rlean config set data_warehouse arn:aws:s3tables:us-west-2:<acct>:bucket/<name>
rlean config set data_sigv4_region us-west-2
rlean config set data_s3_endpoint http://127.0.0.1:8333
rlean config set data_s3_region us-west-2
rlean config set data_s3_access_key_id <endpoint-access-key>
rlean config set data_s3_secret_access_key <endpoint-secret>
```

Without this configuration, rlean refuses to start market-data reads and has
no data-caching mode.

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

### 5. Install a data provider and run live

```sh
rlean plugin install massive
rlean live my_first_strategy/main.py --brokerage paper --data-provider-live tradier
```

## Plugins

Brokerages, data providers, and custom-data sources are runtime `cdylib` plugins loaded from `~/.rlean/plugins/`. rlean has no compile-time dependency on any specific broker or data source.

```sh
rlean plugin list                 # list available and installed plugins
rlean plugin install massive      # clone, build, and install
rlean plugin upgrade massive      # rebuild from latest source
rlean plugin remove massive       # uninstall
```

The plugin ABI lives in the `lean-plugin` crate. Plugin source and the plugin author's guide live in the sibling [rlean-plugins](https://github.com/cascade-labs/rlean-plugins) repo.

## Known rough edges

Remote reads for very wide backtests (many symbols over long ranges) can be slow against a remote catalog. This is an active workstream.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md).

## License

Apache 2.0 — see [LICENSE](./LICENSE).
