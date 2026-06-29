use anyhow::Result;
use lean_core::{DateTime, Market, OptionRight, OptionStyle, Resolution, Symbol, SymbolOptionsExt};
use lean_data::{OptionChainFilterMetadata, TradeBar};
use lean_options::{
    price_batch, pricing_input_from_contract, OptionChain, OptionContract, OptionContractData,
};
use lean_storage::OptionEodBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

pub fn build_daily_eod_chain(
    canonical: &Symbol,
    resolution: Resolution,
    filter: OptionChainFilterMetadata,
    date: chrono::NaiveDate,
    rows: Vec<OptionEodBar>,
    underlying_price: Decimal,
    valuation_time: DateTime,
) -> Result<Option<OptionChain>> {
    if resolution != Resolution::Daily || rows.is_empty() {
        return Ok(None);
    }

    let mut rows = filter_option_rows(rows, date, underlying_price, filter);
    if rows.is_empty() {
        return Ok(None);
    }

    rows.sort_by(|a, b| {
        a.expiration
            .cmp(&b.expiration)
            .then(a.strike.cmp(&b.strike))
            .then(a.right.cmp(&b.right))
    });

    let ticker = option_underlying_ticker(canonical);
    let mut chain = OptionChain::new(canonical.clone(), underlying_price);
    let underlying = canonical
        .underlying
        .as_ref()
        .map(|symbol| (**symbol).clone())
        .unwrap_or_else(|| Symbol::create_equity(&ticker, &Market::usa()));
    let mut contracts = Vec::new();
    for row in rows {
        let Some(right) = option_right_from_row(&row.right) else {
            continue;
        };
        let symbol = Symbol::create_option_osi(
            underlying.clone(),
            row.strike,
            row.expiration,
            right,
            OptionStyle::American,
            &Market::usa(),
        );
        let mut contract = OptionContract::new(symbol);
        contract.data = option_contract_data_from_eod(&row, underlying_price);
        contracts.push(contract);
    }

    let inputs: Vec<_> = contracts
        .iter()
        .map(|contract| pricing_input_from_contract(contract, valuation_time, 0.04, 0.0))
        .collect();
    for (mut contract, pricing) in contracts.into_iter().zip(price_batch(&inputs)) {
        contract.data.theoretical_price = pricing.theoretical_price;
        contract.data.implied_volatility = pricing.implied_volatility;
        contract.data.greeks = pricing.greeks;
        chain.add_contract(contract);
    }

    if chain.contracts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(chain))
    }
}

pub fn option_underlying_ticker(canonical: &Symbol) -> String {
    canonical
        .underlying
        .as_ref()
        .map(|underlying| underlying.permtick.to_string())
        .unwrap_or_else(|| canonical.permtick.trim_start_matches('?').to_string())
        .to_ascii_uppercase()
}

pub fn underlying_price_from_bars(canonical: &Symbol, bars: &[TradeBar]) -> Decimal {
    let Some(underlying) = canonical.underlying.as_ref() else {
        return dec!(0);
    };
    bars.iter()
        .rev()
        .find(|bar| bar.symbol.id.sid == underlying.id.sid)
        .map(|bar| bar.close)
        .unwrap_or(dec!(0))
}

fn filter_option_rows(
    rows: Vec<OptionEodBar>,
    date: chrono::NaiveDate,
    underlying_price: Decimal,
    filter: OptionChainFilterMetadata,
) -> Vec<OptionEodBar> {
    let mut rows: Vec<OptionEodBar> = rows
        .into_iter()
        .filter(|row| {
            let dte = (row.expiration - date).num_days() as i32;
            dte >= filter.min_expiry_days && dte <= filter.max_expiry_days
        })
        .collect();
    rows.sort_by(|a, b| a.strike.cmp(&b.strike));

    let atm_index = rows
        .iter()
        .enumerate()
        .min_by_key(|(_, row)| (row.strike - underlying_price).abs())
        .map(|(idx, _)| idx as i32)
        .unwrap_or(0);

    rows.into_iter()
        .enumerate()
        .filter_map(|(idx, row)| {
            let rank = idx as i32 - atm_index;
            (rank >= filter.min_strike_rank && rank <= filter.max_strike_rank).then_some(row)
        })
        .collect()
}

fn option_right_from_row(value: &str) -> Option<OptionRight> {
    match value.to_ascii_uppercase().as_str() {
        "C" | "CALL" => Some(OptionRight::Call),
        "P" | "PUT" => Some(OptionRight::Put),
        _ => None,
    }
}

fn option_contract_data_from_eod(
    row: &OptionEodBar,
    underlying_price: Decimal,
) -> OptionContractData {
    OptionContractData {
        last_price: row.close,
        volume: row.volume,
        bid_price: row.bid,
        bid_size: row.bid_size,
        ask_price: row.ask,
        ask_size: row.ask_size,
        underlying_last_price: underlying_price,
        ..OptionContractData::default()
    }
}
