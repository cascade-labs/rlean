---
name: lean-reference-design
description: Ground ambiguous rlean design and implementation decisions in the upstream C# LEAN engine. Use when rlean APIs, engine behavior, SDK/Python compatibility, data flow, backtest/live behavior, or architecture are vague and should be compared against ../Lean or ~/code/Lean.
---

# LEAN Reference Design

Use this skill when a design decision in rlean is ambiguous and the best answer likely comes from studying the upstream C# LEAN implementation.

## Core Rule

Treat C# LEAN as the behavioral and API reference, not as code to port mechanically. Propose and implement the rlean equivalent in the Rust layer first.

Python and SDK code should remain thin compatibility layers over Rust-owned behavior. Avoid implementing core engine, data, brokerage, portfolio, universe, option, or algorithm semantics primarily in Python bindings.

## Workflow

1. Identify the rlean surface being designed or changed.
   - Find the relevant Rust crates, traits, structs, and call paths.
   - Note whether the change affects algorithm APIs, data subscriptions, slices, securities, orders, history, universe selection, portfolio, live execution, reporting, or SDK/Python bindings.

2. Inspect upstream C# LEAN in `../Lean` or `~/code/Lean`.
   - Search for equivalent classes, interfaces, methods, tests, and examples.
   - Prefer the real engine path over isolated helper code.
   - Look for both public API shape and internal lifecycle behavior.

3. Compare semantics before designing.
   - Record the C# names and responsibilities being mirrored.
   - Identify where rlean should intentionally differ because of Rust ownership, traits, lifetimes, Parquet-only data, plugin boundaries, or existing rlean architecture.
   - Preserve LEAN-compatible user-facing behavior unless there is a clear rlean constraint.

4. Design the Rust implementation.
   - Put core behavior in the appropriate Rust crate.
   - Use Rust traits, enums, structs, ownership, and lifetimes idiomatically.
   - Keep cross-crate boundaries explicit and avoid moving engine semantics into SDK/Python glue.
   - If Python exposure is needed, bind to Rust behavior rather than duplicating logic.

5. Optimize SDK/Python data transfer.
   - Prefer zero-copy or borrowed views from Rust-owned data into SDK/Python when practical.
   - Avoid unnecessary cloning, transcoding, JSON round-trips, or Python-owned mirror structures for hot-path market data.
   - If copying is unavoidable, make the boundary explicit and justify it.

6. Check both execution paths.
   - Consider how the design behaves in backtests and live trading.
   - Inspect both rlean backtest and live runner paths when the feature can affect subscriptions, data delivery, orders, brokerage interactions, scheduling, state, or reporting.
   - Do not treat backtest-only correctness as complete if the same concept exists in live mode.

7. Propose implementation with evidence.
   - Cite the C# LEAN structures inspected.
   - Cite the rlean files and modules that should change.
   - Explain the Rust-first design and how SDK/Python stays thin.
   - Mention backtest/live implications and zero-copy considerations.

## Output Shape

When presenting a design, include:

- C# LEAN reference: classes, interfaces, methods, and tests/examples inspected.
- rlean mapping: Rust crates, traits, structs, and APIs to create or modify.
- Design choice: the proposed Rust implementation and any intentional differences from C#.
- SDK/Python boundary: how bindings expose the Rust behavior, with zero-copy or borrowed data where practical.
- Backtest/live impact: how both paths are handled or why one path is unaffected.
- Validation: focused tests, compile checks, or parity examples to run.

## Guardrails

- Do not recreate CSV paths; rlean data remains Parquet-only.
- Do not implement core behavior in Python because it is quicker.
- Do not clone hot-path market data into SDK/Python unless required.
- Do not assume C# naming maps directly to Rust naming or ownership.
- Do not finalize an engine design without considering live and backtest behavior.
