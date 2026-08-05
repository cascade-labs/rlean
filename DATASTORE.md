# Data plane

rlean uses separate LEAN-style interfaces for historical providers, live
providers, and execution brokerages. Strategies create and remove subscriptions
through the SDK. Backtests request bounded, nonoverlapping history ranges; live
providers push subscribed events into the synchronizer.

`rlean-data-tables` defines the canonical provider-neutral Arrow contract for TradeBars,
QuoteBars, Ticks, corporate actions, universes, rates, and custom data. `venue`
is distinct from LEAN `market` and is retained for market and custom rows.

Historical reads are cache-first through the Verglas Rust SDK. rlean reads the
canonical cache, asks the selected provider only for durable uncovered ranges,
persists successful canonical Arrow batches, and then consumes one ordered
result. Live events enter the synchronizer immediately and are persisted through
a bounded asynchronous writer.

Live data and execution brokerage are independent authenticated connections.
Paper execution remains inside rlean. Native integration credentials live in
`~/.rlean/integration-configs.json`.

Use `rlean data tables` and `rlean data schema <table>` for the current
executable table contract.
