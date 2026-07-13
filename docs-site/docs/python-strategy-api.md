---
sidebar_position: 4
title: Python Strategy API
---

# Python Strategy API

rlean targets API parity with LEAN's `QCAlgorithm`, so most strategies written
for LEAN run with little or no modification. A strategy is a Python class that
subclasses `QCAlgorithm` and overrides lifecycle methods.

## Lifecycle

```python
from AlgorithmImports import *

class MyStrategy(QCAlgorithm):
    def initialize(self):
        self.set_start_date(2020, 1, 1)
        self.set_end_date(2024, 1, 1)
        self.set_cash(100_000)
        self.spy = self.add_equity("SPY", Resolution.DAILY)

    def on_data(self, data):
        if not self.portfolio.invested:
            self.set_holdings(self.spy.symbol, 1.0)
```

- `initialize` runs once at startup. Set dates and cash, subscribe to
  securities, and construct indicators here.
- `on_data(data)` runs on every data slice. It receives a `Slice` of the current
  bars and ticks.

## Subscribing to securities

- `add_equity(ticker, resolution)` subscribes to an equity and returns its
  security handle.
- `add_option(...)` subscribes to an option and its chain.

## Portfolio and orders

- `set_holdings(symbol, target)` targets a fraction of portfolio value for a
  symbol.
- `portfolio.invested` reports whether any position is open.
- `portfolio.total_portfolio_value` is the current total portfolio value.

## Indicators

`lean-indicators` provides SMA, EMA, RSI, Bollinger Bands, and more, exposed
through the same construction and update pattern as LEAN.

## LEAN-compatible conventions

The Python surface follows LEAN's naming and typing so existing strategies port
cleanly:

- **snake_case** method and property names (`bid_price`, `ask_price`,
  `total_portfolio_value`).
- **Typed enums**, not strings: compare an option right with
  `c.right == OptionRight.Put`.
- **Iterable option chains**: `for c in chain` yields the contracts.
- **Arithmetic on contract fields**: `c.bid_price + c.ask_price`.
- **Nested quote bars**: `qb.bid.close` / `qb.ask.close` read the nested bar on
  a `QuoteBar`.

## Native Rust strategies

The same engine runs strategies written in Rust against the `IAlgorithm` trait,
using `QcAlgorithm` as the base. See
[Getting Started](./getting-started.md#rust-srcmy_strategyrs) for a Rust example.
