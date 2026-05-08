# LEAN Python API Reference

This repository targets 1:1 compatibility with the documented QuantConnect LEAN Python API. When C# LEAN exposes a PascalCase member, Python algorithms should use the documented lowercase or snake_case spelling from QuantConnect's Python examples.

## Compatibility Rule

Implement the Python spelling shown in the official QuantConnect docs first. Rust/rlean compatibility aliases can exist, but they must not replace documented LEAN Python syntax.

## Lifecycle

| C# LEAN | Python LEAN |
| --- | --- |
| `Initialize()` | `initialize(self)` |
| `OnData(Slice slice)` | `on_data(self, slice)` |
| `OnOrderEvent(OrderEvent orderEvent)` | `on_order_event(self, order_event)` |

Source: [Event Handlers](https://www.quantconnect.com/docs/v2/writing-algorithms/key-concepts/event-handlers)

## Initialization

| C# LEAN | Python LEAN |
| --- | --- |
| `SetStartDate(2013, 1, 5)` | `self.set_start_date(2013, 1, 5)` |
| `SetEndDate(2015, 1, 5)` | `self.set_end_date(2015, 1, 5)` |
| `SetCash(100000)` | `self.set_cash(100000)` |
| `SetWarmUp(100, Resolution.Daily)` | `self.set_warm_up(100, Resolution.DAILY)` |

Source: [Initialization](https://www.quantconnect.com/docs/v2/writing-algorithms/initialization)

## Securities

| C# LEAN | Python LEAN |
| --- | --- |
| `AddEquity("SPY")` | `self.add_equity("SPY")` |
| `AddEquity("SPY", Resolution.Daily).Symbol` | `self.add_equity("SPY", Resolution.DAILY).symbol` |
| `AddEquity("SPY", market: Market.USA)` | `self.add_equity("SPY", market=Market.USA)` |

Source: [US Equity Requesting Data](https://www.quantconnect.com/docs/v2/writing-algorithms/securities/asset-classes/us-equity/requesting-data)

## History

| C# LEAN | Python LEAN |
| --- | --- |
| `History(symbol, 20, Resolution.Daily)` | `self.history(symbol, 20, Resolution.DAILY)` |
| `History(symbol, start, end, Resolution.Daily)` | `self.history(symbol, start, end, Resolution.DAILY)` |
| `History(customSymbol, 20, resolution)` | `self.history(custom_symbol, 20, resolution)` |

rlean currently supports the algorithm-scoped subset above for a single subscribed equity/security symbol or a single `add_data` custom symbol. The request is anchored to `self.time`, matching C# LEAN's period-based history semantics. The return value follows rlean's existing QuantBook contract: a column-oriented `dict` suitable for `pd.DataFrame(...)`, not LEAN's full pandas MultiIndex dataframe surface.

Implemented custom history uses the registered custom data subscription/source and the same cache/source helpers used by the runner, so strategies should not read provider files directly. Warm-up daily slices also load low-resolution custom subscriptions through the same custom data path used by the main daily loop. Full C# LEAN overloads that are still out of scope include multi-symbol dataframe shaping, typed `History[T]` subscription selection, tick period requests, fill-forward/extended-hours flags, mapping/normalization override parameters, and option/future universe history.

## Portfolio

| C# LEAN | Python LEAN |
| --- | --- |
| `Portfolio.Invested` | `self.portfolio.invested` |
| `Portfolio[_symbol].Invested` | `self.portfolio[self._symbol].invested` |
| `Portfolio[_symbol].Quantity` | `self.portfolio[self._symbol].quantity` |
| `Portfolio[_symbol].IsLong` | `self.portfolio[self._symbol].is_long` |
| `Portfolio[_symbol].IsShort` | `self.portfolio[self._symbol].is_short` |
| `Portfolio.TotalPortfolioValue` | `self.portfolio.total_portfolio_value` |
| `Portfolio.Cash` | `self.portfolio.cash` |

`self.portfolio.invested` is the correct documented Python syntax for the whole-portfolio investment state. Do not require users to write `self.portfolio.is_invested` for LEAN compatibility.

Source: [Portfolio Key Concepts](https://www.quantconnect.com/docs/v2/writing-algorithms/portfolio/key-concepts)

## Orders

| C# LEAN | Python LEAN |
| --- | --- |
| `MarketOrder("IBM", 100)` | `self.market_order("IBM", 100)` |
| `Buy("AAPL", 10)` | `self.buy("AAPL", 10)` |
| `Sell("TSLA", 25)` | `self.sell("TSLA", 25)` |
| `Order("SPY", 20)` | `self.order("SPY", 20)` |
| `Liquidate("IBM")` | `self.liquidate("IBM")` |

Sources: [Market Orders](https://www.quantconnect.com/docs/v2/writing-algorithms/trading-and-orders/order-types/market-orders), [Liquidating Positions](https://www.quantconnect.com/docs/v2/writing-algorithms/trading-and-orders/liquidating-positions)

## Position Sizing

| C# LEAN | Python LEAN |
| --- | --- |
| `SetHoldings("IBM", 0.5)` | `self.set_holdings("IBM", 0.5)` |
| `SetHoldings("IBM", 0.5, liquidateExistingHoldings: true)` | `self.set_holdings("IBM", 0.5, liquidate_existing_holdings=True)` |
| `new PortfolioTarget("SPY", 0.8m)` | `PortfolioTarget("SPY", 0.8)` |
| `SetHoldings(targets)` | `self.set_holdings(targets)` |

Source: [Position Sizing](https://www.quantconnect.com/docs/v2/writing-algorithms/trading-and-orders/position-sizing)
