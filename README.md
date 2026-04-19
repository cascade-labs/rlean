# rlean

[![Tests Passing](https://github.com/cascade-labs/rlean/actions/workflows/test.yml/badge.svg?branch=main)](https://github.com/cascade-labs/rlean/actions/workflows/test.yml)
[![gitcgr](https://gitcgr.com/badge/cascade-labs/rlean.svg)](https://gitcgr.com/cascade-labs/rlean)

A Rust rewrite of [QuantConnect LEAN](https://github.com/QuantConnect/Lean), the open-source algorithmic trading engine. rlean targets the same strategy API as LEAN's C# Python bindings — existing `QCAlgorithm`-based strategies run unmodified — while adding a native Rust library for writing high-performance strategies directly. All market data is backed by [Apache Parquet](https://parquet.apache.org/), replacing LEAN's CSV-based data layer.

## Features

- **Python strategy compatibility** — `QCAlgorithm` API identical to LEAN C#. Strategies written for LEAN work as-is.
- **Rust strategy library** — implement `IAlgorithm` in Rust for zero-overhead backtests and live execution.
- **Parquet data layer** — trade bars, factor files, map files, and option chains all stored in Parquet. No CSV.
- **Plugin system** — brokerages and data providers are runtime plugins, installed and managed via `rlean plugin`.
- **Research mode** — launches a Jupyter environment wired to the same engine used in backtests.

---

## CI

GitHub Actions runs `cargo fmt --all --check`, `cargo check --workspace --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace --all-targets` on every push to `main` and on every pull request.

To enforce merge gates on GitHub, configure branch protection or a ruleset for `main` and mark these checks as required: `format`, `check`, `clippy`, and `test`.

---

## Installation

### Prerequisites

- Rust toolchain (`rustup` recommended)
- Python 3.10+ with a virtual environment active (for Python strategies)

### Install the CLI

```sh
cargo install --git https://github.com/cascade-labs/rlean --bin rlean
```

Or build from source:

```sh
git clone https://github.com/cascade-labs/rlean
cd rlean
cargo build --release -p rlean
cp target/release/rlean ~/.local/bin/
```

---

## Getting Started

### 1. Initialize a workspace

```sh
mkdir my-strategies && cd my-strategies
rlean init
```

This creates:

```
my-strategies/
  rlean.json      # workspace config (data root, default language)
  data/           # Parquet data directory
```

### 2. Create a project

```sh
rlean create-project my_first_strategy
```

### 3. Write a strategy

**Python** (`my_first_strategy/main.py`):

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

**Rust** (`src/my_strategy.rs`):

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

### 5. Configure data providers and API keys

```sh
rlean config set thetadata.api_key  <your-key>
rlean config set massive.api_key    <your-key>
```

---

## Using rlean as a Library

Add the crates you need to `Cargo.toml`:

```toml
[dependencies]
lean-core       = { git = "https://github.com/cascade-labs/rlean" }
lean-algorithm  = { git = "https://github.com/cascade-labs/rlean" }
lean-engine     = { git = "https://github.com/cascade-labs/rlean" }
lean-indicators = { git = "https://github.com/cascade-labs/rlean" }
lean-orders     = { git = "https://github.com/cascade-labs/rlean" }
lean-data       = { git = "https://github.com/cascade-labs/rlean" }
lean-storage    = { git = "https://github.com/cascade-labs/rlean" }
```

Key crates:

| Crate | Purpose |
|---|---|
| `lean-core` | Shared types: `Symbol`, `DateTime`, `Resolution`, `Market` |
| `lean-algorithm` | `IAlgorithm` trait, `QcAlgorithm` base, portfolio |
| `lean-engine` | `BacktestEngine`, `EngineConfig` |
| `lean-indicators` | SMA, EMA, RSI, Bollinger Bands, and more |
| `lean-orders` | Order types, fills, fee models |
| `lean-data` | `Slice`, bar types, `IHistoricalDataProvider` |
| `lean-storage` | Parquet reader/writer for trade bars, factor files, option chains |
| `lean-options` | Options chain, greeks, exercise models |
| `lean-python` | PyO3 bindings — embed Python strategies in a Rust process |

---

## Plugins

Brokerages and data providers are runtime plugins — compiled `cdylib` crates loaded from `~/.rlean/plugins/` at startup. No compile-time dependencies on specific brokers or data sources.

### List available plugins

```sh
rlean plugin list
```

```
NAME                   KIND             DESCRIPTION                                             STATUS
----------------------------------------------------------------------------------------------------
massive                data-provider    Massive.com (formerly Polygon.io) historical data provider installed
thetadata              data-provider    ThetaData options and equity historical data provider   installed
alpaca                 brokerage        Alpaca brokerage model (commission-free US equities)
binance                brokerage        Binance brokerage model (spot + USDT futures)
...
```

### Install a plugin

```sh
rlean plugin install thetadata
rlean plugin install alpaca
```

### Upgrade or remove

```sh
rlean plugin upgrade thetadata
rlean plugin remove  alpaca
```

### Install from a custom Git URL

```sh
rlean plugin install https://github.com/my-org/rlean-plugin-myprovider
```

### Manage registries

The official registry is always included. Additional registries can be added:

```sh
rlean plugin registry list
rlean plugin registry add    https://raw.githubusercontent.com/my-org/my-plugins/main/registry.json
rlean plugin registry remove https://raw.githubusercontent.com/my-org/my-plugins/main/registry.json
```

### Writing a plugin

A plugin is a Rust `cdylib` crate that exports a descriptor and one or more factory functions:

```rust
use lean_plugin::{PluginDescriptor, PluginKind};

#[no_mangle]
pub extern "C" fn rlean_plugin_descriptor() -> PluginDescriptor {
    PluginDescriptor {
        name:    c"myprovider",
        version: c"0.1.0",
        kind:    PluginKind::DataProvider,
    }
}
```

Implement `IHistoryProvider` (for data providers) or `IBrokerageModel` (for brokerages) and export factory functions to expose them. See any crate under `brokerages/` or `data_providers/` in the [rlean-plugins](https://github.com/cascade-labs/rlean-plugins) repo for a complete example.

---

## Data

All data lives under the workspace `data/` directory in Parquet format, mirroring the LEAN folder layout:

```
data/
  equity/
    usa/
      daily/
        spy/
          20200101_20241231.parquet
      factor_files/
        spy.parquet
      map_files/
        spy.parquet
  option/
    usa/
      chains/
        spy/
          20240115.parquet
```

Factor and map files are used for split/dividend adjustment, exactly as in LEAN.

---

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md).

## License

Apache 2.0 — see [LICENSE](./LICENSE).
