"""
Lean Research kernel startup.
Pre-loads AlgorithmImports and initializes QuantBook so the user
never has to re-run imports.
"""
import sys
import os

print("Lean Research kernel starting...")

try:
    from AlgorithmImports import QuantBook, Resolution

    # Create a global QuantBook instance
    qb = QuantBook()

    print("✓ AlgorithmImports loaded")
    print("✓ QuantBook initialized as 'qb'")
    print()
    print("Available: qb, QuantBook, Resolution")
    print("Use qb.history('SPY', 252) to load data.")

except ImportError as e:
    print(f"Warning: could not import AlgorithmImports: {e}")
    print("Running in demo mode without live data.")

    # Provide a stub QuantBook for testing without the compiled module
    class QuantBook:
        def set_start_date(self, y, m, d): pass
        def set_end_date(self, y, m, d): pass
        def add_equity(self, ticker):
            class Sec: symbol = ticker
            return Sec()
        def history(self, symbol, bars, resolution=None):
            return {"time": [], "open": [], "high": [], "low": [], "close": [], "volume": []}
        def indicator(self, name, symbol, period, bars, resolution=None):
            return {"time": [], "value": []}
        def option_chain(self, ticker):
            return []

    qb = QuantBook()
    Resolution = None
    print("Stub QuantBook available as 'qb'")
