# LEAN regression-driven development specification

This document defines how rlean should consume the C# LEAN regression corpus as
an executable specification for both the native Rust SDK and the Python SDK.
It complements [lean-parity-gaps.md](lean-parity-gaps.md): that document says
what is missing; this document defines the regression sections and test
infrastructure used to implement it.

The governing rule is that a feature is not complete because a Rust type or
Python method exists. It is complete when the same regression behavior passes
through both public SDKs, using identical canonical data, and both results match
the pinned LEAN oracle.

## Test contract

Every regression case has two conformance runs against one normalized LEAN
oracle:

1. rlean Rust SDK output versus the LEAN golden result.
2. rlean Python SDK output versus the same LEAN golden result.

If both runs match the oracle, Rust/Python equivalence follows transitively. A
direct Rust-versus-Python diff may be printed to help localize a failure, but it
is diagnostic output, not a third assertion or acceptance criterion.

Tests must use public strategy APIs only:

- Rust cases implement `IAlgorithm` and use types exported by `rlean-sdk`.
- Python cases subclass `QCAlgorithm` and import from `AlgorithmImports`.
- Neither case may reach into `rlean-engine`, construct slices directly, mutate
  the portfolio behind the algorithm, or bypass the provider interface.

When LEAN ships a Python version of a regression algorithm, use that source
unchanged except for a preserved license header and any packaging needed to
make sibling helper modules importable. The Rust case is a direct behavioral
translation. When LEAN has only a C# case, create both a minimal Rust port and a
minimal Python port and retain the C# source path in the manifest.

## Upstream catalog

LEAN discovers regressions through
`IRegressionAlgorithmDefinition`. A small .NET exporter should reflect over the
same assembly and write a pinned catalog containing:

- Algorithm name and source paths.
- `CanRunLocally`.
- Supported languages.
- Expected final status.
- Total data-point count.
- Algorithm history data-point count.
- Expected formatted statistics.
- `OrderListHash`, when present.
- LEAN repository commit.

Do not parse expected-statistics dictionaries from source text. Reflection is
required because definitions may inherit or compute their metadata.

The checked-in catalog is the default CI oracle. A nightly differential job may
also run the pinned LEAN checkout and refresh detailed traces, but ordinary
rlean tests must not require a sibling LEAN build.

## Repository layout

```text
tests/lean-regressions/
  catalog.json
  cases/
    basic-template/
      case.toml
      strategy.rs
      main.py
      fixture.toml
      expected.json
    ...
  fixtures/
    micro/
    imported-lean/
  schemas/
    regression-result.schema.json
    regression-trace.schema.json

crates/rlean-data-provider-test-support/
  src/
    server.rs
    scenario.rs
    row_builders.rs
    brokerage.rs
    faults.rs

tools/lean-regression-exporter/
tools/lean-data-fixture-importer/
scripts/run-lean-regression
```

The test-support crate must not be a production dependency. Engine integration
tests and SDK regression cases may use it as a dev-dependency.

## Case manifest

Each case declares one source of truth for both SDKs:

```toml
id = "basic-template"
section = "bootstrap"
lean_commit = "be0ad6cb7031980b09166e5451c2edd1f60261e0"
lean_csharp = "Algorithm.CSharp/BasicTemplateAlgorithm.cs"
lean_python = "Algorithm.Python/BasicTemplateAlgorithm.py"
rust_strategy = "strategy.rs"
python_strategy = "main.py"
fixture = "../../fixtures/imported-lean/basic-template"

expected_status = "Completed"
expected_data_points = 3943
expected_history_data_points = 0

compare = [
  "subscriptions",
  "callbacks",
  "orders",
  "order_events",
  "cash",
  "holdings",
  "equity",
  "statistics"
]

[tolerances]
# Tolerances require a documented reason. Quantities, fees, and event ordering
# are exact unless the case explicitly proves a cross-platform float boundary.
statistics_decimal = "0"
```

Unsupported cases remain in the catalog with an explicit state and gap id. They
are not silently skipped:

```toml
state = "expected_gap"
highest_passing_stage = "initialize"
gap = "missing_add_future"
issue = 123
```

## Mocked providers

Regression fixtures implement the production `HistoricalDataProvider`,
`LiveDataProvider`, `HistoricalDataStore`, and `Brokerage` interfaces. The
fixtures replace vendors and persistence while exercising the same request,
cache coverage, subscription, synchronization, and brokerage paths used by the
engine.

Implement reusable recording providers in `rlean-data-provider-test-support`.
Every unexpected request fails with a structural diff so subscription intent,
query boundaries, resolution, tick type, normalization, venue, and custom query
predicates remain independently testable.

### Fixture model

A scenario contains:

```rust,ignore
pub struct ProviderScenario {
    pub expected_subscriptions: Vec<ExpectedSubscription>,
    pub backtest_batches: Vec<ScheduledBatch>,
    pub live_batches: Vec<ScheduledBatch>,
    pub auxiliary_batches: Vec<ScheduledBatch>,
    pub brokerage: Option<BrokerageScenario>,
    pub faults: Vec<FaultInjection>,
    pub chunking: ChunkingPolicy,
}
```

`ScheduledBatch` carries a canonical `TableContract` record batch plus its
subscription identity, query range, and release frontier. Record batches must
be built with `rlean-data-tables` schemas; tests may not invent a second schema.

### Fixture sources

Use three fixture types:

1. **Micro fixtures:** Hand-authored rows for one behavior, such as a split, a
   partial fill, or a two-symbol universe change.
2. **Imported LEAN fixtures:** A converter reads the files under the pinned
   LEAN `Data/` tree and writes compact Arrow IPC fixtures plus a provenance
   manifest. This is used for upstream regression algorithms.
3. **Generated fixtures:** Seeded generators produce larger deterministic
   chains, universes, or tick streams for property and chunking tests.

Imported fixtures must preserve raw values, exchange-local dates, time zones,
factor/map rows, option/future identifiers, and the exact absence of data.
Missing rows are part of the oracle and must not be filled by the importer.

### Deterministic clock

The mock owns a virtual clock. Backtest batches are released only in response
to matching bounded queries. Live batches and brokerage events are released at
scripted clock steps controlled by the test. No regression test depends on wall
time, sleeps, or network scheduling.

### Chunking matrix

Every canonical-data case can be rerun under:

- One record batch.
- One row per batch.
- Fixed-size batches.
- Schema plus empty leading batches.
- Empty middle ranges.
- Reordered arrival across independent subscriptions while preserving each
  subscription's order.

Results must be invariant. Chunking is a transport property, not an algorithm
input.

### Brokerage scripting

Brokerage scenarios define initial cash, holdings, open orders, command
responses, and pushed updates. They support:

- Accept, reject, and retryable transport failure.
- Partial and cumulative fills.
- Cumulative fees and fee currencies.
- Duplicate and out-of-order updates.
- Disconnect/reconnect.
- Late acknowledgement after a timeout.
- Engine restart from a snapshot.

The mock records every command so the comparator can assert submission,
update, cancellation, and idempotency keys.

## Result and trace contract

rlean already writes LEAN-shaped results, orders, and order events. Regression
mode adds a stable machine-readable trace with:

- Algorithm status and error.
- Subscription add/remove requests.
- Backtest query ranges and returned point counts.
- Slice frontier, data-type counts, and symbol set.
- Lifecycle callback name and order.
- `SecurityChanges`.
- History requests and point counts.
- Orders, order responses, and order events.
- Cash by currency, unsettled cash, holdings, fees, and funding.
- Daily equity and benchmark samples.
- Final statistics and order-list hash.

The comparator normalizes representation only: UTC timestamp formatting,
decimal string formatting, enum casing, stable map order, and LEAN's `-0` versus
`0` rule. It does not hide differences in event order, quantities, prices,
fees, cash, or symbol identity.

## Regression sections

Every section below requires a Rust strategy case and a Python strategy case.
The representative LEAN names seed the catalog; the exporter determines the
complete current list at the pinned commit.

### 1. Bootstrap, imports, configuration, and parameters

**Representative LEAN algorithms:** `BasicTemplateAlgorithm`,
`GetParameterRegressionAlgorithm`, `NamedArgumentsRegression`,
`QuitAfterInitializationRegressionAlgorithm`, and
`QuitInInitializationRegressionAlgorithm`.

**Rust SDK:** Algorithm construction, configuration setters, parameters,
logging, controlled quit/status, and initialization errors.

**Python SDK:** `AlgorithmImports` exports, snake_case and PascalCase aliases,
named arguments, overload resolution, enums, `get_parameter`, and lifecycle
method discovery.

**Mock providers:** Validate session initialization and that no subscriptions or
queries occur before the strategy requests them. Provide a minimal SPY stream
for the basic template.

**Assertions:** Import success, initialization call count, parameters, dates,
cash, requested subscriptions, final status, error classification, and logs.

### 2. Lifecycle, clock, market hours, and callbacks

**Representative LEAN algorithms:** `OnEndOfDayRegressionAlgorithm`,
`OnWarmupFinishedNoWarmup`, `OnWarmupFinishedRegressionAlgorithm`,
`DefaultSchedulingSymbolRegressionAlgorithm`, and
`ExtendedMarketTradingRegressionAlgorithm`.

**Rust SDK:** Complete lifecycle callbacks, algorithm/exchange time, UTC time,
end-of-day per symbol, end-of-time-step, and deterministic exception behavior.

**Python SDK:** Optional callback discovery, correct callback argument types,
snake_case/PascalCase behavior, and exception propagation.

**Mock providers:** Session-boundary bars across holidays, early closes, daylight
saving changes, extended hours, and empty leading windows.

**Assertions:** Exact callback ordering and timestamps, no duplicate EOD,
correct exchange-local dates, and the LEAN-expected exception state from each
SDK.

### 3. Symbols, securities, subscriptions, and cache state

**Representative LEAN algorithms:** `AddRemoveSecurityRegressionAlgorithm`,
`SecurityToSymbolRegressionAlgorithm`,
`StringToSymbolImplicitConversionRegressionAlgorithm`,
`SecuritySeederRegressionAlgorithm`, and `DynamicSecurityDataRegressionAlgorithm`.

**Rust SDK:** Typed symbols, SIDs, security manager, add/remove security,
initializers, seeding, subscription settings, and last-known data.

**Python SDK:** String-to-symbol conversions where LEAN permits them, typed
security return objects, `securities`, `active_securities`, and collection
semantics.

**Mock providers:** Assert exact `SubscriptionSpec` creation/removal and return
seed/history rows for newly created securities.

**Assertions:** Symbol identity survives the wire, duplicate adds are
idempotent, removed securities stop consuming data, and both SDKs expose the
same security state.

### 4. Slice composition and market-data semantics

**Representative LEAN algorithms:** `BasicTemplateDailyAlgorithm`,
`BasicTemplateFillForwardAlgorithm`, `RawDataRegressionAlgorithm`,
`SliceGetByTypeRegressionAlgorithm`, and
`PandasDataFrameFromMultipleTickTypeTickHistoryRegressionAlgorithm`.

**Rust SDK:** Trade bars, quote bars, ticks, open interest, typed slice access,
fill-forward metadata, and raw/adjusted values.

**Python SDK:** Dictionary iteration/indexing, `Slice.get`, typed collections,
bar properties, quote bid/ask nesting, and pandas conversions where applicable.

**Mock providers:** Trade, quote, tick, and open-interest batches with missing
symbols, simultaneous data types, empty ranges, and multiple venues.

**Assertions:** Frontier coalescing, `has_data`, collection contents, iteration
order policy, fill-forward behavior, and no loss across chunk boundaries.

### 5. History and warm-up

**Representative LEAN algorithms:** `HistoryAlgorithm`,
`HistoryTickRegressionAlgorithm`, `WarmupAlgorithm`, `WarmupHistoryAlgorithm`,
`HistoryWithDifferentDataNormalizationModeRegressionAlgorithm`,
`HistoryWithDifferentDataMappingModeRegressionAlgorithm`, and
`PeriodBasedHistoryRequestNotAllowedWithTickResolutionRegressionAlgorithm`.

**Rust SDK:** Unified history requests for counts, spans, and date ranges;
symbols and types; mapping/normalization; fill forward; extended hours; and
history errors.

**Python SDK:** LEAN overloads, typed enumerable and pandas return shapes,
multi-symbol history, `is_warming_up`, and automatic/manual indicator warm-up.

**Mock providers:** Validate bounded query ranges and serve trade, quote, tick,
custom, universe, option, and future history from deterministic fixtures.

**Assertions:** Exact requests, history point counts, ordering, data shape,
warm-up frontier, suppression of trading during warm-up, and callback timing.

### 6. Corporate actions, mapping, and auxiliary data

**Representative LEAN algorithms:** `HourSplitRegressionAlgorithm`,
`HourReverseSplitRegressionAlgorithm`, `DividendAlgorithm`,
`DelistingEventsAlgorithm`, `AuxiliaryDataHandlersRegressionAlgorithm`,
`HistoryAuxiliaryDataRegressionAlgorithm`, and `OptionRenameRegressionAlgorithm`.

**Rust SDK:** Split, dividend, delisting, and symbol-change types and callbacks;
factor/map resolution; holding and open-order adjustment.

**Python SDK:** Typed slice collections and callback dictionaries with LEAN
property names and symbol behavior.

**Mock providers:** Add canonical auxiliary contracts and emit factor/map rows and
events at exact availability frontiers.

**Assertions:** Adjusted data, raw holdings, cash distributions, order changes,
mapped symbols, callback order, and trade-builder continuity.

### 7. Indicators and consolidators

**Representative LEAN algorithms:** `RegisterIndicatorRegressionAlgorithm`,
`UnregisterIndicatorRegressionAlgorithm`, `IndicatorHistoryRegressionAlgorithm`,
`StochasticIndicatorWarmsUpProperlyRegressionAlgorithm`,
`ConsolidateRegressionAlgorithm`, `ConsolidateHourBarsIntoDailyBarsRegressionAlgorithm`,
`ClassicRenkoConsolidatorAlgorithm`, and `VolumeRenkoConsolidatorAlgorithm`.

**Rust SDK:** Indicator traits, automatic registration/update, selectors,
warm-up, reset, composition, and all consolidator families.

**Python SDK:** Indicator constructors/factories, `current`, readiness, update
events, consolidator callbacks, selector overloads, and deregistration.

**Mock providers:** Fine-grained bars/ticks across calendar boundaries, sparse
updates, multiple tick types, and out-of-session data.

**Assertions:** Sample counts, readiness frontier, exact indicator values,
consolidated OHLCV/timestamps, callback order, and no updates after removal.

### 8. Orders, tickets, fills, fees, and slippage

**Representative LEAN algorithms:** `SetHoldingsRegressionAlgorithm`,
`LimitFillRegressionAlgorithm`, `StopLimitOrderRegressionAlgorithm`,
`TrailingStopOrderRegressionAlgorithm`, `LimitIfTouchedRegressionAlgorithm`,
`UpdateOrderRegressionAlgorithm`, `CanLiquidateWithOrderPropertiesRegressionAlgorithm`,
`CustomPartialFillModelAlgorithm`, and `MarketImpactSlippageModelRegressionAlgorithm`.

**Rust SDK:** Every order type, order properties, tickets, responses, updates,
cancellations, fill/slippage/fee models, asynchronous submission, and combo
groups.

**Python SDK:** LEAN method signatures, optional tag/asynchronous/properties
arguments, ticket properties and methods, update fields, response errors, and
order-event views.

**Mock providers:** Backtest quote/trade paths for local fills plus brokerage
scripts for accepted/rejected/live orders, partial fills, fees, and retries.

**Assertions:** Submission fields, fill side and price, event sequence, status,
quantity, fee, ticket state, buying-power response, and order-list hash.

### 9. Portfolio, cash, settlement, buying power, and margin

**Representative LEAN algorithms:** `FractionalQuantityRegressionAlgorithm`,
`TwoLegCurrencyConversionRegressionAlgorithm`, `CustomSettlementModelRegressionAlgorithm`,
`NullMarginMultipleOrdersRegressionAlgorithm`, `MarginCallEventsAlgorithm`,
`ShortInterestFeeRegressionAlgorithm`, and
`ShortableProviderOrdersRejectedRegressionAlgorithm`.

**Rust SDK:** CashBook, unsettled cash, currency conversion, holdings, security
and position-group buying power, settlement, leverage, margin calls, shortable
providers, and borrow costs.

**Python SDK:** Portfolio/cash collections, holdings access, account currency,
margin APIs, security model setters, and margin/shortability callbacks.

**Mock providers:** Multiple currency pairs, delayed settlement events, borrow
availability/fees, margin-rate changes, and brokerage account snapshots.

**Assertions:** Cash by currency, conversion graph, settled/unsettled balances,
TPV, margin used/remaining, rejected quantities, margin-call orders, and fees.

### 10. Universe selection

**Representative LEAN algorithms:** `UniverseSelectedRegressionAlgorithm`,
`UniverseUnchangedRegressionAlgorithm`, `ScheduledUniverseRegressionAlgorithm`,
`AsynchronousUniverseRegressionAlgorithm`, `CustomDataUniverseRegressionAlgorithm`,
`CoarseFineFundamentalRegressionAlgorithm`,
`ETFConstituentUniverseFrameworkRegressionAlgorithm`, and
`WeeklyUniverseSelectionRegressionAlgorithm`.

**Rust SDK:** Manual, scheduled, custom, fundamental, ETF constituent, option,
and future universes; per-universe settings; minimum time; async selection;
membership diffs.

**Python SDK:** `add_universe` overloads and return handle, selector row views,
`UniverseSettings`, `SecurityChanges`, and framework selection models.

**Mock providers:** Whole cross-section batches with availability times, filings,
ETF compositions, empty selections, unchanged selections, and shuffled Arrow
batch boundaries.

**Assertions:** Selector inputs, trigger time, no look-ahead, membership,
subscription changes, initialization, removals, and SDK-equivalent callbacks.

### 11. Algorithm Framework

**Representative LEAN algorithms:** `BasicTemplateFrameworkAlgorithm`,
`EmaCrossAlphaModelFrameworkRegressionAlgorithm`,
`InsightScoringRegressionAlgorithm`, `InsightWeightingFrameworkAlgorithm`,
`PortfolioRebalanceOnDateRulesRegressionAlgorithm`,
`ExecutionModelOrderEventsRegressionAlgorithm`, and
`CompositeRiskManagementModelFrameworkAlgorithm`.

**Rust SDK:** Alpha, insight collection/scoring, portfolio construction,
optimizers, risk, execution, composites, rebalance policies, and security-change
callbacks.

**Python SDK:** Subclass adapters and built-in models with complete
`Update`/`CreateTargets`/`ManageRisk`/`Execute`/`OnSecuritiesChanged` contracts.

**Mock providers:** Multi-symbol price streams, universe changes, gaps, and
volatility regimes that drive deterministic insights and rebalances.

**Assertions:** Trace insights -> targets -> risk adjustments -> orders,
including expiry, scores, tags, rebalance causes, and model callback order.

### 12. Equities

**Representative LEAN algorithms:** `BasicTemplateAlgorithm`,
`ExtendedMarketTradingRegressionAlgorithm`, `HourSplitRegressionAlgorithm`,
`FractionalQuantityRegressionAlgorithm`, and `SecuritySessionRegressionAlgorithm`.

**Rust SDK:** Equity creation, exchange hours, normalization, lot size, fills,
fees, settlement, corporate actions, and sessions.

**Python SDK:** `add_equity` overloads, typed `Equity`/`Security` view, market
hours, normalization setters, and session properties.

**Mock providers:** Raw trade/quote/tick data, factors/maps, sessions, and extended
hours.

**Assertions:** All shared engine assertions plus equity-specific normalization,
lot rounding, settlement, and market-hour behavior.

### 13. Equity options

**Representative LEAN algorithms:** `BasicTemplateOptionsAlgorithm`,
`OptionChainConsistencyRegressionAlgorithm`, `OptionOpenInterestRegressionAlgorithm`,
`OptionUniverseFilterGreeksRegressionAlgorithm`,
`AddOptionContractFromUniverseRegressionAlgorithm`,
`OptionAssignmentRegressionAlgorithm`, `OptionExerciseAssignRegressionAlgorithm`,
`OptionSplitRegressionAlgorithm`, and `ComboOrdersFillModelAlgorithm`.

**Rust SDK:** Canonical and contract subscriptions, chain provider, universe
filters, price/IV/Greeks models, margin, exercise, assignment, expiry, strategies,
and combo orders.

**Python SDK:** `add_option`, `add_option_contract`, filters, iterable chains,
contract properties, open/close helpers, pricing methods, and option strategies.

**Mock providers:** Underlying bars, full option-universe snapshots, contract
quotes, OI, IV, Greeks, multipliers, splits, and expiry boundaries.

**Assertions:** Contract set, filters before subscription, spread-side fills,
Greeks/IV, multiplier, margin, expiry callback type, and stock/cash settlement.

### 14. Futures and continuous contracts

**Representative LEAN algorithms:** `BasicTemplateFuturesAlgorithm`,
`FuturesChainFullDataRegressionAlgorithm`, `OpenInterestFuturesRegressionAlgorithm`,
`FutureUniverseHistoryRegressionAlgorithm`, `ContinuousFutureRegressionAlgorithm`,
`ContinuousFutureModelsConsistencyRegressionAlgorithm`, and
`BasicTemplateFutureRolloverAlgorithm`.

**Rust SDK:** Future creation/contracts, chains, mapping/normalization modes,
depth offsets, continuous symbols, rollover, expiry, multiplier, margin, and
extended hours.

**Python SDK:** `add_future`, `add_future_contract`, filter and chain APIs,
future history, mapped symbol properties, and symbol-change callbacks.

**Mock providers:** Future-universe snapshots, contract bars/OI, mapping rows,
roll dates, back months, and exchange calendars.

**Assertions:** Chain contents, mapped contract, prices, rollover events,
holdings/orders through rolls, margin, and mapping-mode results.

### 15. Future options

**Representative LEAN algorithms:** `FutureOptionChainFullDataRegressionAlgorithm`,
`FutureOptionContinuousFutureRegressionAlgorithm`,
`FutureOptionCallITMExpiryRegressionAlgorithm`,
`FutureOptionShortCallOTMExpiryRegressionAlgorithm`, and
`FuturesAndFuturesOptionsExpiryTimeAndLiquidationRegressionAlgorithm`.

**Rust SDK:** Future-option creation, underlying future mapping, chains, filters,
margin groups, exercise/assignment, expiry, and liquidation timing.

**Python SDK:** Future-option adders, chain views, contracts, filters, and event
callbacks matching equity-option conventions.

**Mock providers:** Coordinated underlying future and option-universe streams,
contract quotes, rolls, expiries, and settlement prices.

**Assertions:** Underlying identity, chain membership, prices, margin offsets,
ITM/OTM behavior, resulting future position, and expiry timing.

### 16. Indexes and index options

**Representative LEAN algorithms:** `BasicTemplateIndexAlgorithm`,
`BasicTemplateIndexOptionsAlgorithm`,
`IndexOptionCallITMGreeksExpiryRegressionAlgorithm`,
`IndexOptionModelsConsistencyRegressionAlgorithm`, and
`IndexOptionShortPutOTMExpiryRegressionAlgorithm`.

**Rust SDK:** Index/index-option security types, market hours, cash settlement,
price models, multipliers, exercise, and margin.

**Python SDK:** Typed adders and views, chains, filters, Greeks, and settlement
callbacks.

**Mock providers:** Index bars, index-option universes and quotes, weekly/standard
expiries, and official settlement values.

**Assertions:** Correct target option, cash versus physical settlement, expiry
frontier, multiplier, Greeks, and no phantom underlying position.

### 17. Forex and CFDs

**Representative LEAN algorithms:** `BasicTemplateForexAlgorithm`,
`BasicTemplateCfdAlgorithm`, `TwoLegCurrencyConversionRegressionAlgorithm`, and
`G10CurrencySelectionModelFrameworkAlgorithm`.

**Rust SDK:** Pair symbols, quote currency, pip/tick properties, leverage,
conversion, swap/financing, fills, and settlement.

**Python SDK:** `add_forex`, `add_cfd`, typed security views, cash books,
conversion APIs, and brokerage-model behavior.

**Mock providers:** Bid/ask bars and ticks, direct/triangulated conversion pairs,
rollover/swap rates, and market calendars.

**Assertions:** Bid/ask fills, pip value, conversion, TPV, margin, financing, and
multi-currency equivalence.

### 18. Crypto and crypto futures

**Representative LEAN algorithms:** `BasicTemplateCryptoAlgorithm`,
`BasicTemplateCryptoFutureAlgorithm`, `BybitCryptoRegressionAlgorithm`,
`BybitCryptoFuturesRegressionAlgorithm`, `StableCoinsRegressionAlgorithm`, and
`CustomMarginInterestRateModelAlgorithm`.

**Rust SDK:** Spot/perpetual symbols, fractional lots, quote currencies, fees,
funding/margin interest, leverage, 24/7 calendars, and cash accounting.

**Python SDK:** `add_crypto`, `add_crypto_future`, typed views, brokerage models,
margin-interest access, and portfolio funding totals.

**Mock providers:** Spot/perpetual trade and quote data, margin-interest batches,
funding schedules, stablecoin conversion, and brokerage fee updates.

**Assertions:** Fractional sizing, maker/taker fee, funding cash flow and cadence,
TPV, and margin match the LEAN oracle from each SDK. This section closes issue
#79.

### 19. Custom data and ObjectStore

**Representative LEAN algorithms:** `CustomDataRegressionAlgorithm`,
`CustomDataTypeHistoryAlgorithm`, `CustomDataUniverseRegressionAlgorithm`,
`CustomDataUsingMapFileRegressionAlgorithm`,
`CustomDataObjectStoreRegressionAlgorithm`, and
`CustomDataMultiFileObjectStoreRegressionAlgorithm`.

**Rust SDK:** Custom schemas/points, linked underlyings, history, mapping,
universe usage, and strategy-scoped object storage.

**Python SDK:** `add_data`, dynamic fields, typed/custom history, custom universe
selectors, and ObjectStore CRUD/list/persistence semantics.

**Mock providers:** Canonical custom points with arbitrary fields and explicit
`EndTime`, multiple files/feeds, mappings, empty values, and replayed batches.

**Assertions:** No look-ahead, field types, symbol association, history shape,
mapping, once-only delivery, and ObjectStore persistence across restart.

### 20. Research and QuantBook

**Representative LEAN assets:** QuantBook regression scripts and the basic and
kitchen-sink research notebooks under `../Lean/Research` and
`../Lean/Tests/Research`.

**Rust SDK:** A public research service using the same history, security,
indicator, fundamental, option, and future contracts as algorithms.

**Python SDK:** Full `QuantBook` surface, pandas return shapes, option/future
history objects, fundamentals, indicator history, and portfolio statistics.

**Mock providers:** The same fixtures used by algorithm regressions, proving that
research does not have a second data path.

**Assertions:** Query requests, returned frames/collections, types, index/column
shape, values, and equivalence between research and algorithm history.

### 21. Results, charts, statistics, and runtime state

**Representative LEAN algorithms:** `BasicTemplateBenchmark`,
`CustomBenchmarkRegressionAlgorithm`, `ZeroedBenchmarkRegressionAlgorithm`,
`InsightScoringRegressionAlgorithm`, and algorithms with complete expected
statistics dictionaries.

**Rust SDK:** Chart/series APIs, runtime statistics, benchmark, result packets,
trade builder, capacity, and final statistics.

**Python SDK:** `plot`, chart creation, benchmark setters, runtime statistics,
and result-facing properties with LEAN names.

**Mock providers:** Deterministic strategy and benchmark streams, volumes for
capacity, risk-free rates, and controlled gaps.

**Assertions:** Chart samples, benchmark alignment, trades, daily equity,
drawdown, all formatted statistics, fees/funding, data counts, and order hash.

### 22. Live brokerage, reconciliation, and recovery

**Representative LEAN algorithms:** `BrokerageActivityEventHandlingAlgorithm`,
`CustomBrokerageSideOrderHandlingRegressionAlgorithm`,
`ExecutionModelOrderEventsRegressionAlgorithm`, and live-trading unit scenarios
from LEAN's brokerage/setup/transaction-handler tests.

**Rust SDK:** Live lifecycle, brokerage messages, account synchronization,
unmanaged holdings/orders, pause/resume, checkpoint restore, and disconnects.

**Python SDK:** Brokerage callbacks, live-mode properties, order tickets, and
restored insight/security/portfolio state.

**Mock providers:** Scripted live feed and brokerage with snapshots, orders,
partial fills, cumulative fees, external activity, reconnects, and restarts.

**Assertions:** Idempotent reconciliation, exact callback order, no duplicated
fills/fees, state restoration, unmanaged-position policy, and uninterrupted
versus restarted equivalence.

### 23. Determinism, transport faults, and metamorphic cases

These extend upstream algorithms with rlean-specific operational variants.
Every stable regression section should opt into them.

**Rust SDK and Python SDK:** No extra APIs. Both must remain deterministic under
equivalent inputs.

**Mock provider variants:** Batch-size changes, empty batches, independent stream
reordering, duplicate live batches, delayed query completion, disconnects,
replayed brokerage updates, and checkpoint boundaries.

**Assertions:** Identical normalized results and traces across variants. A warm
cache, cold cache, one large batch, many small batches, and restarted run must
not change strategy-visible behavior.

## Test stages

Each case records its highest passing stage:

1. `cataloged`: source and golden metadata are pinned.
2. `imports`: both SDK strategy fixtures compile/import.
3. `initializes`: initialization and subscription intent match.
4. `runs`: both complete with expected status and point counts.
5. `events`: callbacks, orders, and order events match.
6. `portfolio`: cash, holdings, fees, funding, and equity match.
7. `statistics`: formatted LEAN statistics and order hash match.
8. `trace`: high-fidelity traces match.
9. `metamorphic`: all enabled provider/fault variants match.

CI fails if a case falls below its recorded stage. Implementing a gap advances
the stage and commits the new baseline; it never weakens an assertion to make a
failure disappear.

## CI organization

Regression sections are independent test filters, not sequential rollout
waves:

```sh
cargo test --test lean_regressions -- section=orders
cargo test --test lean_regressions -- case=basic-template
cargo test --test lean_regressions -- state=supported
```

Per pull request:

- Run import/initialization checks for every cataloged Python case.
- Run all supported micro-fixture cases.
- Run affected sections selected from changed crates and a fixed smoke set.
- Fail on any passing-stage regression.

Nightly:

- Run every supported imported-LEAN fixture.
- Run chunking and fault variants.
- Optionally checkout the pinned LEAN commit and regenerate oracle traces.
- Publish a matrix by section, SDK, passing stage, and gap id.

## Definition of done

A regression section is implemented only when:

- Its shared engine behavior is covered by canonical mocked-provider fixtures.
- At least one Rust SDK strategy and one Python SDK strategy exercise each
  required public contract.
- Each SDK output independently matches the pinned LEAN oracle.
- Subscription and query intent are asserted, not merely accepted by the mock.
- Relevant chunking and fault variants pass.
- The generated Python stub describes the tested runtime surface.
- No expected gap or tolerance lacks a documented reason and issue.
