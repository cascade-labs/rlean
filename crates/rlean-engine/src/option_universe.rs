use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use chrono::NaiveDate;
use rlean_core::{
    Greeks, Market, OptionRight, OptionStyle, SecurityType, Symbol, SymbolOptionsExt,
};
use rlean_data::{OptionChainSubscriptionMetadata, SubscriptionDataConfig};
use rlean_data_tables::OptionUniverseRow;
use rlean_options::{OptionChain, OptionContract};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// Convert canonical option-universe rows into one filtered LEAN chain per
/// trading date. The sidecar may already push down the filter, but applying it
/// again here keeps provider behavior irrelevant to the engine contract.
pub(crate) fn option_chains_from_rows(
    config: &SubscriptionDataConfig,
    rows: Vec<OptionUniverseRow>,
) -> anyhow::Result<Vec<(NaiveDate, OptionChain)>> {
    let metadata = config
        .option_chain
        .as_ref()
        .context("option-universe subscription is missing chain metadata")?;
    let mut by_date = BTreeMap::<NaiveDate, Vec<OptionUniverseRow>>::new();
    for row in rows {
        by_date.entry(row.date).or_default().push(row);
    }
    by_date
        .into_iter()
        .map(|(date, rows)| chain_for_date(config, metadata, date, rows))
        .collect()
}

fn chain_for_date(
    config: &SubscriptionDataConfig,
    metadata: &OptionChainSubscriptionMetadata,
    date: NaiveDate,
    rows: Vec<OptionUniverseRow>,
) -> anyhow::Result<(NaiveDate, OptionChain)> {
    // LEAN's BaseChainUniverseData.Time is the source-file date and EndTime is
    // the following midnight, when selection actually runs. Expiry filters
    // are relative to that selection date, not the stored row date.
    let selection_date = date.succ_opt().unwrap_or(date);
    let underlying = config
        .symbol
        .underlying
        .as_deref()
        .cloned()
        .unwrap_or_else(|| Symbol::create_equity(&metadata.underlying_ticker, &Market::usa()));
    let underlying_price = rows
        .iter()
        .find(|row| row.expiration.is_none())
        .map(|row| row.close)
        .unwrap_or_default();
    let mut contracts = rows
        .into_iter()
        .filter_map(|row| contract_from_row(&underlying, underlying_price, date, row))
        .collect::<Vec<_>>();
    contracts.retain(|contract| {
        let days = (contract.expiry - selection_date).num_days();
        days >= i64::from(metadata.filter.min_expiry_days)
            && days <= i64::from(metadata.filter.max_expiry_days)
    });

    let unique_strikes = contracts
        .iter()
        .map(|contract| contract.strike)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if !unique_strikes.is_empty() {
        let atm = unique_strikes.partition_point(|strike| *strike < underlying_price);
        let atm = atm.min(unique_strikes.len().saturating_sub(1));
        let min_index = (atm as i32 + metadata.filter.min_strike_rank)
            .max(0)
            .min(unique_strikes.len().saturating_sub(1) as i32) as usize;
        let max_index = (atm as i32 + metadata.filter.max_strike_rank)
            .max(0)
            .min(unique_strikes.len().saturating_sub(1) as i32) as usize;
        if min_index <= max_index {
            let min_strike = unique_strikes[min_index];
            let max_strike = unique_strikes[max_index];
            contracts
                .retain(|contract| contract.strike >= min_strike && contract.strike <= max_strike);
        } else {
            contracts.clear();
        }
    }

    let mut chain = OptionChain::new(config.symbol.clone(), underlying_price);
    for contract in contracts {
        chain.add_contract(contract);
    }
    Ok((date, chain))
}

fn contract_from_row(
    underlying: &Symbol,
    underlying_price: Decimal,
    date: NaiveDate,
    row: OptionUniverseRow,
) -> Option<OptionContract> {
    let expiry = row.expiration?;
    let strike = row.strike?;
    let days = (expiry - date).num_days();
    if days < 0 {
        return None;
    }
    let right = match row.right?.trim().to_ascii_lowercase().as_str() {
        "call" | "c" => OptionRight::Call,
        "put" | "p" => OptionRight::Put,
        _ => return None,
    };
    let (style, symbol) = if underlying.security_type() == SecurityType::Index {
        (
            OptionStyle::European,
            Symbol::create_index_option_osi(
                underlying.clone(),
                strike,
                expiry,
                right,
                OptionStyle::European,
                &Market::new(&row.market),
            ),
        )
    } else {
        (
            OptionStyle::American,
            Symbol::create_option_osi(
                underlying.clone(),
                strike,
                expiry,
                right,
                OptionStyle::American,
                &Market::new(&row.market),
            ),
        )
    };
    let mut contract = OptionContract::new(symbol);
    contract.style = style;
    contract.data.theoretical_price = row.close;
    contract.data.last_price = row.close;
    contract.data.volume = row.volume.to_i64().unwrap_or_default();
    contract.data.open_interest = row.open_interest.unwrap_or_default();
    contract.data.implied_volatility = row.implied_volatility.unwrap_or_default();
    contract.data.greeks = Greeks {
        delta: row.delta.unwrap_or_default(),
        gamma: row.gamma.unwrap_or_default(),
        vega: row.vega.unwrap_or_default(),
        theta: row.theta.unwrap_or_default(),
        rho: row.rho.unwrap_or_default(),
        lambda: Decimal::ZERO,
    };
    contract.data.underlying_last_price = underlying_price;
    Some(contract)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rlean_core::Resolution;
    use rlean_data::{OptionChainFilterMetadata, OptionChainSubscriptionMetadata};

    fn row(
        date: NaiveDate,
        expiration: Option<NaiveDate>,
        strike: Option<Decimal>,
    ) -> OptionUniverseRow {
        OptionUniverseRow {
            date,
            market: "usa".to_string(),
            security_type: if expiration.is_some() {
                "option".to_string()
            } else {
                "equity".to_string()
            },
            symbol_sid: String::new(),
            symbol_value: "SPY".to_string(),
            underlying_sid: expiration.map(|_| String::new()),
            underlying_value: expiration.map(|_| "SPY".to_string()),
            expiration,
            strike,
            right: expiration.map(|_| "Call".to_string()),
            open: Decimal::new(500, 0),
            high: Decimal::new(500, 0),
            low: Decimal::new(500, 0),
            close: Decimal::new(500, 0),
            volume: Decimal::ZERO,
            open_interest: None,
            implied_volatility: None,
            delta: None,
            gamma: None,
            vega: None,
            theta: None,
            rho: None,
        }
    }

    #[test]
    fn zero_dte_filter_uses_lean_universe_end_time() {
        let source_date = NaiveDate::from_ymd_opt(2026, 7, 13).unwrap();
        let selection_date = source_date.succ_opt().unwrap();
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let canonical = Symbol::create_option(
            underlying,
            &Market::usa(),
            NaiveDate::MIN,
            Decimal::ZERO,
            OptionRight::Call,
            OptionStyle::American,
        );
        let config = SubscriptionDataConfig::new_option_chain(
            canonical,
            Resolution::Minute,
            OptionChainSubscriptionMetadata {
                canonical_permtick: "?SPY".to_string(),
                underlying_ticker: "SPY".to_string(),
                filter: OptionChainFilterMetadata {
                    min_strike_rank: -5,
                    max_strike_rank: 5,
                    min_expiry_days: 0,
                    max_expiry_days: 0,
                },
            },
        );

        let chains = option_chains_from_rows(
            &config,
            vec![
                row(source_date, None, None),
                row(
                    source_date,
                    Some(selection_date),
                    Some(Decimal::new(500, 0)),
                ),
            ],
        )
        .unwrap();

        assert_eq!(chains.len(), 1);
        assert_eq!(chains[0].1.contracts.len(), 1);
    }
}
