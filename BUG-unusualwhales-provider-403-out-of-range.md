# BUG: `unusual_whales` provider aborts backtest with live-API HTTP 403 for pre-availability dates

**Component:** data provider `unusual_whales` (custom data via sidecar)
**Severity:** blocker (can't use UW data in any backtest that starts before the account's earliest UW date)
**Filed:** 2026-07-16 (etf_commodity_blend UW dealer-gamma alpha; gh auth down — promote to cascade-labs/rlean issue)

## Symptom

`add_data("unusual_whales", "spot_exposures", Resolution.Daily, properties={"symbols": ...})` in a
backtest that starts in 2002 aborts immediately:

```
Error: Data error: Unusual Whales returned HTTP 403 Forbidden:
{"code":"historic_data_access_missing","message":"The earliest date currently available to you is
2023-08-16 (730 trading days) so 2000-05-11 in query param date will not return historical data. ..."}
```

The provider fetches from the **live UW REST API** (unusualwhales.com), which serves only the last
~730 trading days (2023-08-16+), and a 403 for an out-of-range date is propagated as a FATAL engine
error rather than treated as "no data for this date."

## Expected

For a long backtest over a data source that only exists for later years, the provider should:
1. Serve historical bars from the configured **S3 bucket** (`unusual_whales.s3_bucket` /
   `s3_endpoint` / `s3_access_key` are all set in `rlean config` — the historical store clearly
   exists), NOT the live REST API; and
2. Return **empty** for dates outside the available window instead of a fatal 403.

Then an alpha subscribed to UW data is simply a no-op in the pre-availability years (emits no insights,
zero portfolio weight, zero dilution) and only expresses views once the data exists — the normal,
desired pattern for a rich-but-short-history dataset.

## Actual

The provider hits the live API for historical dates and a 403 aborts the whole backtest at t0. There
is no `rlean config` knob to bound the subscription's available date range, so this can't be worked
around from strategy code (`add_data` has no start-date parameter).

## Repro

`etf_commodity_blend` (start 2002) + a `DealerGammaExposureAlphaModel` subscribing
`add_data("unusual_whales", "spot_exposures", ...)`. Backtest aborts immediately with the 403 above.

## Ask

Route `unusual_whales` historical reads through the configured S3 bucket and return empty (not 403)
for out-of-range dates — OR add a per-subscription available-window / start-date so out-of-range
fetches are skipped. Either makes short-history UW data usable in full-length backtests.
