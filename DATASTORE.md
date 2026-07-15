# Data plane

rlean strategy processes receive all market and custom data through one
persistent, versioned Apache Arrow Flight session. There are no in-process data
providers or storage fallbacks.

Strategies create and remove subscriptions through the SDK. In backtests rlean
registers each subscription once and requests bounded, nonoverlapping time
ranges with bounded per-subscription buffering. In live mode the sidecar pushes
matching batches unsolicited over the existing exchange.

`rlean-data-tables` defines the canonical provider-neutral Arrow contract for TradeBars,
QuoteBars, Ticks, corporate actions, universes, rates, and custom data. `venue`
is distinct from LEAN `market` and is retained for market and custom rows.

The sidecar owns vendor APIs, persistence, cache filling, and ingestion. A
leader sends each newly ingested canonical batch both to matching local
strategy subscriptions and to generic follower-daemon relay subscriptions.
Followers forward a relayed batch only to their own matching strategies.

Live data and execution brokerage are independent authenticated connections.
Paper execution remains inside rlean. Integration credentials originate in
`~/.rlean/integration-configs.json` and are passed opaquely to the sidecar.

See `docs-site/docs/sidecar-data-plane.md` for the protocol and flow-control
details. Use `rlean data tables` and `rlean data schema <table>` for the current
executable table contract.
Use `rlean data manifest --json` to inspect the configured sidecar and
`rlean data query ... [--json]` to run a bounded query through the existing
add-subscription, backtest-query, and remove-subscription exchange.
