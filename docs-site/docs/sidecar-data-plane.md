---
sidebar_position: 3
title: Sidecar Data Plane
---

# Sidecar Data Plane

rlean receives all strategy data through one persistent, versioned Apache Arrow
Flight `DoExchange` session. The engine has no in-process market-data provider
or storage fallback. A strategy run needs a sidecar endpoint, but the sidecar
may be local or remote and may choose any storage or vendor implementation that
can produce the canonical Arrow schemas.

## Session and subscriptions

At connection time rlean sends `Initialize` with a new `origin_id` and
`session_id`. The sidecar keeps subscriptions under that session, so several
rlean processes can use one daemon without sharing subscription or brokerage
state. Subscription ids are unique within a session.

The strategy SDK remains the source of subscription intent. Calls such as
`add_equity`, custom-data registration, universe changes, and security removal
cause rlean's subscription manager to send `AddSubscription` or
`RemoveSubscription`. Run arguments select connections; they do not list the
strategy's symbols.

Each provider-neutral subscription identifies the symbol, security type, LEAN
market, resolution, tick type, canonical data type, extended-hours choice,
custom-data query, and `venue`. `venue` is the physical dataset or execution
venue and is deliberately separate from the LEAN market encoded in the SID.

## Backtests: registered subscriptions and bounded pulls

A backtest registers each subscription once in `Backtest` delivery mode. rlean
then sends bounded `BacktestQuery` ranges for that subscription instead of
issuing unrelated one-off symbol requests. The current range sizes are at most
one year for daily data, one month for hourly data, 21 calendar days for minute
data, and one day for second or tick data.

rlean owns flow control. Its per-subscription channels are bounded, the
prefetch horizon shrinks as the number of active subscriptions grows, and the
synchronizer advances subscriptions fairly by their watermarks. The sidecar
rejects an identical query while it is already in flight. Completion is
explicitly correlated by query id, and rlean advances to the next nonoverlapping
range only after the current range completes. Removing a subscription also
cancels its active queries.

The sidecar decides whether a range comes from persisted data or a vendor and
returns one or more canonical Arrow record batches. That cache/provider choice
is not visible to the engine.

## Live: unsolicited pushed batches

A live run first opens a live-data feed connection, then registers strategy
subscriptions in `Live` delivery mode. The sidecar pushes matching batches over
the existing Flight exchange as they arrive; rlean does not poll the sidecar or
issue `BacktestQuery` messages in live mode. Each batch carries its subscription
id, allowing rlean to route it into the correct stream and ultimately the
strategy's `on_data` slice.

Multiple rlean processes may subscribe to the same daemon concurrently. The
sidecar tracks their distinct origin/session pairs and fans a batch out only to
matching subscriptions in each session.

## Canonical data contract

The `rlean-data-tables` crate is the single contract used by sidecars,
persistence implementations, and the runtime engine. It defines TradeBars, QuoteBars, Ticks,
margin interest, custom points, universes, factor files, and map files together
with their Arrow schemas and partition specifications.

Prices and custom numeric values use `decimal(38,18)`. Timestamps are Unix epoch
nanoseconds in UTC. Market rows and custom points include `venue`; for custom
data it distinguishes otherwise identical provider/feed observations or series
origins. Existing prototype rows may have a null venue until their backfill is
complete.

Inspect the executable contract rather than duplicating schemas in an adapter:

```sh
rlean data tables
rlean data schema market_trade_bars
rlean data schema custom_points
rlean data manifest --json
rlean data query market_trade_bars SPY --resolution daily \
  --start 2025-07-01 --end 2025-07-01 --json
```

The manifest is owned by the sidecar and passed through by rlean. CLI queries
create a temporary backtest subscription and use the same `AddSubscription`,
`BacktestQuery`, `DataBatch`, and `RemoveSubscription` messages as the engine.
`--json` emits newline-delimited JSON rows while the wire payload remains the
canonical Arrow record batch.

## Endpoint, authentication, and integrations

Configure the endpoint and optional session bearer token globally or per run:

```sh
rlean config set data_sidecar grpc://127.0.0.1:7410
rlean config set data_sidecar_token <token>
```

The equivalent flags are `--data-sidecar` and `--data-sidecar-token`.
`grpc://` plaintext is restricted to loopback. Use
`grpc+tls://host:port` for a remote daemon, or
`grpc+unix:///absolute/path` for a local Unix socket.

Provider-specific credentials are stored in
`~/.rlean/integration-configs.json`. A dotted command such as
`rlean config set tradier.access_token ...` writes that file with owner-only
permissions. rlean passes the selected integration's JSON opaquely to the
sidecar; only the sidecar adapter interprets it.

Live market data and execution brokerage are independent connections. For
example, a run can use Tradier data and Robinhood execution. `--brokerage
paper` keeps simulated execution inside rlean, while live data still comes from
the sidecar.

## Leader and follower daemons

A sidecar leader may run ingestion and also serve locally connected strategies.
Every newly ingested canonical batch is delivered to:

1. matching strategy subscriptions connected directly to the leader; and
2. every connected follower daemon through one generic `Relay` subscription.

The relay carries the same canonical batch plus provider-neutral subscription
identity. It is not hard-coded to a provider, feed, or series. A follower does
not rebroadcast upstream data to other followers; it forwards each relayed
batch only to its own matching live strategy subscriptions. Brokerage sessions
and credentials are never relayed.

## Failure behavior under rleand

For daemon-managed live deployments, a closed or unavailable sidecar session
causes the strategy process to exit. `rleand` recognizes sidecar transport
failures and restarts the deployment with bounded exponential backoff. It does
not restart arbitrary Python or strategy failures.

Before restarting a brokerage deployment, rlean validates the persisted insight
checkpoint when account state exists. Missing or corrupt state fails closed
instead of treating existing holdings as unmanaged.
