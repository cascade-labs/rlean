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
  rlean.json      # workspace config (data root, default language)
  data/           # workspace scratch directory; not the market-data cache
```

## 2. Configure Iceberg and the data endpoint

Market data and its cache always use an Iceberg REST catalog plus an explicit
S3-compatible endpoint. Set both before running a backtest, live deployment,
or research session. This example uses AWS S3 Tables for the catalog and a
local Verglas endpoint for data I/O. Replace the endpoint URL, signing region,
and credentials with the values from your provider.

```sh
rlean config set data_catalog https://s3tables.us-west-2.amazonaws.com/iceberg
rlean config set data_warehouse arn:aws:s3tables:us-west-2:<acct>:bucket/<name>
rlean config set data_sigv4_region us-west-2
rlean config set data_s3_endpoint http://127.0.0.1:8333
rlean config set data_s3_region us-west-2
rlean config set data_s3_access_key_id <endpoint-access-key>
rlean config set data_s3_secret_access_key <endpoint-secret>
```

`data_catalog` identifies the Iceberg tables. `data_s3_endpoint` serves their
metadata, manifests, and Parquet files. rlean will not infer an AWS endpoint or
use a hidden local store when these are absent; it stops with a configuration
error instead. See [Data Backend](./data-backend.md) for every setting.

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

## 6. Configure data providers and API keys

```sh
rlean config set thetadata.api_key  <your-key>
rlean config set massive.api_key    <your-key>
```

## Using rlean as a library

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
