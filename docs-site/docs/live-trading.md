---
sidebar_position: 5
title: Live Trading
---

# Live Trading

rlean runs the same engine used for backtests in live mode against a real (or
paper) brokerage and a native live-data provider.

## Running live locally

Install the per-host supervisor once:

```sh
rlean daemon install
```

On macOS this installs a launchd LaunchAgent; on Linux it installs a systemd
user service. `rleand` starts at login/boot, owns all live strategy processes,
and restores deployments whose desired state is running.

`rleand` has a deliberately narrow ownership boundary. A normal `rlean live
<strategy>` command snapshots the strategy, records the deployment, and submits
the hidden foreground engine command to the daemon over its local Unix control
socket. The daemon owns that long-running child, reboot recovery, pause/resume,
and integration-failure restarts. Finite commands such as backtests, data inspection,
configuration, research setup, and cloud SSH probes run directly as CLI
processes and exit; they are not jobs inside `rleand`.

```sh
rlean daemon status
rlean daemon stop
rlean daemon start
rlean daemon uninstall
```

Pass `--system` only for a machine-wide launchd/systemd service; the default is
the current user's service. Both `rlean` and `rleand` must be installed beside
one another.

Live trading uses independent native connections for market data and live
execution. Strategy SDK calls such as `add_equity` create the subscriptions;
the run command selects only the feed and execution integrations:

```sh
rlean live my_strategy/main.py \
  --live-data-feed tradier \
  --brokerage robinhood
```

- `--live-data-feed` selects and authenticates the native market-data provider. It
  does not declare symbols, resolutions, or data types.
- `--brokerage` independently selects native order execution. Use
  `--brokerage paper` to retain local paper fills.
Integration credentials come from `~/.rlean/integration-configs.json`. For
example, `rlean config set tradier.access_token ...` updates the Tradier
credential bundle used by the native adapter.

Live events are unsolicited. After a subscription is acknowledged, the provider
pushes matching canonical values into the synchronizer and `on_data`.

If a provider or brokerage connection fails terminally, the strategy exits and
`rleand` recreates it with bounded exponential backoff. The existing deployment
directory is reused, so framework insights and strategy subscriptions are
restored. A restart is refused when account state exists but its insight
checkpoint is missing or corrupt.

## Cloud fleet

`rlean cloud` manages a fleet of remote nodes reachable over SSH. Nodes are
recorded in `~/.rlean/nodes.json`. Registering a node probes its OS and
architecture over SSH and derives its Rust target triple, so later commands know
which prebuilt binaries to ship.

### Register and inspect nodes

```sh
rlean cloud add-node <ssh-alias> [--name N] [--role live,backtest]
rlean cloud list [--offline]
rlean cloud remove <name>
rlean cloud exec <name> -- <command...>
```

`cloud list` probes each node over SSH, reports both the installed rlean version
and rleand control-socket health, and refreshes `last_seen`. Pass `--offline`
to read only the cached local registry without making network calls.

### Install rlean onto a node

```sh
rlean cloud install <name> [--release-tag T]
```

`install` is also the node binary-upgrade path. The control machine downloads
the requested release, verifies its checksums, and atomically replaces both
`~/.local/bin/rlean` and `~/.local/bin/rleand`. It regenerates
`~/.rlean/config`, copies `~/.rlean/integration-configs.json` when present, sets
both files to mode `0600`, and installs/starts the node's rleand systemd user
service. The remote host does not need GitHub credentials. Native data and
brokerage integrations run inside the strategy process.

### Deploy a strategy

```sh
rlean cloud deploy <name> <strategy-dir> \
  [--brokerage B]
```

`deploy` snapshots a local strategy directory and submits it to the node's
`rleand`. The node's machine configuration supplies provider credentials and
the Verglas gateway connection.

### Monitor deployments

```sh
rlean cloud status [<name>] [--deploy-id ID]
rlean cloud logs <name> [--deploy-id ID] [--lines N]
rlean cloud portfolio <name> [--deploy-id ID]
```

- `status` shows one node's live deployment, or aggregates across all recorded
  deployments when no node is given.
- `logs` prints a deployment's trailing log lines (`--lines` defaults to 250).
- `portfolio` prints a deployment's portfolio snapshot.

When a node has a single recorded deployment, `--deploy-id` can be omitted.

`rlean live upgrade <deploy-id>` has a different scope from `cloud install`: it
refreshes only the stored strategy-code snapshot. The deployment must be paused
or stopped, and a separate `resume` starts that snapshot under `rleand`.
