# rlean-data-tables

This crate owns the canonical data values shared by providers, persistence, and the runtime
engine. `rlean-data` re-exports the runtime market types; it
does not define a second persistence representation.

| Table | Rust row contract | Module |
| --- | --- | --- |
| `market_trade_bars` | `TradeBar` | `market_trade_bar` |
| `market_quote_bars` | `QuoteBar` | `market_quote_bar` |
| `market_ticks` | `Tick` | `market_tick` |
| `margin_interest` | `MarginInterestRate` | `margin_interest` |
| `custom_points` | `CustomDataPoint` | `custom_point` |
| `option_universe` | `OptionUniverseRow` | `option_universe` |
| `future_universe` | `FutureUniverseRow` | `future_universe` |
| `fundamental_universe` | `FundamentalUniverseRow` | `fundamental_universe` |
| `etf_constituents` | `EtfConstituentRow` | `etf_constituent` |
| `factor_files` | `FactorFileEntry` | `factor_file` |
| `map_files` | `MapFileEntry` | `map_file` |
