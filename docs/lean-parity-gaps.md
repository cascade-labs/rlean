# LEAN parity gap analysis

This document compares rlean `main` at
`0ab74a6b8b3c99356eb3ead743b4081e9f167230` with the sibling C# LEAN checkout
at `be0ad6cb7031980b09166e5451c2edd1f60261e0`.

The companion [regression-driven development specification](lean-regression-tdd.md)
defines the cross-SDK regression sections, mocked-provider fixtures, result
comparisons, and CI contract used to close these gaps.

The goal is strategy and behavior compatibility, not a line-for-line Rust port.
rlean's native provider architecture, native Rust `IAlgorithm` surface, artifact relay,
and deployment supervisor are intentional architectural differences. They are
not gaps unless they prevent a LEAN strategy from observing the same data,
orders, portfolio state, callbacks, or results.

## Executive summary

rlean has a useful equity/options backtest and live-trading core, but it is not
yet a general LEAN replacement. The largest gaps are not isolated missing
types. They are incomplete end-to-end paths:

1. Canonical data that has a table or Rust type but cannot reach a `Slice` or
   affect the portfolio.
2. Asset-class and model crates that are not wired into the engine or Python
   strategy API.
3. A much smaller Python `QCAlgorithm`, `History`, and `QuantBook` contract.
4. Simplified cash, settlement, buying-power, and position-group accounting.
5. No C# regression-algorithm compatibility harness to prove identical
   behavior and statistics.

The first implementation milestone should therefore be correctness plumbing
and a parity harness, followed by Python strategy portability. Adding more
standalone models before those paths exist will increase nominal coverage
without increasing the set of LEAN strategies that actually run correctly.

## Measurement

The repository's API audit was run against freshly generated rlean stubs. The
script also reads the published LEAN Python class reference. This supplements,
but does not replace, direct inspection of the local C# tree.

| Surface | Declared coverage |
|---|---:|
| Overall | 15.7% (143/912) |
| `QCAlgorithm` | 11.9% (43/362) |
| `QuantBook` | 0.0% (0/371) |
| Framework | 44.0% (11/25) |
| Universe | 28.6% (4/14) |
| Market data | 28.6% (4/14) |
| Orders | 61.5% (8/13) |
| Options | 29.6% (8/27) |
| Indicators | 56.2% (9/16) |
| Portfolio/security | 52.9% (9/17) |
| Scheduling | 33.3% (3/9) |

There are 17 detected signature mismatches. Important examples are
`add_equity`, `add_option`, `add_universe`, `history`, `market_order`,
`stop_market_order`, indicator factories, and `warm_up_indicator`.

These figures are a lower bound on runtime behavior because stub generation is
not faithful. For example, rlean has a functional basic `QuantBook` in
`crates/rlean-sdk/src/research.rs`, while the generated stub declares an empty
`QuantBook`. That is still a compatibility defect: strategies, IDEs, static
analysis, and the audit cannot depend on an undocumented runtime surface.

The scale difference is also visible in test assets. rlean currently has 45
Rust test source files across its crates. The sibling LEAN tree has about 802
C# and Python regression-algorithm files, in addition to its unit tests. The
numbers are not directly comparable, but rlean lacks an equivalent
algorithm-output regression suite.

## What is already aligned

The following areas are sufficiently real that new work should extend them,
not replace them:

- Persistent provider streams, bounded backtest queries, pushed live batches,
  canonical Arrow contracts, factor/map files, and provider-neutral venue
  metadata.
- Equity trade/quote/tick subscriptions, custom data, point-in-time fundamental
  snapshots, and option-chain delivery.
- Equity normalization and the option-underlying raw-price invariant.
- Market, limit, stop-market, stop-limit, MOO/MOC, trailing-stop, combo, and
  option-exercise order types in the Rust core, with asset-specific fill models.
- Live brokerage submission and pushed status/fill reconciliation, including
  cumulative brokerage fee deltas and fee-model fallback.
- Option exercise/assignment and expiry handling, including LEAN-compatible OTM
  expiry callbacks.
- Framework execution with built-in and Python alpha, portfolio construction,
  execution, and risk adapters.
- Warm-up, scheduling, `SecurityChanges`, margin-call processing, and common
  lifecycle callbacks.
- Core portfolio/trade statistics, benchmark alignment, and dated risk-free
  rates used by statistics and option valuation.

## P0: correctness and parity infrastructure

### 1. Build a C# regression parity harness

**C# reference:** `../Lean/Algorithm.CSharp`, `../Lean/Algorithm.Python`, and the
expected-statistics contract implemented by LEAN regression algorithms.

**Current rlean state:** Tests validate individual Rust components and a small
number of Python bridges. There is no runner that selects compatible LEAN
regression algorithms, runs equivalent rlean strategies over the same data, and
compares orders, fills, holdings, callbacks, equity, and final statistics.

**Implement:**

- Define a manifest for ported regression cases containing source LEAN test,
  required data, parameters, expected event trace, and expected statistics.
- Start with equity market/limit/stop fills, normalization/splits, warm-up,
  scheduling, universe changes, options expiry/assignment, and framework
  rebalancing.
- Compare structured artifacts, not console text: subscriptions, slice
  frontiers, order events, trades, holdings, cash, equity, and statistics.
- Add every parity bug as a permanent regression case.

**Acceptance:** A deterministic CI suite proves the selected cases match LEAN
at field level, with an explicit allowlist for intentional differences.

### 2. Complete the canonical data-to-slice pipeline

**C# reference:** `../Lean/Common/Data/Slice.cs`,
`../Lean/Engine/DataFeeds`, and `../Lean/Engine/AlgorithmManager.cs`.

**Current rlean state:** `SubscriptionDataPoint` carries trade bars, quote bars,
ticks, custom data, fundamental snapshots, and option chains. The canonical data contract
enum also names open interest and margin interest, while `Slice` has Rust
containers for splits, dividends, delistings, symbol changes, and margin
interest. Those pieces are not connected end to end.

**Implement:**

- Decode and emit `OpenInterest` and `MarginInterestRate` as subscription data.
- Apply margin-interest/funding events to the portfolio on LEAN's cadence. This
  is the remaining work in issue #79; the dated risk-free model in #102 is a
  separate rate and does not change cash.
- Add provider-neutral split, dividend, delisting, and symbol-change contracts
  to `rlean-data-tables` and the native provider interfaces.
- Generate and synchronize those events into `Slice`, then invoke the existing
  algorithm callbacks and order/holding adjustment paths.
- Define live delivery for each event rather than supporting backtests only.

**Acceptance:** Each data type is proven through
provider batch -> decoder -> subscription frontier -> `Slice` -> callback and,
where applicable, portfolio/order mutation.

### 3. Replace scalar cash accounting with LEAN-compatible cash and settlement

**C# reference:** `SecurityPortfolioManager.CashBook`,
`UnsettledCashBook`, currency conversion, and per-security settlement models.

**Current rlean state:** `SecurityPortfolioManager` owns one scalar cash value.
`unsettled_cash` is explicitly a parity stub that always remains zero. There is
no multi-currency cash book, conversion subscription graph, delayed settlement,
or settlement scan.

**Implement:**

- Account-currency-aware `CashBook` and `UnsettledCashBook` equivalents.
- Direct and triangulated currency conversion subscriptions and stale-rate
  handling.
- Settlement models for equities/options and immediate-settlement assets.
- Correct fee, dividend, exercise, assignment, and funding currency conversion.
- Python portfolio/cash-book views matching LEAN names and behavior.

**Acceptance:** Port LEAN regression cases for non-USD accounts, two-leg FX
conversion, unsettled equity cash, and option settlement with identical cash
and buying power at every frontier.

### 4. Wire security models and asset classes into the runtime

**C# reference:** the typed `Security` subclasses and `AddSecurity` overloads in
`../Lean/Algorithm/QCAlgorithm.cs`.

**Current rlean state:** Equity, forex, crypto, crypto-future, and equity-option
subscription helpers exist in the Rust algorithm. Python exposes only
`add_equity` and `add_option`. The `rlean-forex`, `rlean-futures`, and
`rlean-crypto` crates are not dependencies of the engine or SDK, so most of
their models are library islands. Index, index-option, CFD, future,
future-option, and contract-specific adders are absent from the strategy path.

**Implement:**

- Make `add_security` the tested internal path for every supported
  `SecurityType`, then expose LEAN-compatible typed adders.
- Wire forex, crypto, crypto-future, future, CFD, index, index-option, and
  future-option models into security creation, history, fills, buying power,
  fees, settlement, and live restoration.
- Add future canonical/contract chains, mapping modes, normalization modes,
  depth offsets, rollover events, and symbol-change callbacks.
- Return typed security views rather than a generic or `None` result where LEAN
  returns a security/universe object.

**Acceptance:** One C# regression family per asset class passes through both
history and algorithm execution; supported adders have matching Python
signatures and return types.

### 5. Complete buying power, position groups, and margin behavior

**C# reference:** security and position-group buying-power models,
`SecurityPortfolioManager`, and `DefaultMarginCallModel`.

**Current rlean state:** Cash, generic security-margin, and crypto-future
buying-power calculations exist, along with basic margin calls. LEAN's position
groups, option-strategy margin offsets, shortable providers, borrow costs,
security-specific leverage rules, and full live margin behavior are absent or
simplified. Live margin calls are intentionally disabled.

**Implement:**

- Position-group resolution and group buying-power models, beginning with
  covered options, vertical spreads, and futures options.
- Brokerage/security-specific initial and maintenance margin.
- Shortable quantity, borrow availability, and borrow/interest costs.
- Live margin warnings and execution policy with brokerage reconciliation.
- Portfolio APIs for buying power, margin used/remaining, and order sizing.

**Acceptance:** LEAN option-strategy margin and margin-call regression cases
match order quantity, reserved buying power, warnings, and liquidation orders.

## P1: strategy portability

### 6. Make the Python SDK and stubs faithful

**Current rlean state:** The runtime contains more behavior than the generated
stub, but many Rust methods are not Python methods. The audit found 17 signature
mismatches. The audit command itself currently fails because it runs
`cargo run -p rlean` after `rleand` introduced a second binary.

**Implement:**

- Generate stubs from the complete registered PyO3 module and fail CI when a
  runtime method or property is absent.
- Fix the audit command to select `--bin rlean` and add it to CI with a committed
  baseline and no-regression gate.
- Expose existing Rust helpers before reimplementing them: `buy`, `sell`,
  MOO/MOC, stop-limit, trailing-stop, combo orders, option open/close helpers,
  and typed asset adders.
- Match optional arguments, return types, snake_case names, PascalCase aliases,
  enums, collection behavior, and overload dispatch.
- Triage the 98 generated local-only names into intentional extensions or naming
  drift.

**Acceptance:** Generated stubs reflect runtime introspection exactly; API
coverage cannot regress; representative unmodified LEAN Python strategies
import and initialize without compatibility shims.

### 7. Expand `History` and `QuantBook`

**C# reference:** `../Lean/Research/QuantBook.cs`, `OptionHistory.cs`, and
`FutureHistory.cs`.

**Current rlean state:** Algorithm history is essentially one symbol plus bar
count and resolution. Research supports basic trade-bar history/ranges and a
small indicator interface. It lacks LEAN's multi-symbol and typed overloads,
`Slice` history, quote/tick/custom history, normalization/mapping controls,
option/future history, fundamental history, and portfolio-statistics helpers.

**Implement:**

- One internal history-request model covering symbols, types, counts, spans,
  date ranges, fill forward, extended hours, mapping, normalization, depth, and
  flattening.
- Python return shapes compatible with LEAN pandas conventions and typed
  enumerables.
- `QuantBook` security adders plus option history, future history, fundamental
  queries, indicator history, and portfolio statistics.
- Provider query support for every canonical type required by those requests.

**Acceptance:** Port LEAN's basic and kitchen-sink QuantBook notebooks and a
matrix of History regression algorithms.

### 8. Finish universe selection

**C# reference:** `UniverseSelection`, fundamental universes, ETF constituents,
scheduled universes, option universes, and future universes.

**Current rlean state:** Manual/custom membership changes and a daily
fundamental selector run end to end. The Python fundamental row exposes only
`symbol`, `volume`, `dollar_volume`, and `market_cap`. ETF, future, option, and
several fundamental universe models exist as table contracts or Rust models but
are not wired to strategy selection.

**Implement:**

- Complete the fundamental contract: price, `has_fundamental_data`, factors,
  shares, sector/SIC, filing dates, valuation ratios, earnings, and financial
  statements with point-in-time guarantees.
- Return a `Universe` handle from `add_universe` and honor per-universe settings.
- Wire scheduled, ETF-constituent, option, and future universe tables through
  selectors and subscription diffs.
- Support asynchronous selection semantics, minimum time in universe, and
  removal behavior matching LEAN.
- Deliver framework `OnSecuritiesChanged` to every Python model; the current
  Python alpha adapter ignores it.

**Acceptance:** LEAN coarse/fine-equivalent, ETF constituent, scheduled,
option-filter, and continuous-future universe regressions pass without custom
rlean APIs.

### 9. Complete derivatives behavior

**Current rlean state:** Equity option chains, Greeks, IV calculation, spread
fills, exercise, assignment, and expiry have meaningful implementations.
`OptionUniverseRow` already contains OHLCV, open interest, IV, and Greeks, and
filter helpers exist, but the engine does not consume that table for universe
selection. Index options, futures, and futures options are not complete runtime
paths.

**Implement:**

- Consume option-universe snapshots and expose LEAN filters for delta, gamma,
  vega, theta, rho, IV, and open interest before contract subscriptions.
- Add option-chain provider/history APIs and contract add/remove/get-chain APIs.
- Implement index-option and future-option security creation, settlement,
  exercise, assignment, multipliers, market hours, and margin.
- Complete continuous futures, roll mapping, contract depth, and futures chain
  delivery.
- Validate American/European exercise and cash/physical settlement across asset
  classes.

**Acceptance:** Issue #78 is closed by an end-to-end option-universe path, and
selected LEAN equity-option, index-option, future, and future-option regressions
match event and portfolio outputs.

### 10. Connect consolidators, scheduling, and indicator factories

**Current rlean state:** `rlean-consolidators` has multiple implementations but
is not a runtime dependency. Scheduling works for a narrow rule set. rlean has
roughly 73 indicator modules, while only nine main indicator classes are
registered in Python and only a handful of QCAlgorithm factory methods exist.

**Implement:**

- Wire subscription-manager consolidators to automatic updates and callbacks.
- Expose trade/quote/tick, calendar, Renko, range, volume, and Heikin-Ashi
  consolidators with LEAN registration/removal behavior.
- Complete `DateRules` and `TimeRules` overloads, exchange-aware rules, and
  scheduled-event cancellation/error behavior.
- Expose existing indicators and QCAlgorithm factories before porting the
  remaining C# indicator library; support selectors and automatic warm-up.

**Acceptance:** Consolidator, scheduled-event, indicator-suite, and automatic
indicator warm-up regression families pass.

### 11. Finish framework model semantics

**Current rlean state:** Python adapters call `Update`, `CreateTargets`,
`Execute`, and `ManageRisk`, and built-in models are available. Callback and
base-class surfaces remain incomplete, insight/target projections are reduced,
and not all model settings/rebalance functions match LEAN.

**Implement:**

- Full `OnSecuritiesChanged` delivery to alpha, portfolio construction,
  execution, and risk models.
- LEAN-compatible insight properties, scoring, cancellation, source-model
  identity, grouping, and target-insight selection.
- Rebalance delegates for time, calendar, security changes, and insight changes.
- Composite models and the remaining standard framework models only after base
  semantics are locked by regression tests.

**Acceptance:** Framework regression algorithms produce identical insights,
targets, order sequences, and statistics.

## P2: completeness and operational parity

### 12. Add `ObjectStore`, commands, notifications, and complete chart APIs

rlean has chart/result infrastructure, but the Python surface does not match
LEAN's chart/series configuration and update behavior. There is no
strategy-visible `ObjectStore` equivalent, command channel, notification
surface, training scheduler, or signal export contract.

Implement these as engine services so Python and native Rust strategies share
one behavior. Object-store persistence must be session/deployment scoped and
must not bypass the provider and Verglas ownership of market data.

### 13. Close results and statistics gaps

rlean calculates the main return, drawdown, trade, benchmark, and risk-adjusted
statistics. Remaining LEAN gaps include estimated strategy capacity and lowest
capacity asset, drawdown recovery duration, more complete trade MAE/MFE and
duration statistics, runtime statistic mutation, chart sampling parity, and
result packet/API shapes.

Add golden comparisons against LEAN before changing formulas. Statistics that
are intentionally rlean-specific should be namespaced rather than replacing a
LEAN metric with different semantics.

### 14. Integrate optimization and research jobs

`rlean-optimization` is not connected to the CLI/engine, and the research daemon
is an NDJSON Python execution kernel rather than LEAN's full QuantBook research
surface or the agent-oriented job system described in issue #21.

After History/QuantBook parity:

- Connect grid, random, and walk-forward optimization to normal engine runs and
  artifact manifests.
- Add resumable job ids, status, cancellation, reproducibility metadata, and
  result comparison.
- Keep research data access on the public SDK/provider path; raw Python execution
  should not become a second engine contract.

### 15. Finish remote artifact reads

S3 artifact writing exists, but remote-only runs cannot be listed, pulled,
resumed, or read transparently. This is not a C# engine-parity blocker, but it is
required for rlean's own cloud operating model and corresponds to issues
#43-#45.

## Recommended implementation order

1. Regression parity harness.
2. Margin interest plus corporate-action slice delivery.
3. CashBook, FX conversion, and settlement.
4. Python stub fidelity and exposure of already-implemented Rust operations.
5. Unified History/QuantBook request path.
6. Typed asset adders and futures/index/CFD runtime wiring.
7. Fundamental, ETF, option, and future universes.
8. Position-group buying power and derivatives completion.
9. Consolidators, scheduling, framework callbacks, and indicator factories.
10. ObjectStore, results completeness, optimization/research jobs, and remote
    artifact reads.

This order deliberately favors observable correctness and strategy portability
over raw type or model count.

## Audit maintenance

- Pin both repository SHAs whenever this document is refreshed.
- Run `scripts/qc_algorithm_api_coverage.py` in CI after fixing its explicit
  binary selection.
- Track implemented, stub-only, intentionally different, and unsupported APIs
  separately.
- Require every closed parity gap to cite a LEAN source behavior and add a
  regression comparison.
- Re-audit library islands periodically: a crate or model is not complete until
  the engine and strategy SDK can exercise it end to end.
