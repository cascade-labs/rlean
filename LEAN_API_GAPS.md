# LEAN API Gap Analysis: rlean vs. Full LEAN

**Generated:** 2026-04-11
**Scope:** Comparison of rlean's Python bindings against LEAN's full API inventory

This document identifies gaps between what LEAN exposes and what rlean currently implements in its Rust-based Python bindings.

---

## Summary

- **Total LEAN Methods Reviewed:** 200+
- **rlean Implementation Status:**
  - Implemented (EXISTS): ~40 methods
  - Missing (MISSING): ~160 methods
  - Partially Implemented (WRONG/INCOMPLETE): ~15 methods

**High Priority Gaps:** Option chain access patterns, lifecycle hooks, data/portfolio access methods, all indicator shortcuts.

---

## Detailed Gap Analysis by Category

### 1. Lifecycle Callbacks

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `initialize()` | EXISTS | Overrideable hook in PyQcAlgorithm |
| `post_initialize()` | MISSING | Not called after initialize() completes |
| `on_data(slice)` | EXISTS | Called with Slice object on each bar |
| `on_warmup_finished()` | EXISTS | Overrideable hook |
| `on_order_event(order_event)` | EXISTS | Called with OrderEvent |
| `on_assignment_order_event(assignment_event)` | **WRONG** | Current signature: `on_assignment_order_event(contract, quantity, is_assignment)` — should mirror `on_order_event(order_event)` signature where event includes is_assignment flag |
| `on_end_of_algorithm()` | EXISTS | Called at end |
| `on_end_of_day()` | MISSING | No per-symbol or daily finalization hook |
| `on_end_of_day(symbol)` | MISSING | |
| `on_splits(splits)` | MISSING | No corporate action callbacks |
| `on_dividends(dividends)` | MISSING | |
| `on_delistings(delistings)` | MISSING | |
| `on_symbol_changed_events()` | MISSING | |
| `on_securities_changed(changes)` | MISSING | Universe change notifications |
| `on_margin_call(requests)` | MISSING | No margin management |
| `on_margin_call_warning()` | MISSING | |
| `on_brokerage_message()` | MISSING | |
| `on_brokerage_disconnect()` | MISSING | |
| `on_brokerage_reconnect()` | MISSING | |

**Action:** Standardize `on_assignment_order_event` to take an OrderEvent object (with is_assignment=True flag) instead of custom parameters.

---

### 2. Universe/Subscription Methods

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `add_equity(ticker, resolution, ...)` | EXISTS | Basic implementation |
| `add_forex(ticker, resolution, ...)` | EXISTS | Basic implementation |
| `add_crypto(ticker, resolution, ...)` | EXISTS | Basic implementation |
| `add_option(ticker, resolution)` | EXISTS | Returns canonical symbol string (e.g., "?SPY") |
| `add_option(underlying, resolution)` | PARTIAL | Only one overload; LEAN has multiple (target_option variants) |
| `add_future()` | MISSING | No futures support |
| `add_future_contract()` | MISSING | |
| `add_future_option()` | MISSING | |
| `add_future_option_contract()` | MISSING | |
| `add_index_option()` | MISSING | No index options |
| `add_option_contract(symbol, ...)` | MISSING | Adding specific contract by symbol |
| `add_data(ticker, resolution, ...)` | MISSING | Custom data types |
| `add_cfd()` | MISSING | |
| `add_index()` | MISSING | |
| `add_crypto_future()` | MISSING | |
| `universe` property | MISSING | UniverseDefinitions object |
| `add_universe()` (all overloads) | MISSING | Universe selection/filtering |
| `add_universe_options()` | MISSING | Option filter universe |
| `remove_security()` | MISSING | Dynamic removal |
| `remove_option_contract()` | MISSING | |

**Action:** For immediate need (spy_wheel): current `add_option()` is sufficient. For full LEAN parity, need futures + index options support.

---

### 3. Order Methods

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `buy(symbol, quantity)` | MISSING | Simplified buy order (not implemented) |
| `sell(symbol, quantity)` | MISSING | Simplified sell order (not implemented) |
| `order(symbol, quantity)` | MISSING | Generic order (positive=buy, negative=sell) |
| `market_order(symbol, qty, ...)` | EXISTS | Full signature with optional tag |
| `market_on_open_order()` | MISSING | |
| `market_on_close_order()` | MISSING | |
| `limit_order(symbol, qty, price, ...)` | EXISTS | |
| `stop_limit_order()` | MISSING | |
| `stop_market_order(symbol, qty, stop, ...)` | EXISTS | |
| `limit_if_touched_order()` | MISSING | |
| `trailing_stop_order()` | MISSING | |
| `exercise_option()` | MISSING | Exercise specific option contract |
| `sell_to_open(contract, qty, premium)` | EXISTS | rlean-specific name; returns order_id |
| `buy_to_open(contract, qty, premium)` | EXISTS | rlean-specific |
| `buy_to_close(contract, qty, premium)` | EXISTS | rlean-specific |
| `sell_to_close(contract, qty, premium)` | EXISTS | rlean-specific |
| `set_holdings(symbol, percentage, ...)` | EXISTS | Target portfolio weight |
| `liquidate(symbol=None, ...)` | EXISTS | Close positions |
| `calculate_order_quantity(symbol, target)` | MISSING | Helper to compute quantity for target % |
| `submit_order_request(request)` | MISSING | Low-level request submission |
| `combo_market_order()` | MISSING | Multi-leg orders |
| `combo_limit_order()` | MISSING | |
| `combo_leg_limit_order()` | MISSING | |
| `is_market_open(symbol)` | MISSING | Check market hours |

**Assessment for spy_wheel:** 
- LEAN's simplified `buy()/sell()` not needed — spy_wheel uses `set_holdings()` + `sell_to_open()` directly ✓
- Option orders sufficient for wheel strategy
- **Gap:** No way to check if market is open before submitting orders

---

### 4. Portfolio/Position Access

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `portfolio` property | EXISTS | Returns PyPortfolio object |
| `portfolio.cash` | EXISTS | Returns f64 |
| `portfolio.cash_book` | MISSING | Multi-currency cash management |
| `portfolio.total_portfolio_value` | EXISTS | Property on PyPortfolio |
| `portfolio.total_fees` | MISSING | Cumulative fees |
| `portfolio.margin_remaining` | MISSING | Available margin |
| `portfolio.total_margin_used` | MISSING | Used margin |
| `portfolio.buying_power` | MISSING | |
| `portfolio[symbol]` | EXISTS | Returns PySecurityHolding; accepts Symbol/string |
| `portfolio.invested` | EXISTS | True if ANY position open |
| `portfolio.absolute_invested` | MISSING | Absolute value of invested capital |
| `portfolio.get_holdings()` | MISSING | All holdings as list |
| `securities[symbol]` | **WRONG** | Not exposed in rlean at all. LEAN has `self.securities[symbol].price`. rlean has no `securities` manager. |
| `account_currency` property | MISSING | Current account currency |
| `set_account_currency()` | MISSING | Multi-currency support |
| `set_cash(amount)` | EXISTS | Set initial cash |
| `time` property | EXISTS | Returns Python datetime |
| `utc_time` property | EXISTS | Returns Python datetime |
| `time_zone` property | MISSING | Algorithm timezone |
| `start_date` property | MISSING | Algorithm start date |
| `end_date` property | MISSING | Algorithm end date |

**Assessment for spy_wheel:**
- `portfolio.cash`, `portfolio[symbol].invested`, `portfolio_value` all work ✓
- Missing `securities[symbol].price` — workaround: must pass spot price manually from bar or use OptionChain.underlying_price
- **Fix needed:** Add `securities` manager or expose price lookups via alternative API

---

### 5. Option-Specific Methods

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `get_option_chain(canonical_ticker)` | EXISTS | Returns PyOptionChain; rlean-specific name |
| `get_option_positions()` | EXISTS | Returns list of dicts (rlean custom format) |
| `option_chain(symbol)` | MISSING | LEAN API name; rlean uses `get_option_chain()` |
| `option_chains(symbols)` | MISSING | Get multiple chains |
| **Option Chain Data Access (CRITICAL)** | **WRONG** | See below |
| `slice.option_chains[symbol]` | **WRONG** | LEAN delivers option chains via `slice.option_chains`. rlean's Slice does NOT have `option_chains` property. Must call `self.get_option_chain()` separately. |
| `chain.underlying` | EXISTS | Property on PyOptionChain |
| `chain.underlying_price` | EXISTS | Property on PyOptionChain |
| `chain.expiry_dates` | MISSING | List of available expirations |
| `chain.contracts` | EXISTS | Method returns list of PyOptionContract |
| `chain.calls()` | EXISTS | Filter for calls |
| `chain.puts()` | EXISTS | Filter for puts |
| `contract.symbol` | EXISTS | As `ticker` property |
| `contract.strike` | EXISTS | Property |
| `contract.right` | EXISTS | Property; returns PyOptionRight |
| `contract.expiry` | EXISTS | Property as ISO string |
| `contract.open_interest` | EXISTS | Property |
| `contract.implied_volatility` | EXISTS | Property |
| `contract.greeks` | EXISTS | Returns PyGreeks object with delta, gamma, vega, theta, rho, lambda |
| `contract.bid_price` | EXISTS | Property |
| `contract.ask_price` | EXISTS | Property |
| `contract.last_price` | EXISTS | Property |
| `contract.mid_price` | EXISTS | Computed property |
| `contract.volume` | EXISTS | Property |
| `contract.intrinsic_value()` | EXISTS | Method |
| `contract.time_value()` | EXISTS | Method |

**CRITICAL ARCHITECTURE ISSUE:** 
LEAN's pattern is:
```python
def on_data(self, slice):
    chain = slice.option_chains[self.spy_option_symbol]  # Access via Slice
```

rlean's pattern is:
```python
def on_data(self, data):
    chain = self.get_option_chain(self.canonical)  # Access via algorithm method
```

This fundamental difference won't break spy_wheel but diverges from LEAN semantics. **Recommendation:** Consider adding `slice.option_chains` property as a PyDict pointing to cached chains, accessible by canonical symbol.

---

### 6. Data Access

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `current_slice` property | MISSING | Property to access current Slice |
| `slice.bars` | EXISTS | TradeBars collection; access via `data.bars[symbol]` or `data.get(symbol)` |
| `slice.quotes` | MISSING | Bid/ask QuoteBar data |
| `slice.option_chains` | **WRONG** | Not on Slice; must use `self.get_option_chain()` |
| `slice.future_chains` | MISSING | Futures chain data |
| `data.get(symbol)` | EXISTS | Get bar for symbol |
| `data.get_bar(symbol)` | EXISTS | Alias for get() |
| `data[symbol]` | EXISTS | Dict-like access |
| `history(symbols, periods, ...)` | MISSING | Get historical bars |
| `history(symbols, start, end, ...)` | MISSING | Historical date range |
| `get_last_known_prices()` | MISSING | Last data for symbol(s) |
| `get_last_known_price()` | MISSING | Single last data point |
| `fundamentals(symbol)` | MISSING | Fundamental data (P/E, sector, etc.) |
| `future_chain(symbol)` | MISSING | Futures chain access |

**Assessment for spy_wheel:**
- Current `data.get(symbol)` and `self.get_option_chain()` sufficient ✓
- No historical data needs for wheel

---

### 7. Indicators

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| **Direct indicator creation (rlean approach)** | EXISTS | `SimpleMovingAverage(period)` constructor |
| `indicator.update(time, value)` | EXISTS | Accepts time (ignored) or just value |
| `indicator.update_bar(bar)` | EXISTS | Update from TradeBar |
| `indicator.is_ready` | EXISTS | Property |
| `indicator.current` | EXISTS | Returns IndicatorDataPoint with `.value` |
| `indicator.current.value` | EXISTS | Float value |
| `indicator.value` | EXISTS | Shorthand for current.value |
| `indicator.samples` | EXISTS | Count of updates |
| `indicator.reset()` | EXISTS | Clear indicator |
| **Indicator shortcuts (rlean missing)** | MISSING | LEAN provides ~100+ shortcut methods like `self.sma(symbol, period)` that auto-subscribe. rlean requires manual indicator creation. |
| `self.sma()` | MISSING | |
| `self.ema()` | MISSING | |
| `self.rsi()` | MISSING | |
| `self.macd()` | MISSING | |
| `self.bollinger_bands()` | MISSING | |
| `self.atr()` | MISSING | |
| All other technical indicators | MISSING | 200+ shortcut methods |
| `register_indicator()` | MISSING | Register indicator for updates |
| `warm_up_indicator()` | MISSING | Auto-fill on subscription |

**rlean provides:**
- Core indicators as classes: `SimpleMovingAverage`, `ExponentialMovingAverage`, `RelativeStrengthIndex`, `MovingAverageConvergenceDivergence`, `BollingerBands`, `AverageTrueRange`
- Manual instantiation + update pattern (more control, less ergonomic)

**LEAN provides:**
- All indicators as shortcut methods on algorithm + auto-subscription
- Cleaner syntax but less explicit

**Assessment:** spy_wheel doesn't use indicators, so this gap is N/A. For general strategies, rlean's approach is workable but verbose.

---

### 8. Logging & Debugging

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `log(message)` | EXISTS | Basic logging |
| `debug(message)` | EXISTS | Debug-level logging |
| `error(message)` | MISSING | Error logging (not implemented) |
| `error(exception)` | MISSING | Exception logging |
| `quit(message)` | MISSING | Algorithm termination with message |

**Assessment:** `log()` works; missing error-specific methods but workaround possible.

---

### 9. Configuration/Settings

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `set_start_date(year, month, day)` | EXISTS | |
| `set_end_date(year, month, day)` | EXISTS | |
| `set_cash(amount)` | EXISTS | |
| `set_name(name)` | EXISTS | |
| `set_benchmark(ticker)` | EXISTS | Defaults to SPY if not set |
| `set_warm_up(bars_or_days)` | EXISTS | Accepts bars (>365) or days (<=365) |
| `set_brokerage_model()` | MISSING | No brokerage models |
| `set_risk_free_interest_rate_model()` | MISSING | |
| `set_account_currency()` | MISSING | Multi-currency |
| `add_tag()` | MISSING | Algorithm tagging |
| `set_tags()` | MISSING | |
| `set_maximum_orders()` | MISSING | Order rate limiting |
| `get_parameter()` | MISSING | Parameter passing |
| `set_parameters()` | MISSING | |
| `set_default_order_properties()` | MISSING | Order defaults |
| `set_algorithm_mode()` | MISSING | Backtesting vs. live |
| `set_time_zone()` | MISSING | Timezone (defaults to UTC?) |
| `universe_settings` property | MISSING | Default universe config |

**Assessment for spy_wheel:** Basic settings work; missing advanced config but not needed for backtest.

---

### 10. Charting/Plotting

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `plot(series_name, value)` | EXISTS | Single value plot |
| `plot(chart_name, series_name, value)` | EXISTS | Named chart + series |
| `add_chart(name)` | EXISTS | Ensure chart exists |
| `plot(chart, series, trade_bar)` | MISSING | Plot OHLC bar directly |
| `add_series()` | MISSING | Add series to existing chart |
| `get_chart_updates()` | MISSING | Retrieve chart data |
| `plot_indicator()` | MISSING | Plot indicator on chart |
| `set_runtime_statistic()` | MISSING | Runtime stats |
| `set_summary_statistic()` | MISSING | Summary stats |
| `record()` | MISSING | Record value to chart |

**Assessment:** Basic plotting works. spy_wheel doesn't use charting, so not blocking.

---

### 11. Scheduling

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `schedule` property | MISSING | ScheduleManager for `schedule.on(date_rule, time_rule, callback)` |
| `date_rules.*` | MISSING | Predefined date rules (everyday, week_start, month_end, etc.) |
| `time_rules.*` | MISSING | Predefined time rules (at, every_minute, before_market_close, etc.) |

**Assessment:** Not needed for spy_wheel (daily on_data is sufficient).

---

### 12. Risk/Margin Methods

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `shortable(symbol)` | MISSING | Check if shortable |
| `shortable_quantity(symbol)` | MISSING | Get available short quantity |
| `on_margin_call()` | MISSING | Margin call handler |
| `on_margin_call_warning()` | MISSING | Margin warning handler |

**Assessment:** Not critical for wheel (no shorting equity, options handled by framework).

---

### 13. Advanced/Framework Features

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `debug_mode` property | MISSING | |
| `universe_selection` property | MISSING | IUniverseSelectionModel |
| `alpha` property | MISSING | IAlphaModel |
| `portfolio_construction` property | MISSING | IPortfolioConstructionModel |
| `execution` property | MISSING | IExecutionModel |
| `risk_management` property | MISSING | IRiskManagementModel |
| `insights` property | MISSING | InsightManager |
| All framework setters | MISSING | `set_universe_selection()`, `set_alpha()`, etc. |

**Assessment:** Algorithm framework abstraction layer not ported. Not needed for simple strategies like spy_wheel.

---

### 14. Warm-up

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `set_warm_up(bars_or_days)` | EXISTS | Accepts bar count or days |
| `is_warming_up` property | EXISTS | Check if in warm-up period |
| `on_warmup_finished()` | EXISTS | Callback when ready |

**Assessment:** Fully implemented ✓

---

### 15. Utilities & Metadata

| LEAN API Method | rlean Status | Notes |
|---|---|---|
| `symbol(ticker)` | MISSING | Resolve ticker to Symbol |
| `ticker(symbol)` | MISSING | Get ticker from Symbol |
| `isin()`, `cusip()`, `sedol()`, `cik()` | MISSING | Identifier mapping |
| `object_store` property | MISSING | Persistent data store |
| `notify` property | MISSING | Notifications (email, Slack, etc.) |
| `download()` | MISSING | Download from URL |
| `trading_calendar` property | MISSING | Market calendar |
| `consolidate()` | MISSING | Data consolidation |
| `name` property/method | EXISTS | Get/set algorithm name |
| `status` property | MISSING | Current AlgorithmStatus |
| `algorithm_id` property | MISSING | Algorithm ID string |
| `live_mode` property | MISSING | True if live trading |
| `benchmark` property | MISSING | Benchmark object |
| `brokerage_model` property | MISSING | Brokerage configuration |

**Assessment:** Metadata mostly missing; not critical for backtest.

---

## Critical Issues Requiring Action

### 1. **Option Chain Access Pattern** (ARCHITECTURE)
**Severity:** MEDIUM (workaround exists)

**Issue:**
- LEAN: `chain = slice.option_chains[symbol]`
- rlean: `chain = self.get_option_chain(canonical_symbol)`

**Impact:** Code incompatibility at Slice level; requires different pattern in on_data().

**Recommendation:** Add `slice.option_chains` as a property returning dict-like access to option chains. Cache chains in the Slice proxy.

---

### 2. **on_assignment_order_event Signature** (BREAKING)
**Severity:** HIGH (affects option strategies)

**Issue:**
- LEAN: `on_assignment_order_event(order_event: OrderEvent)`
- rlean: `on_assignment_order_event(contract, quantity, is_assignment: bool)`

**Impact:** Fundamentally different API. spy_wheel currently uses rlean signature; migrating to LEAN will break strategy.

**Current spy_wheel code:**
```python
def on_assignment_order_event(self, contract, quantity, is_assignment):
    if is_assignment:
        self._state = "long_stock"
```

**LEAN API code would be:**
```python
def on_assignment_order_event(self, event):
    if event.is_assignment and event.symbol.is_put():
        self._state = "long_stock"
```

**Recommendation:** Standardize to LEAN signature. Create OrderEvent wrapper with `is_assignment` field (already exists in py_orders.rs).

---

### 3. **Missing securities Manager** (MAJOR)
**Severity:** MEDIUM (workaround needed)

**Issue:**
- LEAN: `self.securities[symbol].price`
- rlean: No `securities` manager exposed

**Impact:** No direct price lookups outside on_data(). Must pass prices manually or extract from bars/chains.

**Current spy_wheel workaround:**
```python
spot = bar.close  # From data in on_data()
underlying_price = chain.underlying_price  # From option chain
```

**Recommendation:** Add `securities` manager property or expose `get_price(symbol)` method.

---

### 4. **Indicator Shortcuts Missing** (FEATURE GAP)
**Severity:** LOW (for wheel; HIGH for complex strategies)

**Issue:**
- LEAN: `self.sma(symbol, 50)`
- rlean: `self.sma_ind = SimpleMovingAverage(50); self.sma_ind.update(time, price)`

**Impact:** More verbose indicator usage; no auto-subscription.

**Recommendation:** Not urgent for spy_wheel. For general use, add shortcut methods to PyQcAlgorithm that create + register indicators.

---

### 5. **Missing Lifecycle Hooks** (COMPLETENESS)
**Severity:** LOW (for wheel; MEDIUM for complex strategies)

**Missing:**
- `post_initialize()`
- `on_end_of_day(symbol)`
- Corporate action callbacks (splits, dividends, delistings)
- Securities universe change notifications

**Impact:** Limited ability to react to market events beyond bars.

---

## Implementation Priority for LEAN Parity

### Phase 1: Critical (breaks compatibility)
1. Standardize `on_assignment_order_event` to LEAN signature
2. Add `slice.option_chains` property for LEAN-compatible access
3. Add `securities` manager or `get_price(symbol)` method

### Phase 2: High (expected patterns)
4. Add `post_initialize()` callback
5. Add `on_end_of_day()` callbacks (symbol-specific + general)
6. Add basic lifecycle (corporate actions: splits, dividends)
7. Add indicator shortcut methods (sma, ema, rsi, etc.)

### Phase 3: Medium (common features)
8. Add universe selection support (needed for dynamic universes)
9. Add fundamental data access (for stock screening)
10. Add scheduling (on/date_rules/time_rules)
11. Add margin call handlers

### Phase 4: Nice-to-have
12. Add all 100+ indicator shortcuts
13. Add advanced framework abstractions
14. Add multi-currency support
15. Add brokerage models

---

## Assessment for spy_wheel Strategy

### Current Implementation Status: **WORKS WITH WORKAROUNDS**

**What Works:**
- ✓ `add_option(ticker)` — returns canonical string
- ✓ `get_option_chain(canonical)` — accesses chain
- ✓ `chain.calls()` / `chain.puts()` — filters contracts
- ✓ `contract.strike`, `contract.expiry`, `contract.right`, `contract.mid_price` — contract details
- ✓ `sell_to_open()`, `buy_to_open()`, `buy_to_close()`, `sell_to_close()` — option orders
- ✓ `get_option_positions()` — track open positions
- ✓ `portfolio[symbol].is_invested`, `portfolio.cash`, `portfolio_value` — portfolio state
- ✓ `self.time`, `is_warming_up` — timing
- ✓ `set_cash()`, `set_start_date()`, `set_end_date()` — configuration
- ✓ `log()` — logging
- ✓ `plot()` — charting (not used but available)

**Workarounds/Differences:**
- `on_assignment_order_event(contract, quantity, is_assignment)` is rlean-specific (not LEAN-compatible)
- Must manually get spot price from `bar.close` or `chain.underlying_price` (no `securities[symbol].price`)
- Must call `get_option_chain()` explicitly (LEAN uses `slice.option_chains[symbol]`)

**Gaps that don't affect spy_wheel:**
- No history() — all decisions made on current bar
- No indicators — pure price-based logic
- No universe selection — static SPY + options
- No scheduling — daily on_data is sufficient
- No margin calls — not applicable to wheel
- No fundamental data — not needed

**Verdict:** **spy_wheel runs successfully on current rlean implementation.** Migrating to full LEAN would require:
1. Change `on_assignment_order_event` signature
2. Change option chain access from `self.get_option_chain()` to `data.option_chains[symbol]` (if implemented)

---

## Appendix: Test Coverage Matrix

Methods actively used by spy_wheel (all PASS):
- `initialize()` ✓
- `set_start_date()`, `set_end_date()`, `set_cash()` ✓
- `add_equity()`, `add_option()` ✓
- `on_data()` ✓
- `data.get(symbol)` ✓
- `self.get_option_chain()` ✓
- `chain.underlying_price`, `chain.calls()`, `chain.puts()` ✓
- `contract.strike`, `contract.expiry`, `contract.right`, `contract.mid_price` ✓
- `sell_to_open()`, `get_option_positions()` ✓
- `portfolio[symbol].is_invested`, `self.cash`, `self.portfolio_value` ✓
- `on_assignment_order_event()` (rlean signature) ✓
- `on_end_of_algorithm()` ✓
- `log()` ✓

Methods NOT used by spy_wheel but expected in LEAN:
- All indicator shortcuts (low priority for wheel)
- All ordering methods except market_order + option orders
- History + fundamental data
- Scheduling + universe selection
- Framework abstractions
- Multi-currency + margin management

---

**End of Gap Analysis**
