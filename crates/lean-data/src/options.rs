use chrono::NaiveDate;
use lean_core::{Greeks, OptionRight, OptionStyle, Symbol, SymbolOptionsExt};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Market data and model output for a single option contract at a point in time.
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
        Self {
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

/// A single option contract in an option chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionContract {
    pub symbol: Symbol,
    pub strike: Decimal,
    pub expiry: NaiveDate,
    pub right: OptionRight,
    pub style: OptionStyle,
    pub data: OptionContractData,
    /// 100 for equity options (shares per contract).
    pub contract_unit_of_trade: i64,
    /// Contract multiplier for P&L (usually 100).
    pub contract_multiplier: i64,
}

impl OptionContract {
    pub fn new(symbol: Symbol) -> Self {
        let (strike, expiry, right, style) = symbol
            .option_symbol_id()
            .map(|id| (id.strike, id.expiry, id.right, id.style))
            .unwrap_or((
                Decimal::ZERO,
                NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(),
                OptionRight::Call,
                OptionStyle::American,
            ));
        Self {
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
        match self.right {
            OptionRight::Call => (self.data.underlying_last_price - self.strike).max(Decimal::ZERO),
            OptionRight::Put => (self.strike - self.data.underlying_last_price).max(Decimal::ZERO),
        }
    }

    pub fn time_value(&self) -> Decimal {
        (self.mid_price() - self.intrinsic_value()).max(Decimal::ZERO)
    }
}

/// Collection of option contracts for a single underlying at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionChain {
    /// The canonical option symbol (e.g. ?SPY).
    pub canonical_symbol: Symbol,
    /// Current price of the underlying.
    pub underlying_price: Decimal,
    /// All contracts keyed by their full symbol.
    pub contracts: HashMap<Symbol, OptionContract>,
}

impl OptionChain {
    pub fn new(canonical_symbol: Symbol, underlying_price: Decimal) -> Self {
        Self {
            canonical_symbol,
            underlying_price,
            contracts: HashMap::new(),
        }
    }

    pub fn add_contract(&mut self, contract: OptionContract) {
        self.contracts.insert(contract.symbol.clone(), contract);
    }

    /// Update an existing chain without replacing the whole contract map.
    pub fn update_from(&mut self, next: &OptionChain) {
        self.canonical_symbol = next.canonical_symbol.clone();
        self.underlying_price = next.underlying_price;

        self.contracts
            .retain(|symbol, _| next.contracts.contains_key(symbol));

        for (symbol, next_contract) in &next.contracts {
            if let Some(current_contract) = self.contracts.get_mut(symbol) {
                current_contract.strike = next_contract.strike;
                current_contract.expiry = next_contract.expiry;
                current_contract.right = next_contract.right;
                current_contract.style = next_contract.style;
                current_contract.data = next_contract.data.clone();
                current_contract.contract_unit_of_trade = next_contract.contract_unit_of_trade;
                current_contract.contract_multiplier = next_contract.contract_multiplier;
            } else {
                self.contracts.insert(symbol.clone(), next_contract.clone());
            }
        }
    }

    /// Returns contracts filtered by the given function.
    pub fn filter<F: Fn(&OptionContract) -> bool>(&self, f: F) -> Vec<&OptionContract> {
        self.contracts.values().filter(|contract| f(contract)).collect()
    }

    /// All contracts in a canonical, deterministic order: expiry, then strike,
    /// then right (Call before Put).
    pub fn sorted(&self) -> Vec<&OptionContract> {
        let mut contracts: Vec<&OptionContract> = self.contracts.values().collect();
        contracts.sort_by(|a, b| {
            a.expiry
                .cmp(&b.expiry)
                .then(a.strike.cmp(&b.strike))
                .then(a.right.cmp(&b.right))
        });
        contracts
    }
}
