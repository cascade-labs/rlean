use chrono::NaiveDate;
use lean_core::{Greeks, OptionRight, OptionStyle, Symbol};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Market data for a single option contract at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionContractData {
    pub theoretical_price: Decimal,
    pub implied_volatility: Decimal,
    pub greeks: Greeks,
    pub open_interest: Decimal,
    pub last_price: Decimal,
    pub volume: i64,
    pub bid_price: Decimal,
    pub bid_size: i64,
    pub ask_price: Decimal,
    pub ask_size: i64,
    pub underlying_last_price: Decimal,
}

impl Default for OptionContractData {
    fn default() -> Self {
        OptionContractData {
            theoretical_price: Decimal::ZERO,
            implied_volatility: Decimal::ZERO,
            greeks: Greeks::default(),
            open_interest: Decimal::ZERO,
            last_price: Decimal::ZERO,
            volume: 0,
            bid_price: Decimal::ZERO,
            bid_size: 0,
            ask_price: Decimal::ZERO,
            ask_size: 0,
            underlying_last_price: Decimal::ZERO,
        }
    }
}

/// A single option contract in the universe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionContract {
    pub symbol: Symbol,
    pub strike: Decimal,
    pub expiry: NaiveDate,
    pub right: OptionRight,
    pub style: OptionStyle,
    pub data: OptionContractData,
    /// 100 for equity options (shares per contract)
    pub contract_unit_of_trade: i64,
    /// Contract multiplier for P&L (usually 100)
    pub contract_multiplier: i64,
}

impl OptionContract {
    pub fn new(symbol: Symbol) -> Self {
        use lean_core::SymbolOptionsExt;
        let (strike, expiry, right, style) = symbol
            .option_symbol_id()
            .map(|id| (id.strike, id.expiry, id.right, id.style))
            .unwrap_or((
                Decimal::ZERO,
                NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                OptionRight::Call,
                OptionStyle::American,
            ));
        OptionContract {
            symbol,
            strike,
            expiry,
            right,
            style,
            data: OptionContractData::default(),
            contract_unit_of_trade: 100,
            contract_multiplier: 100,
        }
    }

    pub fn mid_price(&self) -> Decimal {
        if self.data.bid_price > Decimal::ZERO && self.data.ask_price > Decimal::ZERO {
            (self.data.bid_price + self.data.ask_price) / rust_decimal_macros::dec!(2)
        } else {
            self.data.last_price
        }
    }

    pub fn intrinsic_value(&self) -> Decimal {
        crate::payoff::intrinsic_value(self.data.underlying_last_price, self.strike, self.right)
    }

    pub fn time_value(&self) -> Decimal {
        (self.mid_price() - self.intrinsic_value()).max(Decimal::ZERO)
    }
}
