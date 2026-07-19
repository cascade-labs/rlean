# BUG: count-based `history()` re-reads the sidecar on every call (no warm buffer/cache)

**Component:** engine history API + data sidecar (`market_trade_bars` served via S3 Iceberg → verglas)
**Severity:** performance (makes an otherwise-cheap alpha pattern unusable)
**Filed:** 2026-07-16 (via etf_commodity_blend seasonality work; gh CLI auth was down, so filed as a repo note rather than a GitHub issue — please promote to cascade-labs/rlean issues)

## Symptom

`algorithm.history(symbol, count, Resolution.Daily)` called repeatedly in an alpha (e.g. a
seasonality alpha that recomputes once per calendar month for 15 symbols → ~4,300 calls over a
2002–2026 backtest) makes the backtest ~50–100× slower: a normally ~60s cached run was at only
20% (year 2006) after ~8 minutes and climbing linearly. Profiling by wall-clock shows the cost is
entirely in the repeated `history()` calls (the slowdown is present from 2002 onward, well before
any custom-data cold-fetch), and each call appears to issue a fresh `market_trade_bars` Iceberg
scan through the sidecar/verglas rather than serving from an already-warmed local buffer.

## Expected

For a symbol+resolution the algorithm is already subscribed to (daily equity bars here), a
count-based `history()` request should serve from the warm local cache / streamed buffer in ~O(count),
not re-scan S3 Iceberg per call. Repeated `history()` on the same symbol/resolution should be near-free
after the first read.

## Actual

Each `history(symbol, N, Resolution.Daily)` looks like an independent provider round-trip
(Iceberg/verglas read), so N-calls cost O(calls) sidecar reads. An alpha that legitimately needs a
long lookback recomputed periodically is forced to either hand-roll an incremental store or eat the
O(calls) penalty.

## Repro

`etf_commodity_blend` with a `SeasonalityAlphaModel` whose `Update` calls
`algorithm.history(symbol, 2600, Resolution.Daily)` per symbol once per month. Backtest 2002–2026.
Observe near-linear slowdown dominated by history calls (dir `backtests/2026-07-16_223800_strategy`).

## Workaround in use

Reimplemented the alpha as an incremental accumulator that updates from the daily bar stream
(`SymbolState.Update`) — O(1)/bar, zero `history()` calls. Works, but the point of the bug is that
`history()` shouldn't require this for already-subscribed data.

## Secondary note

The rlean engine's embedded Python has **no `pandas`** (`import pandas` → ModuleNotFoundError at
compile), while some workspace strategies (`ta_sweep_plus`) `import pandas as pd` and call
`pd.Series(bars["close"])`. Either those strategies fail at runtime on this build or pandas
availability is inconsistent across builds — worth confirming/documenting. `history()` returns a
dict-of-column-lists (`bars["close"]`, `bars["end_time"]`), which is fine to consume with numpy only.
