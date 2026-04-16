---
description: >
  Manage rlean research sessions and notebooks. Use when the user wants to explore
  trading data, run Python analysis, add or edit notebook cells, plot charts, check
  variables, or do any interactive research on a strategy project.
---

You are helping the user run an **rlean research session** — a persistent Python kernel
(PyO3-based, no Jupyter required) attached to a strategy project notebook (`research.ipynb`).

Use the Bash tool to run `rlean research` commands. Figures are auto-saved as PNG and
embedded in the notebook.

User request: "$ARGUMENTS"

---

## Finding the project

Research sessions are project-scoped. Resolve the project directory before issuing commands:

1. Look for subdirectories of the current workspace that contain `config.json`.
2. If the user names a strategy (e.g. "sma_crossover"), match it to `./sma_crossover/`.
3. If ambiguous, list candidates and ask.

Set `PROJECT=<resolved-path>` mentally before constructing commands below.

---

## Command reference

### Kernel lifecycle
```bash
rlean research <project>            # start the kernel (must run first)
rlean research <project> shutdown   # stop the kernel
rlean research <project> vars       # list all variables in the live namespace
```

The kernel starts a background daemon. Subsequent commands connect to it instantly.
If a command fails with "kernel not running", start it first.

### Inspecting the notebook
```bash
rlean research <project> cells                  # list cells: index, type, preview, output count
rlean research <project> get-cell <n>           # print full source of cell N
```

### Adding and editing cells
```bash
rlean research <project> add-cell "code"        # append new cell, execute it, save outputs
rlean research <project> add-cell --at N "code" # insert at index N, execute
rlean research <project> upsert-cell N "code"   # replace cell N source, re-execute
```

### Running cells
```bash
rlean research <project> run-cell N             # execute cell N, update its outputs
rlean research <project> run-all                # execute all cells in order (restores state)
```

### Notebook maintenance
```bash
rlean research <project> delete-cell N         # remove cell N
rlean research <project> clear-outputs         # strip all outputs, keep source
```

---

## What's pre-loaded in the kernel

Every session starts with these available — no imports needed:

| Name         | Type        | Description                                  |
|--------------|-------------|----------------------------------------------|
| `qb`         | QuantBook   | Historical data access (local Parquet)       |
| `Resolution` | enum        | `Resolution.DAILY`, `Resolution.HOUR`, etc.  |
| `np`         | numpy       | NumPy                                        |
| `pd`         | pandas      | Pandas                                       |
| `plt`        | matplotlib  | Pyplot — figures captured automatically      |

`plt.show()` is a no-op; just call it normally. Figures are harvested after each cell,
saved as PNG under `~/.lean-research/sessions/<name>/plots/`, and embedded in the notebook.

---

## QuantBook API
```python
qb.set_start_date(2020, 1, 1)
qb.set_end_date(2025, 1, 1)
qb.set_data_folder("/path/to/data")         # default: workspace data/ dir

spy = qb.add_equity("SPY")                  # subscribe — returns Security

# history() returns a dict: {time, open, high, low, close, volume}
h = qb.history("SPY", 252, Resolution.DAILY)
df = pd.DataFrame(h).set_index("time")

# date-range variant
h2 = qb.history_range("SPY", (2022,1,1), (2023,1,1), Resolution.DAILY)

# indicators: "SMA", "EMA", "RSI", "MACD", "BB", "ATR"
ema = qb.indicator("EMA", "SPY", 20, 252, Resolution.DAILY)
```

---

## Typical workflow

When the user asks to research or plot something:

1. **Check if kernel is running** — run `cells` or `vars`; if it errors, start it first.
2. **Inspect existing cells** — run `cells` to see what's already in the notebook.
3. **Write the code** — construct clean, self-contained Python.
4. **Choose the right command**:
   - New analysis → `add-cell`
   - Fixing or improving an existing cell → `upsert-cell N`
   - Re-running existing work → `run-cell N` or `run-all`
5. **Report outputs** — show the printed results; mention figure paths if generated.
6. **Multi-step analysis** — chain several `add-cell` calls for logically separate steps.

---

## Multi-line code quoting

For code with quotes, use single-quoted shell heredoc style or escape carefully.
When code is complex, write it to a temp file and use shell substitution:

```bash
cat > /tmp/research_cell.py << 'EOF'
# your python code here
EOF
rlean research sma_crossover add-cell "$(cat /tmp/research_cell.py)"
```
