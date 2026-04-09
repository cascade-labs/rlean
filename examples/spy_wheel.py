"""
SPY Monthly Options Wheel Strategy
====================================
Classic wheel using the lean_rust options framework:
  1. Sell a cash-secured put (nearest 3rd-Friday expiry, lowest available delta)
     when flat — framework credits premium to cash automatically via sell_to_open
  2. Framework handles assignment/expiry via process_option_expirations:
       - Put ITM at expiry → assigned: buy 100 shares at strike (framework does this)
       - Put OTM at expiry → expires worthless (framework removes position)
  3. Once long stock, sell a covered call at the nearest 3rd-Friday expiry
  4. Framework handles call assignment/expiry the same way

on_assignment_order_event fires whenever the framework assigns or exercises.
The strategy only decides WHICH contract to sell — the framework executes lifecycle.
"""
from AlgorithmImports import *
from datetime import datetime


class SpyWheel(QCAlgorithm):
    TARGET_DTE_MIN = 20   # minimum days to expiration when selecting
    TARGET_DTE_MAX = 45   # maximum days to expiration when selecting

    def initialize(self):
        self.set_start_date(2021, 4, 9)
        self.set_end_date(2024, 1, 1)
        self.set_cash(60_000)

        self.spy    = self.add_equity("SPY", Resolution.Daily).symbol
        self.canon  = self.add_option("SPY")          # returns "?SPY"

        # state: "flat" | "short_put" | "long_stock" | "short_call"
        self._state          = "flat"
        self._active_contract = None   # PyOptionContract we're short

        self._total_premium  = 0.0
        self._trades         = 0
        self._assignments    = 0

        self.log(f"Wheel initialized. Option canonical: {self.canon}")

    # ─── Helpers ──────────────────────────────────────────────────────────────

    def _bar_date(self, bar):
        return datetime.fromisoformat(bar.end_time).date()

    def _select_put(self, chain, today):
        """Pick the nearest-expiry OTM put within TARGET_DTE window."""
        spot = chain.underlying_price
        candidates = [
            c for c in chain.puts()
            if self.TARGET_DTE_MIN
               <= (datetime.fromisoformat(c.expiry).date() - today).days
               <= self.TARGET_DTE_MAX
            and c.strike < spot                # OTM put
        ]
        if not candidates:
            return None
        # Nearest expiry, then highest strike (closest to ATM = highest premium)
        return sorted(candidates, key=lambda c: (c.expiry, -c.strike))[0]

    def _select_call(self, chain, today):
        """Pick the nearest-expiry OTM call within TARGET_DTE window."""
        spot = chain.underlying_price
        candidates = [
            c for c in chain.calls()
            if self.TARGET_DTE_MIN
               <= (datetime.fromisoformat(c.expiry).date() - today).days
               <= self.TARGET_DTE_MAX
            and c.strike > spot                # OTM call
        ]
        if not candidates:
            return None
        # Nearest expiry, then lowest strike (closest to ATM = highest premium)
        return sorted(candidates, key=lambda c: (c.expiry, c.strike))[0]

    def _has_open_option(self):
        """True if framework still tracks an open option position."""
        return len(self.get_option_positions()) > 0

    def _is_long_stock(self):
        """True if we hold SPY shares."""
        return self.portfolio.is_invested

    # ─── Main loop ────────────────────────────────────────────────────────────

    def on_data(self, data):
        bar = data.get(self.spy)
        if bar is None:
            return

        today = self._bar_date(bar)
        chain = self.get_option_chain(self.canon)
        if chain is None:
            return

        spot = bar.close

        # ── flat: sell a cash-secured put ─────────────────────────────────
        if self._state == "flat" and not self._has_open_option():
            contract = self._select_put(chain, today)
            if contract is None:
                self.log(f"{today} No suitable put found (spot={spot:.2f})")
                return

            premium = contract.mid_price
            self.sell_to_open(contract, 1.0, premium)
            self._active_contract = contract
            self._total_premium  += premium * 100
            self._trades         += 1
            self._state           = "short_put"
            self.log(
                f"{today} SELL PUT  K={contract.strike:.0f} "
                f"exp={contract.expiry} S={spot:.2f} "
                f"prem={premium:.2f} (+${premium*100:.2f})"
            )

        # ── long_stock: sell a covered call ───────────────────────────────
        elif self._state == "long_stock" and not self._has_open_option():
            contract = self._select_call(chain, today)
            if contract is None:
                self.log(f"{today} No suitable call found (spot={spot:.2f})")
                return

            premium = contract.mid_price
            self.sell_to_open(contract, 1.0, premium)
            self._active_contract = contract
            self._total_premium  += premium * 100
            self._trades         += 1
            self._state           = "short_call"
            self.log(
                f"{today} SELL CALL K={contract.strike:.0f} "
                f"exp={contract.expiry} S={spot:.2f} "
                f"prem={premium:.2f} (+${premium*100:.2f})"
            )

    # ─── Framework assignment/expiry callback ─────────────────────────────────

    def on_assignment_order_event(self, contract, quantity, is_assignment):
        """
        Called by the framework when an option position is resolved at expiry.
        is_assignment=True  → ITM at expiry, shares delivered/taken
        is_assignment=False → expired worthless (framework also calls this)
        """
        spot = self.portfolio_value  # approximate; log only
        right = contract.right

        if is_assignment:
            self._assignments += 1
            if right.is_put():
                # Put assigned: framework bought 100 SPY at strike for us
                self._state = "long_stock"
                self.log(
                    f"PUT ASSIGNED K={contract.strike:.0f} "
                    f"→ now long 100 SPY. Total premium: ${self._total_premium:,.2f}"
                )
            else:
                # Call assigned: framework sold 100 SPY at strike for us
                self._state = "flat"
                self.log(
                    f"CALL ASSIGNED K={contract.strike:.0f} "
                    f"→ flat. Total premium: ${self._total_premium:,.2f}"
                )
        else:
            # Expired worthless
            if right.is_put():
                self._state = "flat"
                self.log(f"PUT EXPIRED worthless K={contract.strike:.0f} → flat")
            else:
                self._state = "long_stock"
                self.log(f"CALL EXPIRED worthless K={contract.strike:.0f} → remain long")

        self._active_contract = None

    # ─── End of backtest summary ──────────────────────────────────────────────

    def on_end_of_algorithm(self):
        self.log("─" * 55)
        self.log(f"Final portfolio value  : ${self.portfolio_value:,.2f}")
        self.log(f"Cash remaining         : ${self.cash:,.2f}")
        self.log(f"Total premium collected: ${self._total_premium:,.2f}")
        self.log(f"Option cycles opened   : {self._trades}")
        self.log(f"Assignments            : {self._assignments}")
        self.log(f"Final state            : {self._state}")
