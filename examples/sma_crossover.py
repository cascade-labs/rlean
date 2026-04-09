"""
SMA Crossover — the canonical "hello world" algorithmic strategy.

Buy when the fast SMA (50-day) crosses above the slow SMA (200-day).
Sell when it crosses below. Fully-invested or flat.

Run with:
    cargo run --bin python_runner -- examples/sma_crossover.py --data data/
"""
from AlgorithmImports import *


class SmaCrossover(QCAlgorithm):

    def initialize(self):
        self.set_start_date(2020, 1, 1)
        self.set_end_date(2024, 1, 1)
        self.set_cash(100_000)

        self.spy = self.add_equity("SPY", Resolution.Daily).symbol

        self.fast = SimpleMovingAverage(50)
        self.slow = SimpleMovingAverage(200)

        self.log("SMA Crossover initialised.")

    def on_data(self, data):
        bar = data.bars.get(self.spy)
        if bar is None:
            return

        self.fast.update(self.time, bar.close)
        self.slow.update(self.time, bar.close)

        if not self.fast.is_ready or not self.slow.is_ready:
            return

        invested = self.portfolio[self.spy].invested

        if self.fast.current.value > self.slow.current.value and not invested:
            self.set_holdings(self.spy, 1.0)
            self.log(f"BUY  SPY @ {bar.close:.2f}  fast={self.fast.current.value:.2f}  slow={self.slow.current.value:.2f}")

        elif self.fast.current.value < self.slow.current.value and invested:
            self.liquidate(self.spy)
            self.log(f"SELL SPY @ {bar.close:.2f}  fast={self.fast.current.value:.2f}  slow={self.slow.current.value:.2f}")

    def on_order_event(self, event):
        if event.is_fill:
            print(f"  Fill: {event.symbol} qty={event.fill_quantity:.0f} @ {event.fill_price:.2f}")

    def on_end_of_algorithm(self):
        print(f"Final portfolio value : ${self.portfolio.total_portfolio_value:,.2f}")
        print(f"Cash remaining        : ${self.cash:,.2f}")
