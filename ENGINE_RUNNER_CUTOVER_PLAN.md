# Engine Runner Cutover Plan

## Objective

Move all engine execution ownership into `lean-engine` in one complete cutover.

After this work lands, `lean-python` must not own a backtest loop, live loop, data loop,
warmup loop, parquet/provider orchestration, benchmark loader, subscription data feed,
live event loop, or cache prefetch path. Python remains only a strategy language adapter
and API compatibility layer.

## Non-Negotiables

- This is a single cutover, not a staged production migration.
- There must be one production runner for backtests and one production runner for live,
  both in `lean-engine`.
- The old Python-owned backtest and live runner paths must be removed or made
  non-production test fixtures in the same branch.
- No duplicate production data loops may remain after the cutover.
- No benchmark-specific fallback pipeline may remain.
- No local-history/provider/parquet orchestration may remain in `lean-python`.
- The GIL may be held only for Python API calls, Python strategy callbacks, Python
  universe/model callbacks, and Python object conversion.
- Rust data loading must use grouped, partition-aware query patterns by default.

## Target Ownership

### `lean-engine`

`lean-engine` owns:

- Backtest runner.
- Live runner.
- Warmup execution.
- Subscription lifecycle.
- Dynamic universe scheduling and application of `SecurityChanges`.
- Custom data and custom universe data scheduling.
- Data prefetch and local cache coverage checks.
- Local parquet reads and provider-backed history requests.
- Benchmark subscription and benchmark curve generation.
- Map file and factor file orchestration.
- Slice assembly.
- Fill-forward.
- Option chain runtime assembly.
- Margin interest and perpetual context loading.
- Order processing.
- Portfolio price updates.
- Brokerage synchronization and paper restore for live runs.
- Live data queue subscription reconciliation.
- Live deployment snapshots and result emission.

### `lean-python`

`lean-python` owns only:

- Loading Python strategy modules/classes.
- Exposing LEAN-compatible Python APIs.
- Holding shared algorithm state created by Python API calls.
- Implementing the engine callback bridge for Python strategies.
- Converting Rust engine data into Python-visible objects at callback boundaries.
- Calling Python strategy/framework/universe methods under the GIL.

### `lean-storage`

`lean-storage` owns:

- Parquet path resolution.
- Parquet read/write primitives.
- Grouped multi-partition market-data reads.
- Schema conversion.
- Predicate/projection-friendly storage access.

### `lean-data-providers`

`lean-data-providers` owns:

- Provider-specific history retrieval.
- Provider-specific batch requests.
- Provider-specific cache writes.
- Local provider implementation over `lean-storage`.

### `lean-live`

`lean-live` owns live-feed primitives only, unless folded into `lean-engine`:

- Data queue handler manager.
- Live data subscription primitives.
- Live data event primitives.
- Live slice assembler primitives if not moved directly into `lean-engine`.

It must not own the top-level live runner if `lean-engine` owns execution.

## New Engine Interfaces

Add an engine-owned strategy bridge trait in `lean-engine`, with Python implementing
the trait from `lean-python`.

```rust
pub trait AlgorithmBridge {
    fn initialize(&mut self, services: &mut AlgorithmServices) -> anyhow::Result<()>;
    fn on_data(&mut self, slice: &Slice, services: &mut AlgorithmServices) -> anyhow::Result<()>;
    fn on_order_event(
        &mut self,
        event: &OrderEvent,
        services: &mut AlgorithmServices,
    ) -> anyhow::Result<()>;
    fn on_end_of_day(
        &mut self,
        symbol: Option<&Symbol>,
        services: &mut AlgorithmServices,
    ) -> anyhow::Result<()>;
    fn on_warmup_finished(&mut self, services: &mut AlgorithmServices) -> anyhow::Result<()>;
    fn on_end_of_algorithm(&mut self, services: &mut AlgorithmServices) -> anyhow::Result<()>;
}
```

The bridge must not expose engine internals to Python. Python receives API-compatible
objects, while the engine owns state transitions.

Add `AlgorithmServices` in `lean-engine` as the controlled surface for:

- Subscription changes.
- History requests.
- Portfolio/order access.
- Charting/results.
- Framework state access.
- Runtime parameters.
- Security and universe registration.

## New Engine Modules

Create or rewrite these modules under `lean-engine`:

- `runner/backtest.rs`: complete backtest execution runner.
- `runner/live.rs`: complete live execution runner.
- `runner/common.rs`: shared runner state and callback plumbing.
- `data/feed.rs`: subscription data feed coordinator.
- `data/prefetch.rs`: provider-backed prefetch and cache coverage.
- `data/history.rs`: engine-owned history service.
- `data/warmup.rs`: warmup data loading and replay.
- `data/benchmark.rs`: internal benchmark subscription and curve generation.
- `data/factors.rs`: map/factor file orchestration.
- `data/custom.rs`: custom data and custom universe loading.
- `data/options.rs`: option universe and option chain runtime assembly.
- `live/subscriptions.rs`: live subscription reconciliation.
- `live/brokerage.rs`: brokerage synchronization and event bridge.
- `live/snapshots.rs`: paper restore and deployment snapshots.
- `results/writer.rs`: backtest/live result sidecar writing.

Module names can change during implementation, but the ownership split cannot.

## Single Cutover Work

All items below are completed in one branch before the production path changes hands.
Intermediate commits are fine; partial production routing is not.

1. Add `AlgorithmBridge`, `AlgorithmServices`, `BacktestRunConfig`,
   `LiveRunConfig`, `BacktestRunResult`, and `LiveRunResult` to `lean-engine`.

2. Implement `AlgorithmBridge` for `PyAlgorithmAdapter` in `lean-python`.
   The adapter handles Python calls only. It does not load data, assemble slices,
   prefetch history, or manage live subscriptions.

3. Move the current backtest execution behavior from `lean-python::runner::run_strategy`
   into `lean-engine::runner::backtest`.

4. Move the current live execution behavior from `lean-python::runner::run_live_strategy`
   into `lean-engine::runner::live`.

5. Move backtest and live warmup into `lean-engine`.
   Warmup data reads must use the same engine data feed/history path as normal runtime
   history reads.

6. Move startup data prefetch into `lean-engine`.
   Cache coverage checks must be grouped by partition and SID, not subscription by
   subscription row reads.

7. Move local parquet history access into `lean-engine` history services.
   Python `History()` calls route into engine history service through
   `AlgorithmServices`.

8. Move benchmark handling into `lean-engine`.
   Benchmarks are internal subscriptions through the same data feed as every other
   symbol. There is no benchmark-specific fallback loader.

9. Move factor/map file lifecycle into `lean-engine`.
   The engine ensures required auxiliary files before data consumption and applies
   normalization consistently.

10. Move custom data and custom universe data loading into `lean-engine`.
    Python can define the custom data classes and selectors; Rust owns scheduling,
    cache reads, and slice delivery.

11. Move scheduled universe timing into `lean-engine`.
    Python selectors are callback implementations only.

12. Move option runtime assembly into `lean-engine`.
    This includes option universe rows, held-contract retention, option chain
    construction, and underlying spot handling.

13. Move live data queue subscription reconciliation into `lean-engine`.
    The engine subscribes/unsubscribes live market, custom, and universe streams
    after applying subscription changes.

14. Move live brokerage synchronization, event processing, paper restore, and live
    deployment snapshots into `lean-engine`.

15. Move result writing and sidecar event/trade output into `lean-engine`, with
    Python chart/framework data exposed through the bridge when needed.

16. Update `rlean` CLI so backtest and live paths construct a Python bridge when the
    strategy is Python, then call `lean-engine` runners directly.

17. Delete production calls to `lean_python::runner::run_strategy` and
    `lean_python::runner::run_live_strategy`.

18. Delete or quarantine obsolete `lean-python` runner code in the same branch.
    Quarantined code must be test-only and must not be reachable from `rlean`.

19. Replace the old `lean-engine::BacktestEngine` and `DataManager` with the new
    runner/data-feed implementation, or delete them if fully superseded.

20. Ensure all data paths use grouped storage/provider access:
    - daily backtest preload
    - warmup
    - algorithm history
    - local history provider
    - QuantBook local history
    - benchmark
    - live warmup
    - live price seeding
    - cache coverage checks

## Rust/Python Boundary Rules

- Python strategy code is called only through `AlgorithmBridge`.
- Python API compatibility objects may mutate shared algorithm state, but engine
  runtime ownership remains in Rust.
- Engine state changes caused by Python API calls must be observed through explicit
  service/state snapshots, not by letting Python own the loop.
- Slice construction happens in Rust.
- Python `Slice` objects are views/proxies over Rust slices at callback time.
- Indicator updates triggered by Python indicators happen in the Python bridge, but
  timing is controlled by the engine.
- Universe selection timing is controlled by the engine; selector execution may call
  Python.

## Data Path Requirements

- Data reads are grouped by partition/date/resolution/tick type and filtered by SID.
- Multi-symbol requests use provider batch APIs when available.
- Local parquet reads use `lean-storage` grouped readers.
- Coverage checks inspect partition SID columns once per partition when possible.
- No path may loop `subscription x date` and read full rows just to determine cache
  coverage.
- No path may implement a special fallback that bypasses the engine data feed.

## Live Runner Requirements

The engine live runner must own:

- Live data queue setup.
- Initial brokerage sync.
- Paper restore.
- Live warmup.
- Live subscription registration.
- Custom live data subscription setup.
- Universe live data subscription setup.
- Frontier/slice assembly.
- Fill-forward.
- OnData delivery.
- Post-OnData order processing.
- Brokerage event reconciliation.
- Subscription reconciliation after universe changes.
- Snapshot persistence.
- Graceful shutdown and final result generation.

Python only receives callbacks and returns strategy decisions through API state.

## Backtest Runner Requirements

The engine backtest runner must own:

- Strategy initialization callback.
- Start/end date resolution.
- Warmup window calculation and replay.
- Initial universe selection.
- Data materialization.
- Data prefetch.
- Auxiliary data loading.
- Main daily/intraday loop.
- Dynamic subscription reconciliation.
- Custom data delivery.
- Option chain assembly.
- Portfolio and order lifecycle.
- Benchmark curve generation.
- Result/statistics generation.

Python only receives callbacks and invokes compatible API methods.

## Deletion List

Remove or make test-only:

- `lean-python` production backtest loop.
- `lean-python` production live loop.
- `lean-python` data prefetch code.
- `lean-python` warmup data replay code.
- `lean-python` benchmark loader code.
- `lean-python` local parquet orchestration.
- Old `lean-engine::DataManager` if superseded.
- Old `lean-engine::BacktestEngine` if superseded.
- Any duplicate live runner outside `lean-engine`.

## Validation

The cutover is complete only when:

- `rlean backtest` routes through `lean-engine`.
- `rlean live` routes through `lean-engine`.
- Python strategy support works through `PyAlgorithmAdapter` implementing
  `AlgorithmBridge`.
- There is no production runner loop in `lean-python`.
- There is no production data loop in `lean-python`.
- Existing backtest tests pass.
- Existing live tests pass.
- New tests cover Python strategy backtest execution through `lean-engine`.
- New tests cover Python strategy live execution through `lean-engine`.
- Tests cover warmup, history, benchmark, dynamic universe selection, custom data,
  options, and live restore through the new engine path.
- Search checks confirm no `rlean` production path calls `lean_python::runner`.

## Search Gates

Before merge, these checks must pass by inspection or automated test:

```text
rg "run_strategy|run_live_strategy" crates/rlean crates/lean-python crates/lean-engine
rg "read_trade_bar_partition\\(|read_quote_bar_partition\\(|read_tick_partition\\(" crates/lean-python/src
rg "pre_fetch|warmup|benchmark_price_map|materialize_subscription_range" crates/lean-python/src
```

Any remaining hits in `lean-python` must be bridge/API compatibility code or tests,
not production execution ownership.

## Final State

The final system has one engine. Python is one way to implement a strategy, not a
place where the engine runs.
