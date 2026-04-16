# rlean — Claude Code Plugin

Extends Claude Code with two skills for the [rlean](https://github.com/cascade-labs/rlean) quantitative trading CLI.

## Skills

### `/rlean:rlean` — General rlean commands
Covers backtesting, live trading, project creation, configuration, and plugin management.

```
/rlean:rlean run a backtest on my sma_crossover strategy
/rlean:rlean create a new project called vol_harvest
/rlean:rlean set my ThetaData API key to abc123
/rlean:rlean what does the latest backtest report show?
```

### `/rlean:research` — Research kernel & notebooks
Manages persistent Python research sessions. Starts/stops kernels, adds/edits/runs
notebook cells, plots charts, and inspects the live kernel namespace.

```
/rlean:research plot SPY vs QQQ cumulative returns for the past 5 years
/rlean:research add a cell that computes annualised volatility for all holdings
/rlean:research fix cell 3 to use a 20-day EMA instead of 10-day
/rlean:research show me all variables currently in the kernel
```

## Installation

### Local (development)
```bash
claude --plugin-dir ./plugins/claude-code
```

### Via marketplace
Add this entry to your `~/.agents/plugins/marketplace.json` or your repo's
`.agents/plugins/marketplace.json`:

```json
{
  "plugins": [
    {
      "name": "rlean",
      "description": "rlean quantitative trading CLI plugin",
      "path": "/path/to/rlean/plugins/claude-code"
    }
  ]
}
```

Then install inside Claude Code:
```
/plugin install rlean
```

## Requirements

- `rlean` must be installed and on `PATH` (`~/.cargo/bin/rlean`)
- For research sessions: `numpy`, `pandas`, `matplotlib` must be installed for Python 3.13+
- For ThetaData/Polygon backtests: API keys configured via `rlean config set`
