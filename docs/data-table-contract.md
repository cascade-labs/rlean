# Canonical data-table contract

`rlean-data-tables` is the only row and schema contract shared by rlean and data
sidecars. Runtime types are not separately translated
into provider-specific records.

The contract currently includes:

| Table | Rust row type | Partition specification |
|---|---|---|
| `market_trade_bars` | `TradeBar` | `security_type`, `market`, `resolution`, `month(day)` |
| `market_quote_bars` | `QuoteBar` | `security_type`, `market`, `resolution`, `month(day)` |
| `market_ticks` | `Tick` | `security_type`, `market`, `resolution`, `month(day)` |
| `margin_interest` | `MarginInterestRate` | `security_type`, `market`, `resolution`, `month(day)` |
| `custom_points` | `CustomDataPoint` | `provider`, `resolution`, `month(day)` |
| `option_universe` | `OptionUniverseRow` | `market`, `month(day)` |
| `future_universe` | `FutureUniverseRow` | `market`, `month(day)` |
| `fundamental_universe` | `FundamentalUniverseRow` | `market`, `month(day)` |
| `etf_constituents` | `EtfConstituentRow` | `market`, `month(day)` |
| `factor_files` | `FactorFileEntry` | `market` |
| `map_files` | `MapFileEntry` | `market` |

Do not copy a schema from this document into an integration. The executable
contract reports exact field types, nullability, descriptions, and transforms:

```sh
rlean data tables
rlean data schema market_trade_bars
rlean data schema custom_points
```

Prices, quantities, rates, factors, and custom scalar values use
`decimal(38,18)`. Times use signed 64-bit Unix epoch nanoseconds in UTC. The
`day` routing field is a logical calendar date.

Market bars, ticks, margin interest, and custom points include `venue`.
`venue` identifies a physical dataset/exchange or provider-defined series
origin; it is independent of the LEAN `market` encoded in the symbol SID.
Tick `exchange` remains a separate per-tick upstream code. Venue is nullable
until existing prototype rows are backfilled.

Custom data retains its original named payload in `fields_json`, with stable
`provider`, `feed`, and `resolution` routing columns and optional
`symbol_sid`/`symbol_value`. `end_time_ns` is the availability frontier and the
point must not reach a strategy before it.

Factor and map rows use the same sidecar subscription/query path as market
data. They are not files read directly by a strategy process.
