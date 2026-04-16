# rlean — Codex Plugin

Extends Codex with two skills for the [rlean](https://github.com/cascade-labs/rlean) quantitative trading CLI.

## Skills

### `/rlean:rlean` — General rlean commands
Covers backtesting, live trading, project creation, configuration, and plugin management.

```
/rlean:rlean run a backtest on my sma_crossover strategy
/rlean:rlean create a new project called vol_harvest
/rlean:rlean set my ThetaData API key to abc123
```

### `/rlean:research` — Research kernel & notebooks
Manages persistent Python research sessions. Starts/stops kernels, adds/edits/runs
notebook cells, plots charts, and inspects the live kernel namespace.

```
/rlean:research plot SPY vs QQQ cumulative returns for the past 5 years
/rlean:research add a cell that computes annualised volatility for all holdings
/rlean:research fix cell 3 to use a 20-day EMA instead of 10-day
```

## Installation

### Local (development)
```bash
codex --plugin-dir ./plugins/codex
```

### Via marketplace
Add to your `~/.agents/plugins/marketplace.json` or repo-level
`.agents/plugins/marketplace.json`:

```json
{
  "plugins": [
    {
      "name": "rlean",
      "description": "rlean quantitative trading CLI plugin",
      "path": "/path/to/rlean/plugins/codex"
    }
  ]
}
```

## Requirements

- `rlean` must be installed and on `PATH` (`~/.cargo/bin/rlean`)
- For research sessions: `numpy`, `pandas`, `matplotlib` installed for the linked Python version
- For ThetaData/Polygon backtests: API keys configured via `rlean config set`
