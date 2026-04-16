---
description: >
  Run rlean commands for backtesting, live trading, project scaffolding, configuration,
  and plugin management. Use when the user wants to run a backtest, create a strategy
  project, set API keys, manage plugins, install stubs, or perform any rlean CLI operation.
---

You are helping the user operate **rlean**, a Rust-native LEAN quantitative trading CLI.
Use the Bash tool to run rlean commands from the user's workspace directory.

User request: "$ARGUMENTS"

---

## Finding the workspace and project

rlean workspaces contain `rlean.json` at the root. Strategy projects are subdirectories
that each contain `config.json` and `main.py`. When a project is needed:

1. Check the current directory for `rlean.json` — if present, this is the workspace root.
2. Look for subdirectories containing `config.json` to discover projects.
3. If ambiguous, ask the user which project to use.

---

## Command reference

### Workspace setup
```
rlean init                          # bootstrap workspace (creates rlean.json + data/)
rlean create-project <name>         # scaffold a new strategy project
```

### Backtesting
```
rlean backtest <project-dir>
rlean backtest <project-dir> --data-provider-historical thetadata
rlean backtest <project-dir> --data-provider-historical polygon
rlean backtest <project-dir> --start-date YYYY-MM-DD --end-date YYYY-MM-DD
rlean backtest <project-dir> --verbose
```
Pass the **project directory** (e.g. `sma_crossover/`), not `main.py` directly.
Results are written to `<project>/backtests/<timestamp>/report.html`.

### Live trading
```
rlean live <project-dir>
```

### Configuration
```
rlean config list
rlean config get <key>
rlean config set <key> <value>
```
Common keys: `thetadata-api-key`, `polygon-api-key`.

### Plugins
```
rlean plugin list
rlean plugin install <path-or-name>
rlean plugin remove <name>
```

### IDE stubs
```
rlean stubs install                 # writes AlgorithmImports.pyi for autocomplete
```

---

## Data layout
```
<workspace>/data/
  equity/usa/daily/<ticker>/        # daily OHLCV parquet files
  equity/usa/factor_files/          # split/dividend adjustment parquets
  equity/usa/minute/<ticker>/       # minute bars (date-partitioned)
  option/usa/daily/<ticker>/        # EOD option chains
```
All data is **Parquet only** — no CSV files exist or should be created.

---

## Strategy file conventions
- `main.py` is the entry point for Python strategies
- Strategies subclass `QCAlgorithm` and import from `AlgorithmImports`
- Compiled Rust plugins export `create_algorithm` and use `.so` / `.dylib` extensions

---

## Instructions
1. Identify the correct subcommand from the user's request.
2. Resolve the project directory if needed (check current dir, list subdirs with config.json).
3. Run the command using the Bash tool.
4. On success, summarise the key results (e.g. final equity, Sharpe, drawdown from report).
5. On error, diagnose and explain — common causes: missing data, invalid date range, missing API key.
