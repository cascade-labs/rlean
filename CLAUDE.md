# rlean — Claude Project Context

Rust rewrite of QuantConnect LEAN. Targets the same `QCAlgorithm` Python strategy API as LEAN's C# bindings while adding a native Rust `IAlgorithm` trait. All data is Apache Parquet — no CSV anywhere.

## Workspace Layout

```
crates/
  lean-core          # Shared types: Symbol, DateTime, Resolution, Market
  lean-algorithm     # IAlgorithm trait, QcAlgorithm base, portfolio
  lean-engine        # BacktestEngine, EngineConfig, runner
  lean-data          # Slice, bar types, IHistoricalDataProvider
  lean-storage       # Parquet reader/writer — trade bars, factor files, map files, option chains
  lean-options       # Option chain, greeks, exercise models, PyOptionChain/PyOptionContract
  lean-python        # PyO3 bindings — embeds Python strategies in a Rust process
  lean-indicators    # SMA, EMA, RSI, Bollinger Bands, etc.
  lean-orders        # Order types, fills, fee models
  lean-live          # Live execution infrastructure
  lean-plugin        # Plugin ABI: PluginDescriptor, PluginKind, factory function contracts
  lean-brokerages    # Built-in brokerage models
  lean-data-providers# Built-in data provider interfaces
  lean-scheduling    # Scheduled events
  lean-statistics    # Backtest result stats
  lean-universe      # Universe selection
  lean-execution     # Order routing / execution models
  lean-risk          # Risk management framework
  lean-consolidators # Data consolidators
  lean-alpha         # Alpha model framework
  lean-portfolio-construction
  lean-optimization
  lean-forex
  lean-futures
  lean-crypto
  rlean              # CLI binary (backtest, live, init, create-project, plugin, config)
```

## Data Architecture — Parquet Only

**All data is Parquet. No CSV. This is absolute.**

- `lean_csv_reader.rs` has been permanently deleted — do not recreate it.
- `parquet_migration.rs` has been permanently deleted — do not recreate it.
- Data lives under the workspace `data/` directory:

```
data/
  equity/usa/
    daily/spy/20200101_20241231.parquet
    factor_files/spy.parquet    # split/dividend adjustments
    map_files/spy.parquet       # ticker rename history
  option/usa/
    daily/spy_eod.parquet       # EOD option chains (from ThetaData)
    chains/spy/20240115.parquet # per-date option universe
```

- `lean-storage` owns all Parquet I/O — use `ParquetWriter`/`ParquetReader` from that crate.
- If adding a new data type: define a Parquet schema in `lean-storage`, never CSV.

## Plugin System

Brokerages and data providers are runtime `cdylib` plugins loaded from `~/.rlean/plugins/`. The `lean-plugin` crate defines the ABI.

Every plugin must export:
```rust
#[no_mangle]
pub extern "C" fn rlean_plugin_descriptor() -> PluginDescriptor { ... }
```

Plus factory functions for `IHistoryProvider` (data) or `IBrokerageModel` (brokerage). See `rlean-plugins/` for canonical implementations.

## Key Invariants

- **Option underlyings skip factor adjustment** — `option_underlying_sids` set in runner.rs; do not apply `apply_factor_row` to equity SIDs that serve as option underlyings (SPY price should be ~$411 not ~$383 after adjustment).
- **OTM expiry fires `on_order_event`** with `fill_price=0` and message `"OTM. Underlying: X. Profit: Y"` — not `on_assignment_order_event`. ITM assignments use `on_assignment_order_event`.
- **Daily resolution for options** — minute option quote zips from LEAN are corrupted; use daily resolution for option subscriptions.
- **Parquet price scale**: ThetaData stores prices in 1e8 units. LEAN zip format uses prices × 10000. `lean_strike` in LEAN filenames = dollars × 10000 (e.g., $411 → 4110000).

## Python Compatibility

The Python API must stay LEAN-compatible:
- `c.right == OptionRight.Put` (not string comparison)
- `for c in chain` (iterable chain)
- `c.bid_price + c.ask_price` (snake_case)
- `portfolio.total_portfolio_value`
- `qb.bid.close` / `qb.ask.close` (nested `PyBar` on `QuoteBar`)

## Related Repos (sibling directories)

- **`../rlean-plugins/`** — plugin source code (brokerages, data providers, custom data). If a plugin needs to be modified or a new one added, edit it there.
- **`../Lean/`** — the original LEAN C# engine. Available for reference or spot-checking behavior against rlean's output.

## Build

```sh
cargo build --release -p rlean   # CLI only
cargo build --release             # all crates
cargo test                        # run all tests
```
