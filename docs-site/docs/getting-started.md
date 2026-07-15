---
sidebar_position: 2
title: Getting Started
---

# Getting Started

## Prerequisites

- Rust toolchain (`rustup` recommended)
- Python 3.10+ with a virtual environment active (for Python strategies)

## Install the CLI

Install straight from the repository:

```sh
cargo install --git https://github.com/cascade-labs/rlean rlean --bin rlean
```

Or build from source:

```sh
git clone https://github.com/cascade-labs/rlean
cd rlean
cargo build --release -p rlean
cp target/release/rlean ~/.local/bin/
```

### Install from a release artifact

Each tagged release publishes a `rlean-<version>-<triple>.tar.gz` bundle for
every supported platform (`aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`,
`aarch64-apple-darwin`), plus a top-level `manifest.json` describing the release.
Download the tarball matching your platform from the
[GitHub Releases](https://github.com/cascade-labs/rlean/releases) page, unpack it,
and put the `rlean` binary on your `PATH`.

## 1. Initialize a workspace

```sh
mkdir my-strategies && cd my-strategies
rlean init
```

This creates:

```
my-strategies/
  rlean.json      # workspace config
```

## 2. Configure the sidecar endpoint

Backtests, live deployments, research, and data tooling all communicate with a
sidecar. rlean does not configure or access the sidecar's storage backend.

```sh
rlean config set data_sidecar grpc://127.0.0.1:7410
# Only when the sidecar requires authentication:
rlean config set data_sidecar_token <token>
```

See [Data Contract and Sidecar](./data-backend.md) for the canonical tables and
query tooling.

## 3. Create a project

```sh
rlean create-project my_first_strategy
```

## 4. Write a strategy

### Python (`my_first_strategy/main.py`)

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

### Rust (`src/my_strategy.rs`)

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

## 5. Run a backtest

```sh
rlean backtest my_first_strategy/main.py
```

## 6. Configure the sidecar

```sh
rlean config set data_sidecar grpc://127.0.0.1:7410
rlean config set thetadata.api_key  <your-key>
rlean config set massive.api_key    <your-key>
```

The endpoint is stored in `~/.rlean/config`. Dotted integration keys are stored
in the owner-only `~/.rlean/integration-configs.json` and passed opaquely to the
sidecar. Strategies still declare symbols and resolutions through calls such as
`add_equity`; configuration only selects and authenticates integrations.

## Using rlean as a library

Add the crates you need to `Cargo.toml`:

```toml
[dependencies]
rlean-core        = { git = "https://github.com/cascade-labs/rlean" }
rlean-algorithm   = { git = "https://github.com/cascade-labs/rlean" }
rlean-engine      = { git = "https://github.com/cascade-labs/rlean" }
rlean-indicators  = { git = "https://github.com/cascade-labs/rlean" }
rlean-orders      = { git = "https://github.com/cascade-labs/rlean" }
rlean-data        = { git = "https://github.com/cascade-labs/rlean" }
rlean-data-tables = { git = "https://github.com/cascade-labs/rlean" }
```
