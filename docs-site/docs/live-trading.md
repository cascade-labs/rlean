---
sidebar_position: 5
title: Live Trading
---

# Live Trading

rlean runs the same engine used for backtests in live mode against a real (or
paper) brokerage and a live data provider.

## Running live locally

Live trading needs a brokerage and a live data provider. Both are supplied as
plugins (except the built-in paper brokerage):

```sh
rlean live my_strategy/main.py \
  --brokerage tradier \
  --data-provider-live tradier
```

- `--brokerage` selects the brokerage. Use `--brokerage paper` (or another paper
  brokerage) to trade with local paper fills instead of a live broker.
- `--data-provider-live` selects the live data feed; comma-separated values stack
  multiple live feeds.

Brokerages and data providers are runtime plugins — see
[Plugin Development](./plugin-development.md).

## Cloud fleet

`rlean cloud` manages a fleet of remote nodes reachable over SSH. Nodes are
recorded in `~/.rlean/nodes.json`. Registering a node probes its OS and
architecture over SSH and derives its Rust target triple, so later commands know
which prebuilt binaries to ship.

### Register and inspect nodes

```sh
rlean cloud add-node <ssh-alias> [--name N] [--role live,backtest]
rlean cloud list [--probe]
rlean cloud remove <name>
rlean cloud exec <name> -- <command...>
```

`list --probe` checks each node for reachability and its installed rlean version.

### Install rlean onto a node

```sh
rlean cloud install <name> [--release-tag T] [--plugin N...] [--plugin-repo owner/repo:tag...]
```

`install` ships rlean, the plugin set, and a subset of `~/.rlean` to the node.
`--plugin` restricts the default plugin set; `--plugin-repo` adds a plugin from
an explicit `owner/repo[:tag][#name]`.

### Deploy a strategy

```sh
rlean cloud deploy <name> <strategy-dir> \
  [--brokerage B] \
  [--data-provider-live L] \
  [--data-provider-historical H]
```

`deploy` snapshots a local strategy directory to the node and launches
`rlean live` there. The brokerage and live data provider default to `tradier`;
historical data providers default to `thetadata,massive`.

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
