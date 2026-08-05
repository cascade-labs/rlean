# rlean — Codex Project Context

Rust rewrite of QuantConnect LEAN. Targets the same `QCAlgorithm` Python strategy API as LEAN's C# bindings while adding a native Rust `IAlgorithm` trait.

## C# LEAN is the behavioral source of truth

Before changing framework or SDK behavior—subscriptions, universe selection,
filtering, scheduling, data synchronization, security lifecycle, brokerage
state, orders, fills, fees, portfolio accounting, or event ordering—locate and
read the exact implementation in `../Lean/` and port its semantics. Do not
infer behavior from a comment, approximate it from memory, or hand-roll an
alternative. Any intentional divergence requires an explicit user decision and
must be documented and covered by tests; PR descriptions should name the LEAN
classes or methods used as the reference.

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
  rlean-data-providers # Native historical/live providers and Verglas adapter
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

## Data Architecture — Native Providers + Verglas

- Historical providers, live providers, and execution brokerages are separate
  LEAN-style interfaces selected at deployment time. Strategy SDK calls still
  own add/remove subscription intent.
- Historical reads are cache-first: query canonical data through the Verglas
  SDK, request only durable uncovered ranges from the selected provider,
  persist canonical bounded Arrow batches, then consume the ordered result.
- Live provider events enter the synchronizer immediately and are independently
  persisted through a bounded asynchronous Verglas writer.
- The Verglas SDK connects to one gateway. Catalog metadata goes to the
  advertised catalog service; queries and writes are streamed to Verglas's
  isolated query and write roles. rlean must not embed Iceberg/DataFusion or
  receive object-store credentials.
- If adding a new data type, define its provider-neutral Arrow contract in
  `rlean-data-tables`.
- Private brokerage or data adapters integrate through the generic HTTP
  provider/brokerage contracts, not compiled plugins.

The canonical wire and persisted schemas live in `rlean-data-tables`, including
the venue discriminator. Provider adapters must convert vendor payloads into
these types before persistence; raw provider JSON is not a cache contract.

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

- **`../verglas/`** — canonical cache, catalog, query, and write services used through the Rust SDK.
- **`../Lean/`** — the original LEAN C# engine. Available for reference or spot-checking behavior against rlean's output.

## Build

```sh
cargo build --release -p rlean   # CLI only
cargo build --release             # all crates
cargo test                        # run all tests
```
