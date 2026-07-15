# rlean — Codex Project Context

Rust rewrite of QuantConnect LEAN. Targets the same `QCAlgorithm` Python strategy API as LEAN's C# bindings while adding a native Rust `IAlgorithm` trait.

## Workspace Layout

```
crates/
  rlean-core          # Shared types: Symbol, DateTime, Resolution, Market
  rlean-algorithm     # IAlgorithm trait, QcAlgorithm base, portfolio
  rlean-engine        # BacktestEngine, EngineConfig, runner
  rlean-data          # Slice and subscription types
  rlean-data-tables   # Canonical provider-neutral Arrow table contracts
  rlean-options       # Option chain, greeks, exercise models, PyOptionChain/PyOptionContract
  rlean-python-runtime # PyO3 bindings — embeds Python strategies in a Rust process
  rlean-indicators    # SMA, EMA, RSI, Bollinger Bands, etc.
  rlean-orders        # Order types, fills, fee models
  rlean-live          # Live execution infrastructure
  rlean-brokerages    # Paper brokerage and built-in brokerage models
  rlean-data-sidecar # Arrow Flight data and brokerage protocol/client
  rlean-scheduling    # Scheduled events
  rlean-statistics    # Backtest result stats
  rlean-universe      # Universe selection
  rlean-execution     # Order routing / execution models
  rlean-risk          # Risk management framework
  rlean-consolidators # Data consolidators
  rlean-alpha         # Alpha model framework
  rlean-portfolio-construction
  rlean-optimization
  rlean-forex
  rlean-futures
  rlean-crypto
  rlean              # CLI binary (backtest, live, init, create-project, config)
```

## Data Architecture — Sidecar Only

- Strategy processes receive canonical batches exclusively through the sidecar;
  the sidecar owns vendor and persistence decisions.
- rlean has no local market-data cache, catalog client, or storage backend.
- If adding a new data type, define its provider-neutral Arrow contract in
  `rlean-data-tables`.

Backtests and live runs use a persistent Arrow Flight sidecar session. Live
feed and execution brokerage connections are independently authenticated.
Strategy SDK calls own add/remove subscription intent. Backtests issue bounded
range queries against registered subscriptions; live batches are pushed
unsolicited. The canonical wire and persisted schemas live in
`rlean-data-tables`, including the venue discriminator.

## Key Invariants

- **Option underlyings skip factor adjustment** — `option_underlying_sids` set in runner.rs; do not apply `apply_factor_row` to equity SIDs that serve as option underlyings (SPY price should be ~$411 not ~$383 after adjustment).
- **OTM expiry fires `on_order_event`** with `fill_price=0` and message `"OTM. Underlying: X. Profit: Y"` — not `on_assignment_order_event`. ITM assignments use `on_assignment_order_event`.
- **Daily resolution for options** — minute option quote zips from LEAN are corrupted; use daily resolution for option subscriptions.

## Python Compatibility

The Python API must stay LEAN-compatible:
- `c.right == OptionRight.Put` (not string comparison)
- `for c in chain` (iterable chain)
- `c.bid_price + c.ask_price` (snake_case)
- `portfolio.total_portfolio_value`
- `qb.bid.close` / `qb.ask.close` (nested `PyBar` on `QuoteBar`)

## Related Repos (sibling directories)

- **`../data_sidecar/`** — internal market/custom-data/brokerage Flight service and integration stubs.
- **`../Lean/`** — the original LEAN C# engine. Available for reference or spot-checking behavior against rlean's output.

## Build

```sh
cargo build --release -p rlean   # CLI only
cargo build --release             # all crates
cargo test                        # run all tests
```
