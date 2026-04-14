# LEAN QCAlgorithm Python API Inventory

Comprehensive reference of all public methods and properties available on LEAN's `QCAlgorithm` base class that Python trading strategies inherit from.

**Note:** Python snake_case method names are typically the C# camelCase names converted to lowercase with underscores. For example, `SetStartDate()` becomes `set_start_date()`.

---

## Table of Contents
1. [Lifecycle Callbacks](#lifecycle-callbacks)
2. [Universe/Subscription Methods](#universesubscription-methods)
3. [Order Methods](#order-methods)
4. [Portfolio/Position Access](#portfolioposition-access)
5. [Option-Specific Methods](#option-specific-methods)
6. [Data Access](#data-access)
7. [Indicators](#indicators)
8. [Scheduling](#scheduling)
9. [Logging](#logging)
10. [Settings](#settings)
11. [Warm-up](#warm-up)
12. [Risk/Margin Methods](#riskmargin-methods)
13. [Charting/Plotting](#chartingplotting)
14. [Framework/Advanced Features](#frameworkadvanced-features)
15. [Fundamental Data](#fundamental-data)
16. [Additional Utilities](#additional-utilities)

---

## Lifecycle Callbacks

These are virtual methods that you override in your Python algorithm class to handle different phases and events.

### Initialization & Finalization
- `initialize()` - Called at algorithm startup to set up subscriptions, universe selections, etc.
- `post_initialize()` - Called after Initialize() has been run and before algorithm trading begins
- `on_warmup_finished()` - Called when the warm-up period is complete

### Data Events
- `on_data(slice)` - Main data event handler, called when new data arrives
  - **Parameters:** `slice` (Slice object containing the current data)

### Order & Trade Events
- `on_order_event(order_event)` - Called when an order status changes
  - **Parameters:** `order_event` (OrderEvent object)
- `on_assignment_order_event(assignment_event)` - Called on option assignment
  - **Parameters:** `assignment_event` (OrderEvent for assignment)

### Corporate Actions
- `on_splits(splits)` - Called when stock splits occur
  - **Parameters:** `splits` (Splits object)
- `on_dividends(dividends)` - Called when dividends are received
  - **Parameters:** `dividends` (Dividends object)
- `on_delistings(delistings)` - Called when securities are delisted
  - **Parameters:** `delistings` (Delistings object)
- `on_symbol_changed_events(symbols_changed)` - Called on symbol changes (mergers, etc.)
  - **Parameters:** `symbols_changed` (SymbolChangedEvents object)

### Securities Changes
- `on_securities_changed(changes)` - Called when universe additions/removals occur
  - **Parameters:** `changes` (SecurityChanges object)

### End of Day
- `on_end_of_day()` - Called at end of trading day (no symbol specified)
- `on_end_of_day(symbol)` - Called at end of trading day for a specific symbol
  - **Parameters:** `symbol` (string or Symbol)

### Algorithm Lifecycle
- `on_end_of_algorithm()` - Called when algorithm terminates

### Margin & Risk
- `on_margin_call(requests)` - Called when margin call occurs
  - **Parameters:** `requests` (List[SubmitOrderRequest])
- `on_margin_call_warning()` - Called to warn of impending margin call

### Brokerage Events
- `on_brokerage_message(message_event)` - Called when brokerage sends a message
  - **Parameters:** `message_event` (BrokerageMessageEvent)
- `on_brokerage_disconnect()` - Called when connection to brokerage is lost
- `on_brokerage_reconnect()` - Called when connection to brokerage is re-established

### Custom Commands
- `on_command(data)` - Called when a command is received (advanced feature)
  - **Parameters:** `data` (dynamic command data)
  - **Returns:** `bool?` (optional response)

---

## Universe/Subscription Methods

### Add Securities
- `add_equity(ticker, resolution=None, market=None, fill_forward=True, leverage=1.0, extended_market_hours=None)`
- `add_option(underlying, resolution=None, market=None, fill_forward=None, leverage=1.0)` - Add option chain
- `add_option(underlying_symbol, resolution=None, market=None, fill_forward=None, leverage=1.0)`
- `add_option(underlying_symbol, target_option, resolution=None, ...)`
- `add_future(ticker, resolution=None, market=None, ...)`
- `add_future_contract(symbol, resolution=None, fill_forward=True, ...)`
- `add_future_option(symbol, option_filter=None)` - Add futures option chain
- `add_future_option_contract(symbol, resolution=None, fill_forward=True, ...)`
- `add_index_option(underlying, resolution=None, market=None, fill_forward=True)` - Index options
- `add_index_option(symbol, resolution=None, fill_forward=True)`
- `add_index_option(symbol, target_option, resolution=None, fill_forward=True, ...)`
- `add_index_option_contract(symbol, resolution=None, fill_forward=True, ...)`
- `add_option_contract(symbol, resolution=None, fill_forward=True, ...)` - Add specific option contract
- `add_forex(ticker, resolution=None, market=None, fill_forward=True, leverage=1.0)`
- `add_cfd(ticker, resolution=None, market=None, fill_forward=True, leverage=1.0)`
- `add_index(ticker, resolution=None, market=None, fill_forward=True)`
- `add_crypto(ticker, resolution=None, market=None, fill_forward=True, leverage=1.0)`
- `add_crypto_future(ticker, resolution=None, market=None, fill_forward=True, leverage=1.0)`
- `add_security(security_type, ticker, resolution=None, fill_forward=None, leverage=1.0, extended_market_hours=None)`
- `add_security(symbol, resolution=None, fill_forward=None, leverage=1.0, extended_market_hours=None)`

### Add Custom Data
- `add_data(ticker_or_symbol, resolution=None)` - Generic custom data (type passed in with `<T>`)
- `add_data(ticker_or_symbol, resolution=None, fill_forward=False, leverage=1.0)`
- `add_data(ticker_or_symbol, resolution=None, time_zone=None, fill_forward=False, leverage=1.0)`

### Universe Selection
- `universe` - Property: UniverseDefinitions object for adding universes
- `add_universe(universe)` - Add a pre-built universe
- `add_universe(selector)` - Generic universe with custom data type selector
  - **Parameters:** `selector` (Func[IEnumerable[BaseData], IEnumerable[Symbol]])
- `add_universe(name, selector)` - Named universe
- `add_universe(name, resolution, selector)`
- `add_universe(security_type, name, resolution, market, selector)`
- `add_universe(selector)` - Fundamental data universe (coarse filtering)
- `add_universe(date_rule, selector)` - Universe with schedule
- `add_universe(coarse_selector, fine_selector)` - Coarse and fine filtering
- `add_universe(name, selector)` - Custom date-based selector
- `add_universe_options(underlying_symbol, option_filter)` - Add option universe filter
- `add_universe_options(universe, option_filter)`

### Remove Securities
- `remove_security(symbol, tag=None)` - Remove a security from the algorithm
- `remove_option_contract(symbol, tag=None)` - Remove a specific option contract

---

## Order Methods

### Simple Market Orders
- `buy(symbol, quantity)` - Buy a quantity of a security
  - **Parameters:** `symbol` (Symbol or string), `quantity` (int/float/decimal)
  - **Returns:** OrderTicket
- `sell(symbol, quantity)` - Sell a quantity of a security
- `order(symbol, quantity)` - Generic order (positive = buy, negative = sell)

### Market Orders
- `market_order(symbol, quantity, asynchronous=False, tag="", order_properties=None)`
- `market_on_open_order(symbol, quantity, asynchronous=False, tag="", order_properties=None)` - MOO
- `market_on_close_order(symbol, quantity, asynchronous=False, tag="", order_properties=None)` - MOC

### Limit Orders
- `limit_order(symbol, quantity, limit_price, asynchronous=False, tag="", order_properties=None)`
- `stop_limit_order(symbol, quantity, stop_price, limit_price, asynchronous=False, tag="", order_properties=None)`
- `limit_if_touched_order(symbol, quantity, trigger_price, limit_price, asynchronous=False, tag="", order_properties=None)` - LIT

### Stop Orders
- `stop_market_order(symbol, quantity, stop_price, asynchronous=False, tag="", order_properties=None)`
- `trailing_stop_order(symbol, quantity, trailing_amount, trailing_as_percentage, asynchronous=False, tag="", order_properties=None)`
- `trailing_stop_order(symbol, quantity, stop_price, trailing_amount, trailing_as_percentage, asynchronous=False, tag="", order_properties=None)`

### Option Orders
- `exercise_option(option_symbol, quantity, asynchronous=False, tag="", order_properties=None)` - Exercise option

### Strategy Orders
- `buy(option_strategy, quantity, asynchronous=False, tag="", order_properties=None)` - Buy strategy
- `sell(option_strategy, quantity, asynchronous=False, tag="", order_properties=None)` - Sell strategy
- `order(option_strategy, quantity, asynchronous=False, tag="", order_properties=None)` - Generic strategy order

### Combo Orders
- `combo_market_order(legs, quantity, asynchronous=False, tag="", order_properties=None)`
- `combo_limit_order(legs, quantity, limit_price, asynchronous=False, tag="", order_properties=None)`
- `combo_leg_limit_order(legs, quantity, asynchronous=False, tag="", order_properties=None)`

### Low-Level Order Submission
- `submit_order_request(request)` - Submit a SubmitOrderRequest directly
  - **Parameters:** `request` (SubmitOrderRequest)
  - **Returns:** OrderTicket

### Portfolio Management
- `set_holdings(symbol, percentage, liquidate_existing_holdings=False, asynchronous=False, tag=None, order_properties=None)` - Set portfolio target
  - **Parameters:** `symbol` (Symbol), `percentage` (decimal 0-1)
- `set_holdings(targets, liquidate_existing_holdings=False, asynchronous=False, tag=None, order_properties=None)` - Multiple targets
  - **Parameters:** `targets` (List[PortfolioTarget])
- `liquidate(symbol=None, asynchronous=False, tag=None, order_properties=None)` - Close all positions
- `liquidate(symbols, asynchronous=False, tag=None, order_properties=None)` - Close specific symbols
- `calculate_order_quantity(symbol, target)` - Calculate quantity for target percentage
  - **Parameters:** `symbol` (Symbol), `target` (decimal)
  - **Returns:** decimal

### Order Status
- `is_market_open(symbol)` - Check if market is open
  - **Parameters:** `symbol` (Symbol)
  - **Returns:** bool

---

## Portfolio/Position Access

### Main Portfolio Objects
- `portfolio` - Property: SecurityPortfolioManager containing all holdings
- `securities` - Property: SecurityManager containing all security objects
- `active_securities` - Property: Read-only dictionary of active securities
- `transactions` - Property: SecurityTransactionManager for tracking trades

### Cash & Valuation
- `portfolio.cash` - Current cash available
- `portfolio.cash_book` - CashBook for multi-currency cash management
- `portfolio.total_portfolio_value` - Total portfolio value
- `portfolio.total_fees` - Cumulative fees paid
- `portfolio.margin_remaining` - Available margin
- `portfolio.total_margin_used` - Used margin
- `portfolio.buying_power` - Available buying power
- `account_currency` - Property: Account currency code (e.g., "USD")
- `set_account_currency(currency, starting_cash=None)` - Set account currency
- `set_cash(amount)` - Set initial cash
  - **Parameters:** `amount` (decimal/double/int)

### Position Information
- `portfolio[symbol]` - Access security holding for a symbol
  - **Returns:** SecurityHolding
- `portfolio.get_holdings()` - Get all holdings
- `portfolio.invested` - Check if portfolio has positions
- `portfolio.absolute_invested` - Absolute value of invested capital

### Time/Status
- `time` - Property: Current algorithm time (DateTime)
- `utc_time` - Property: Current UTC time
- `time_zone` - Property: Algorithm timezone
- `start_date` - Property: Algorithm start date
- `end_date` - Property: Algorithm end date

---

## Option-Specific Methods

### Option Chain Access
- `option_chain(symbol, flatten=False)` - Get option chain for a symbol
  - **Parameters:** `symbol` (Symbol)
  - **Returns:** OptionChain
- `option_chains(symbols, flatten=False)` - Get multiple option chains
  - **Parameters:** `symbols` (IEnumerable[Symbol])
  - **Returns:** OptionChains

### OptionChain API
The `OptionChain` object returned has:
- `underlying` - Symbol of the underlying asset
- `expiry_dates` - List of available expiration dates
- `contracts` - List of OptionContract objects
- `contracts.filter()` - Filter contracts by criteria

### OptionContract API
Contract objects have:
- `symbol` - The contract's Symbol
- `strike` - Strike price
- `right` - Call or Put
- `expiry` - Expiration date
- `open_interest` - Open interest
- `implied_volatility` - IV (from data)
- `greeks` - Access to Greeks (delta, gamma, vega, theta, rho)

### Option Filtering (Universe)
When using `add_option()` or universe options:
```python
def option_filter(self, contracts):
    return contracts.where(lambda x: x.strike < self.underlyings.price * 1.1)
```

---

## Data Access

### Historical Data
- `history(symbols, periods, resolution=None, fill_forward=None, extended_market_hours=None)` - Get historical bars
  - **Parameters:** `symbols` (Symbol or list), `periods` (int), `resolution` (Resolution)
  - **Returns:** DataFrame or dict of DataFrames
- `history(symbols, time_span, resolution=None, fill_forward=None, extended_market_hours=None)` - Historical with time span
  - **Parameters:** `time_span` (TimeSpan)
- `history(symbols, start, end, resolution=None, fill_forward=None, extended_market_hours=None)` - Historical date range
  - **Parameters:** `start` (DateTime), `end` (DateTime)
- `history(type, symbols, periods, resolution=None, fill_forward=None)` - Custom data type history
- `history(type, symbols, time_span, resolution=None, fill_forward=None)`
- `history(type, symbols, start, end, resolution=None, fill_forward=None)`
- `history(request)` - Get history from HistoryRequest object
- `history(requests)` - Get history from multiple HistoryRequest objects

### Last Known Prices
- `get_last_known_prices(symbol)` - Get last data points for a symbol
  - **Returns:** IEnumerable[BaseData]
- `get_last_known_prices(symbols)` - Get last data for multiple symbols
  - **Returns:** DataDictionary[IEnumerable[BaseData]]
- `get_last_known_price(symbol)` - Get single last data point
  - **Returns:** BaseData

### Current Data
- `current_slice` - Property: Current Slice object with latest data
- `slice.bars` - TradeBar data for all symbols
- `slice.quotes` - QuoteBar data (bid/ask)
- `slice.option_chains` - OptionChain data
- `slice.future_chains` - FutureChain data

### Fundamental Data
- `fundamentals(symbol)` - Get fundamental data for a symbol
  - **Returns:** Fundamental object
- `fundamentals(symbols)` - Get fundamentals for multiple symbols
  - **Returns:** List[Fundamental]

### Future Chain Data
- `future_chain(symbol, flatten=False)` - Get futures chain
  - **Returns:** FuturesChain
- `future_chains(symbols, flatten=False)` - Get multiple futures chains
  - **Returns:** FuturesChains

---

## Indicators

LEAN provides hundreds of technical indicators as shortcut methods. These are called directly on the algorithm instance. Each takes:
- **Required:** `symbol` (Symbol)
- **Optional:** `resolution` (Resolution), `selector` (Func to extract price)

### Moving Averages
- `sma(symbol, period, resolution=None, selector=None)` - Simple Moving Average
- `ema(symbol, period, resolution=None, selector=None)` - Exponential Moving Average
- `ema(symbol, period, smoothing_factor, resolution=None, selector=None)` - EMA with custom smoothing
- `dema(symbol, period, resolution=None, selector=None)` - Double EMA
- `tema(symbol, period, resolution=None, selector=None)` - Triple EMA
- `kama(symbol, period, resolution=None, selector=None)` - Kaufman Adaptive MA
- `kama(symbol, period, fast_ema_period, slow_ema_period, resolution=None, selector=None)`
- `hma(symbol, period, resolution=None, selector=None)` - Hull Moving Average
- `alma(symbol, period, sigma=6, offset=0.85, resolution=None, selector=None)` - Arnaud Legoux MA
- `mama(symbol, fast_limit=0.5, slow_limit=0.05, resolution=None, selector=None)` - MESA Adaptive MA
- `wma(symbol, period, resolution=None, selector=None)` - Weighted MA (deprecated, use wwma)
- `wwma(symbol, period, resolution=None, selector=None)` - Wilder Moving Average
- `lsma(symbol, period, resolution=None, selector=None)` - Least Squares MA
- `rma(symbol, period, resolution=None, selector=None)` - Relative Moving Average
- `lwma(symbol, period, resolution=None, selector=None)` - Linear Weighted MA
- `zlema(symbol, period, resolution=None, selector=None)` - Zero Lag EMA
- `trima(symbol, period, resolution=None, selector=None)` - Triangular MA
- `t3(symbol, period, volume_factor=0.7, resolution=None, selector=None)` - T3 Moving Average
- `vidya(symbol, period, resolution=None, selector=None)` - Variable Index Dynamic Average

### Momentum & Oscillators
- `rsi(symbol, period, moving_average_type=MovingAverageType.Wilders, resolution=None, selector=None)` - Relative Strength Index
- `stochastic(symbol, period, k_period, d_period, resolution=None, selector=None)` - Stochastic
- `sto(symbol, period, resolution=None, selector=None)` - Stochastic (alternate)
- `srsi(symbol, rsi_period, stoch_period, k_smoothing_period, d_smoothing_period, resolution=None, selector=None)` - Stochastic RSI
- `macd(symbol, fast_period, slow_period, signal_period, type=MovingAverageType.Exponential, resolution=None, selector=None)` - MACD
- `mom(symbol, period, resolution=None, selector=None)` - Momentum
- `roc(symbol, period, resolution=None, selector=None)` - Rate of Change
- `rocp(symbol, period, resolution=None, selector=None)` - ROC Percent
- `rocr(symbol, period, resolution=None, selector=None)` - ROC Ratio
- `apo(symbol, fast_period, slow_period, moving_average_type, resolution=None, selector=None)` - Absolute Price Oscillator
- `ppo(symbol, fast_period, slow_period, moving_average_type, resolution=None, selector=None)` - Percentage Price Oscillator
- `ao(symbol, fast_period, slow_period, type, resolution=None, selector=None)` - Awesome Oscillator
- `kst(symbol, ...)` - Know Sure Thing
- `tsi(symbol, long_term_period=25, short_term_period=13, signal_period=7, resolution=None, selector=None)` - True Strength Index
- `trix(symbol, period, resolution=None, selector=None)` - TRIX
- `dpo(symbol, period, resolution=None, selector=None)` - Detrended Price Oscillator
- `cmo(symbol, period, resolution=None, selector=None)` - Chande Momentum Oscillator

### Volatility & Bands
- `bollinger_bands(symbol, period, k, moving_average_type=MovingAverageType.Simple, resolution=None, selector=None)` - Bollinger Bands
- `bb(symbol, period, k, moving_average_type=MovingAverageType.Simple, resolution=None, selector=None)` - Bollinger Bands (alias)
- `keltner_channels(symbol, period, k, moving_average_type=MovingAverageType.Simple, resolution=None, selector=None)` - Keltner Channels
- `kch(symbol, period, k, moving_average_type=MovingAverageType.Simple, resolution=None, selector=None)` - Keltner (alias)
- `atr(symbol, period, type=MovingAverageType.Simple, resolution=None, selector=None)` - Average True Range
- `natr(symbol, period, resolution=None, selector=None)` - Normalized ATR
- `true_range(symbol, resolution=None, selector=None)` - True Range
- `tr(symbol, resolution=None, selector=None)` - True Range (alias)
- `std(symbol, period, resolution=None, selector=None)` - Standard Deviation
- `var(symbol, period, resolution=None, selector=None)` - Variance
- `donchian_channel(symbol, period, resolution=None, selector=None)` - Donchian Channel
- `dch(symbol, period, resolution=None, selector=None)` - Donchian (alias)
- `regression_channel(symbol, period, k, resolution=None, selector=None)` - Regression Channel

### Volume
- `obv(symbol, resolution=None, selector=None)` - On-Balance Volume
- `sobv(symbol, period, type=MovingAverageType.Simple, resolution=None, selector=None)` - Smoothed OBV
- `vwap(symbol, period, resolution=None, selector=None)` - Volume Weighted Average Price
- `vwap(symbol)` - Intraday VWAP (no period)
- `vwma(symbol, period, resolution=None, selector=None)` - Volume Weighted MA
- `ad(symbol, resolution=None, selector=None)` - Accumulation/Distribution
- `adosc(symbol, fast_period, slow_period, resolution=None, selector=None)` - Acc/Dist Oscillator
- `cmf(symbol, period, resolution=None, selector=None)` - Chaikin Money Flow
- `mfi(symbol, period, resolution=None, selector=None)` - Money Flow Index
- `kvo(symbol, fast_period, slow_period, signal_period=13, resolution=None, selector=None)` - Klinger Volume Oscillator

### Trend & Reversal
- `adx(symbol, period, resolution=None, selector=None)` - Average Directional Index
- `adxr(symbol, period, resolution=None, selector=None)` - ADX Rating
- `aroon(symbol, period, resolution=None, selector=None)` - Aroon
- `aroon(symbol, up_period, down_period, resolution=None, selector=None)` - Aroon (custom periods)
- `cci(symbol, period, moving_average_type=MovingAverageType.Simple, resolution=None, selector=None)` - Commodity Channel Index
- `psar(symbol, af_start=0.02, af_increment=0.02, af_max=0.2, resolution=None, selector=None)` - Parabolic SAR
- `sarext(symbol, sar_start=0.0, offset_on_reverse=0.0, ...)` - Parabolic SAR Extended
- `supertrend(symbol, period, multiplier, moving_average_type=MovingAverageType.Wilders, resolution=None, selector=None)` - SuperTrend
- `str(symbol, period, multiplier, resolution=None, selector=None)` - SuperTrend (alias)
- `rvi(symbol, period, moving_average_type=MovingAverageType.Simple, resolution=None, selector=None)` - Relative Vigor Index
- `wilr(symbol, period, resolution=None, selector=None)` - Williams %R
- `vortex(symbol, period, resolution=None, selector=None)` - Vortex Indicator

### High/Low/Range
- `max(symbol, period, resolution=None, selector=None)` - Maximum
- `min(symbol, period, resolution=None, selector=None)` - Minimum
- `midpoint(symbol, period, resolution=None, selector=None)` - Midpoint
- `midprice(symbol, period, resolution=None, selector=None)` - Mid Price
- `ar(symbol, period, resolution=None, selector=None)` - Average Range
- `chop(symbol, period, resolution=None, selector=None)` - Choppiness Index
- `zi(symbol, period, resolution=None, selector=None)` - ZigZag

### Pattern Recognition
- `candlestick_patterns.X()` - Various candlestick patterns (via candlestick_patterns property)
- `tds(symbol, resolution=None, selector=None)` - Tom DeMark Sequential

### Other Technical
- `sum(symbol, period, resolution=None, selector=None)` - Sum
- `beta(target_symbol, reference_symbol, period, resolution=None, selector=None)` - Beta
- `correlation(target_symbol, reference_symbol, period, resolution=None, selector=None)` - Correlation
- `covariance(target_symbol, reference_symbol, period, resolution=None, selector=None)` - Covariance
- `alpha(target_symbol, reference_symbol, alpha_period=1, beta_period=252, resolution=None, selector=None)` - Alpha
- `mass_index(symbol, ema_period=9, sum_period=25, resolution=None, selector=None)` - Mass Index
- `fisher_transform(symbol, period, resolution=None, selector=None)` - Fisher Transform
- `hurst_exponent(symbol, period, max_lag=20, resolution=None, selector=None)` - Hurst Exponent
- `fi(symbol, period, type=MovingAverageType.Exponential, resolution=None, selector=None)` - Force Index
- `frama(symbol, period, long_period=198, resolution=None, selector=None)` - Fractal Adaptive MA
- `log_return(symbol, period, resolution=None, selector=None)` - Log Return
- `ibs(symbol, resolution=None, selector=None)` - Internal Bar Strength
- `mosc(symbols, fast_period=19, slow_period=39, resolution=None, selector=None)` - McClellan Oscillator
- `msi(symbols, fast_period=19, slow_period=39, resolution=None, selector=None)` - McClellan Summation Index
- `trin(symbols, resolution=None, selector=None)` - Arms Index
- `adr(symbols, resolution=None, selector=None)` - Advance Decline Ratio
- `advr(symbols, resolution=None, selector=None)` - Advance Decline Volume Ratio

### Risk-Adjusted Returns
- `sharpe_ratio(symbol, sharpe_period, risk_free_rate=None, resolution=None, selector=None)` - Sharpe Ratio
- `sortino_ratio(symbol, sortino_period, minimum_acceptable_return=0.0, resolution=None, selector=None)` - Sortino Ratio
- `target_downside_deviation(symbol, period, minimum_acceptable_return=0, resolution=None, selector=None)` - Downside Deviation

### Option Greeks
- `delta(symbol, mirror_option=None, risk_free_rate=None, dividend_yield=None, option_model=OptionPricingModelType.BlackScholes, resolution=None, selector=None)` - Delta
- `gamma(symbol, ...)` - Gamma
- `vega(symbol, ...)` - Vega
- `theta(symbol, ...)` - Theta
- `rho(symbol, ...)` - Rho
- `implied_volatility(symbol, ...)` - Implied Volatility

### Indicator Registration & Warm-up
- `register_indicator(symbol, indicator, resolution=None, selector=None)` - Register indicator for updates
- `warm_up_indicator(symbol, indicator, resolution=None, selector=None)` - Warm up an indicator
- `warm_up_indicator(symbol, indicator, time_span, selector=None)` - Warm up with time span
- `unregister_indicator(indicator)` - Stop receiving updates for an indicator
- `indicator_history(indicator, symbol, periods, resolution=None, selector=None)` - Get indicator history
- `create_indicator_name(symbol, type, resolution)` - Generate indicator name

---

## Scheduling

### Schedule Manager
- `schedule` - Property: ScheduleManager for scheduling events
- `schedule.on(date_rule, time_rule, callback)` - Schedule a callback
  - **Example:** `self.schedule.on(self.date_rules.every_day(), self.time_rules.at(15, 30), self.my_function)`

### Date Rules
- `date_rules.everyday()` - Every trading day
- `date_rules.every_day()` - Every day (including non-trading days)
- `date_rules.monday()` - Every Monday
- `date_rules.tuesday()` through `date_rules.friday()` - Specific weekdays
- `date_rules.week_start()` - First trading day of week
- `date_rules.week_end()` - Last trading day of week
- `date_rules.month_start()` - First trading day of month
- `date_rules.month_end()` - Last trading day of month
- `date_rules.quarter_start()` - First trading day of quarter
- `date_rules.quarter_end()` - Last trading day of quarter
- `date_rules.year_start()` - First trading day of year
- `date_rules.year_end()` - Last trading day of year

### Time Rules
- `time_rules.at(hour, minute)` - At specific time
- `time_rules.every_minute()` - Every minute
- `time_rules.every(time_span)` - Every interval
- `time_rules.before_market_close(symbol, minutes)` - Minutes before market close
- `time_rules.after_market_open(symbol, minutes)` - Minutes after market open
- `time_rules.midnight()` - At midnight
- `time_rules.noon()` - At noon

### Training (Scheduled Training Code)
- `train(training_code)` - Schedule training code
  - **Parameters:** `training_code` (Callable)
  - **Returns:** ScheduledEvent
- `train(date_rule, time_rule, training_code)` - Schedule training with rules

---

## Logging

All logging methods output to the Results page and backtest logs.

### Debug Output
- `debug(message)` - Log debug message
  - **Parameters:** `message` (string/int/double/decimal)

### General Logging
- `log(message)` - Log general message
  - **Parameters:** `message` (string/int/double/decimal)

### Error Logging
- `error(message)` - Log error message
  - **Parameters:** `message` (string/int/double/decimal)
- `error(exception)` - Log exception

### Algorithm Termination
- `quit(message="")` - Terminate algorithm with optional message
- `set_quit(quit_bool)` - Set quit flag

---

## Settings

### Dates
- `set_start_date(year, month, day)` - Set algorithm start date
  - **Parameters:** `year` (int), `month` (int), `day` (int)
- `set_start_date(datetime)` - Set start date from DateTime
- `set_end_date(year, month, day)` - Set algorithm end date
- `set_end_date(datetime)` - Set end date from DateTime

### Cash & Account
- `set_cash(amount)` - Set initial cash
  - **Parameters:** `amount` (decimal/double/int)
- `set_cash(symbol, amount, conversion_rate)` - Set cash in specific currency
  - **Parameters:** `symbol` (string/Symbol), `amount` (decimal), `conversion_rate` (decimal)
- `set_account_currency(currency, starting_cash=None)` - Set account currency

### Brokerage & Fees
- `set_brokerage_model(brokerage_name, account_type=AccountType.Margin)` - Set brokerage
  - **Parameters:** `brokerage_name` (BrokerageName enum)
- `set_brokerage_model(model)` - Set custom brokerage model
- `set_brokerage_message_handler(handler)` - Set brokerage message handler
- `set_risk_free_interest_rate_model(model)` - Set risk-free rate model

### Benchmark & Risk
- `set_benchmark(security_type, symbol)` - Set benchmark
  - **Parameters:** `security_type` (SecurityType), `symbol` (string)
- `set_benchmark(ticker)` - Set benchmark from ticker
- `set_benchmark(symbol)` - Set benchmark from Symbol
- `set_benchmark(func)` - Set custom benchmark function
  - **Parameters:** `func` (Func[DateTime, decimal])

### Name & Tags
- `set_name(name)` - Set algorithm name
- `add_tag(tag)` - Add a tag to the algorithm
- `set_tags(tags)` - Set tags (replaces all)
  - **Parameters:** `tags` (HashSet[string])

### Max Orders
- `set_maximum_orders(max_orders)` - Set maximum orders per minute
  - **Parameters:** `max_orders` (int)

### Parameters
- `get_parameter(name, default_value)` - Get parameter value
  - **Returns:** string/int/double/decimal (type depends on overload)
- `get_parameters()` - Get all parameters
  - **Returns:** ReadOnlyExtendedDictionary[string, string]
- `set_parameters(parameters)` - Set parameters
  - **Parameters:** `parameters` (Dictionary[string, string])

### Other Settings
- `set_default_order_properties(properties)` - Set default order properties
- `set_algorithm_id(algorithm_id)` - Set algorithm ID
- `set_algorithm_mode(mode)` - Set backtesting vs. live
  - **Parameters:** `mode` (AlgorithmMode)
- `set_deployment_target(target)` - Set deployment target
  - **Parameters:** `target` (DeploymentTarget)
- `set_security_initializer(initializer)` - Set security initializer
- `add_security_initializer(initializer)` - Add additional security initializer
- `set_history_provider(provider)` - Set history data provider
- `set_time_zone(time_zone_id)` - Set algorithm time zone
  - **Parameters:** `time_zone_id` (string, e.g., "America/New_York")

### Universe Settings
- `universe_settings` - Property: UniverseSettings for default universe configuration
  - `resolution` - Default data resolution
  - `leverage` - Default leverage
  - `fill_forward` - Default fill forward setting
  - `extended_market_hours` - Default extended hours setting
  - `data_normalization_mode` - Normalization for splits/dividends

---

## Warm-up

### Warm-up Configuration
- `set_warm_up(time_span)` - Set warm-up period with TimeSpan
  - **Parameters:** `time_span` (TimeSpan)
- `set_warm_up(time_span, resolution)` - Set warm-up with custom resolution
- `set_warm_up(bar_count)` - Set warm-up by bar count
  - **Parameters:** `bar_count` (int)
- `set_warm_up(bar_count, resolution)` - Set warm-up bars with custom resolution

### Warm-up Status
- `is_warming_up` - Property: Check if algorithm is in warm-up
  - **Returns:** bool
- `on_warmup_finished()` - Callback when warm-up completes (override in algorithm)

---

## Risk/Margin Methods

### Shortability
- `shortable(symbol)` - Check if symbol is shortable
  - **Parameters:** `symbol` (Symbol)
  - **Returns:** bool
- `shortable(symbol, short_quantity, update_order_id=None)` - Check shortability with quantity
  - **Parameters:** `symbol` (Symbol), `short_quantity` (decimal), `update_order_id` (int?)
  - **Returns:** bool
- `shortable_quantity(symbol)` - Get shortable quantity
  - **Parameters:** `symbol` (Symbol)
  - **Returns:** long

### Margin Callbacks (see Lifecycle Callbacks)
- `on_margin_call(requests)` - Respond to margin call
- `on_margin_call_warning()` - Respond to margin warning

---

## Charting/Plotting

### Basic Plotting
- `plot(series_name, value)` - Plot single value
  - **Parameters:** `series_name` (string), `value` (decimal/double/int/float)
- `plot(chart_name, series_name, value)` - Plot with chart grouping
- `plot(series_name, open, high, low, close)` - Plot OHLC bars
  - **Parameters:** All values as decimal/double/int/float
- `plot(chart_name, series_name, open, high, low, close)` - OHLC to named chart
- `plot(series_name, trade_bar)` - Plot TradeBar directly
- `plot(chart_name, series_name, trade_bar)`

### Charts
- `add_chart(chart)` - Add a chart to results
  - **Parameters:** `chart` (Chart)
- `add_series(chart_name, series_name, series_type, unit="$")` - Add series to chart
- `get_chart_updates(clear_chart_data=False)` - Get chart updates
  - **Returns:** IEnumerable[Chart]

### Indicators
- `plot_indicator(chart_name, *indicators)` - Plot indicator(s)
- `plot_indicator(chart_name, wait_for_ready, *indicators)` - Plot with ready flag

### Statistics
- `set_runtime_statistic(name, value)` - Set runtime statistic
  - **Parameters:** `name` (string), `value` (string/decimal/int/double)
- `set_summary_statistic(name, value)` - Set summary statistic
  - **Parameters:** `name` (string), `value` (string/int/double/decimal)

### Recording
- `record(series_name, value)` - Record value to chart
  - **Parameters:** `series_name` (string), `value` (int/double/decimal)

---

## Framework/Advanced Features

### Algorithm Framework Models
- `debug_mode` - Property: Enable debug mode
- `universe_selection` - Property: IUniverseSelectionModel
- `alpha` - Property: IAlphaModel
- `portfolio_construction` - Property: IPortfolioConstructionModel
- `execution` - Property: IExecutionModel
- `risk_management` - Property: IRiskManagementModel
- `insights` - Property: InsightManager

### Framework Configuration
- `set_universe_selection(model)` - Set universe selection model
- `add_universe_selection(model)` - Add additional universe selection
- `set_alpha(model)` - Set alpha model
- `add_alpha(model)` - Add additional alpha model
- `set_portfolio_construction(model)` - Set portfolio construction
- `set_execution(model)` - Set execution model
- `set_risk_management(model)` - Set risk management model
- `add_risk_management(model)` - Add additional risk management

### Insights
- `emit_insights(*insights)` - Emit insights
  - **Parameters:** `insights` (Insight objects)
- `emit_insights(insight)` - Emit single insight

### Framework Callbacks
- `on_framework_data(slice)` - Data handler for framework algorithms
- `on_framework_securities_changed(changes)` - Universe changes for framework

---

## Fundamental Data

### Access Methods
- `fundamentals(symbol)` - Get fundamental data
  - **Parameters:** `symbol` (Symbol)
  - **Returns:** Fundamental
- `fundamentals(symbols)` - Get fundamentals for multiple
  - **Parameters:** `symbols` (List[Symbol])
  - **Returns:** List[Fundamental]

### Fundamental Data Access
The `Fundamental` object provides access to:
- Company information (name, sector, industry)
- Financial statements (income statement, balance sheet, cash flow)
- Valuation metrics (P/E, P/B, dividend yield, etc.)
- Growth metrics
- Profitability metrics

---

## Additional Utilities

### Security Definition Resolution
- `symbol(ticker)` - Resolve ticker string to Symbol
  - **Parameters:** `ticker` (string)
  - **Returns:** Symbol
- `ticker(symbol)` - Get ticker string from Symbol
  - **Parameters:** `symbol` (Symbol)
  - **Returns:** string

### Identifier Mapping
- `isin(isin_code, trading_date=None)` - Resolve ISIN to Symbol
- `isin(symbol)` - Get ISIN from Symbol
  - **Returns:** string
- `composite_figi(figi_code, trading_date=None)` - Resolve Composite FIGI to Symbol
- `composite_figi(symbol)` - Get Composite FIGI
- `cusip(cusip_code, trading_date=None)` - Resolve CUSIP to Symbol
- `cusip(symbol)` - Get CUSIP
- `sedol(sedol_code, trading_date=None)` - Resolve SEDOL to Symbol
- `sedol(symbol)` - Get SEDOL
- `cik(cik_code, trading_date=None)` - Resolve CIK to Symbols
  - **Returns:** Symbol[]
- `cik(symbol)` - Get CIK
  - **Returns:** int?

### Object Store
- `object_store` - Property: ObjectStore for persistent data
- `set_object_store(store)` - Set custom object store

### Notifications
- `notify` - Property: NotificationManager for alerts
- Supports email, Slack, webhook notifications

### Downloads
- `download(address)` - Download file from URL
  - **Parameters:** `address` (string)
  - **Returns:** string (content)
- `download(address, headers)` - Download with custom headers
- `download(address, headers, username, password)` - Download with authentication

### Commands & Links
- `add_command(type)` - Register custom command type
- `broadcast_command(command)` - Broadcast command to other algorithms
- `link(command)` - Create shareable link for command
- `run_command(command)` - Run callback command

### Trading Calendar
- `trading_calendar` - Property: TradingCalendar object

### Data Consolidation
- `consolidate(symbol, period, handler)` - Consolidate bars
  - **Parameters:** `symbol` (Symbol), `period` (Resolution), `handler` (Action[TradeBar])
  - **Returns:** IDataConsolidator
- `consolidate(symbol, time_span, handler)` - Consolidate by time span
- `consolidate(symbol, calendar, handler)` - Consolidate by market calendar
- `consolidate(symbol, size, tick_type, handler)` - Consolidate by bar size
- `resolve_consolidator(symbol, resolution, data_type=None)` - Get consolidator for resolution

### Other Properties
- `name` - Property/Method: Get/set algorithm name
- `tags` - Property/Method: Get/set algorithm tags
- `status` - Property: Current AlgorithmStatus
- `algorithm_id` - Property: Algorithm ID string
- `live_mode` - Property: True if live trading
- `algorithm_mode` - Property: Backtesting or live mode
- `deployment_target` - Property: Local, cloud, etc.
- `project_id` - Property: QuantConnect project ID
- `benchmark` - Property: IBenchmark object
- `brokerage_model` - Property: IBrokerageModel
- `brokerage_name` - Property: BrokerageName enum
- `settings` - Property: IAlgorithmSettings
- `schedule` - Property: ScheduleManager
- `candlestick_patterns` - Property: CandlestickPatterns for pattern detection
- `time_rules` - Property: TimeRules for scheduling
- `date_rules` - Property: DateRules for scheduling
- `run_time_error` - Property: Set exception information
- `statistics` - Property: StatisticsResults from backtest
- `get_locked()` - Check if algorithm is locked
- `set_locked()` - Lock algorithm from modifications

### Indicator Shortcut Helpers
- `filtered_identity(symbol, selector=None, filter=None, field_name=None)` - Custom identity indicator
- `identity(symbol, selector=None, field_name=None)` - Identity indicator

---

## Important Notes

### Python Naming Convention
All C# method names use PascalCase (e.g., `SetStartDate`), but in Python they become snake_case (e.g., `set_start_date`). This conversion is automatic in the Python wrapper.

### Symbol Representation
- Most methods accept either a string ticker or a Symbol object
- The algorithm automatically converts strings to Symbols
- Symbol objects are preferred for complex multi-currency/market scenarios

### Resolution
- Data resolution can be specified as: `Resolution.Minute`, `Resolution.Hour`, `Resolution.Daily`, etc.
- Or as strings: `'minute'`, `'hour'`, `'daily'`
- Or as TimeSpan: `TimeSpan.FromMinutes(5)`

### Selector Functions
Many methods accept optional `selector` parameters (Func) to extract specific data:
```python
def my_selector(data):
    return data.close  # Extract closing price

self.sma(symbol, 20, selector=my_selector)
```

### Data Types
- `TradeBar` - OHLCV data
- `QuoteBar` - Bid/ask data
- `OptionChain` - Options data including all contracts
- `FuturesChain` - Futures data
- `Slice` - Container for all current data
- `BaseData` - Generic data point

---

## Quick Reference - Most Common Methods

```python
# Initialization
def initialize(self):
    self.set_start_date(2020, 1, 1)
    self.set_end_date(2021, 1, 1)
    self.set_cash(10000)
    self.add_equity("SPY", Resolution.DAILY)

# Main data handler
def on_data(self, slice):
    if not self.portfolio.invested:
        self.buy("SPY", 100)

# Orders
self.market_order(symbol, quantity)
self.limit_order(symbol, quantity, price)
self.set_holdings(symbol, 0.5)  # 50% portfolio

# Scheduling
self.schedule.on(self.date_rules.every_day(), 
                self.time_rules.at(9, 30), 
                self.my_function)

# Indicators
self.sma(symbol, 20)
self.rsi(symbol, 14)

# Data
history = self.history(symbol, 20, Resolution.DAILY)
```

---

This inventory is comprehensive as of LEAN 2.0 and covers the primary Python API surface used in algorithmic trading strategies.
