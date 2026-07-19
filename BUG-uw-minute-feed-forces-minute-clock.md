# BUG/PERF: subscribing a minute-native UW feed forces a minute clock over the whole backtest

**Component:** engine time-stepping + `unusual_whales` (data_sidecar) custom-data subscription
**Severity:** performance (makes UW data unusable in multi-year daily backtests)
**Filed:** 2026-07-16 (etf_commodity_blend dealer-gamma alpha)

## Symptom

Adding a UW `spot_exposures` subscription to an otherwise all-Daily strategy:

```python
self.add_data("unusual_whales", "spot_exposures", Resolution.Minute,
              properties={"symbols": "XLK,XLV,..."})
```

makes the backtest step MINUTE-by-minute across the ENTIRE date range, ~390 steps/day, even for the
2002-2023 span where UW has NO data (UW starts ~2023-08). Measured: ~30% (year 2009) in ~7 min; a full
2002-2026 run projects to ~40+ min (vs ~60s for the same book without the UW feed). Requesting
`Resolution.Daily` on the subscription does NOT help — it still steps at minute granularity (the
minute-native feed's resolution appears to drive the algorithm clock).

## Expected

Either:
- serve UW as a **daily/EOD aggregate** when `Resolution.Daily` is requested (one point/day, keeps the
  algorithm on a daily clock), or
- only advance the minute clock on days/subscriptions that actually have data, so the pre-availability
  span (and days with no UW prints) don't pay the 390x per-day stepping cost.

## Actual

The mere presence of a minute-native subscription forces minute stepping for all 24 years, so a strategy
that only needs one UW reading/day (EOD, 1-day-lagged) is ~40x slower and impractical to iterate.

## Workaround

Measuring the alpha on a short window (2022-2026) to confine the minute-clock cost — but even 4 years
is ~30 min. A daily UW aggregate feed would make the full-window book usable.

## UPDATE 2026-07-17: precise root cause for the backtest hang/abort

After the availability-boundary + universe-rebuild fixes, `underlying_fields_eod` (Daily) still aborts
at algorithm init with:

```
Flight exchange response stream failed: code 'Operation was attempted past the valid range',
"Error, decoded message length too large: found 6277470 bytes, the limit is: <max>"
```

Root cause: the UW Flight response (~6.28 MB, multi-symbol batch) EXCEEDS the Arrow Flight gRPC max
decode message size, so the stream fails on the first UW fetch and the backtest dies. This also
explains the earlier intermittent "Flight exchange closed / h2 broken pipe" failures (same oversized
message). Fix: raise `max_decoding_message_size` on the Flight client (rlean engine) and/or server,
or chunk the UW response into smaller Flight record batches. Standalone daemon single-symbol queries
stay small (sub-second) and don't hit it; the backtest's multi-symbol batch does.
