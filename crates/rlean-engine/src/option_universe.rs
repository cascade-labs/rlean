use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use chrono::NaiveDate;
use rlean_core::{
    Greeks, Market, MarketHoursDatabase, OptionRight, OptionStyle, SecurityType, Symbol,
    SymbolOptionsExt,
};
use rlean_data::{OptionChainSubscriptionMetadata, SubscriptionDataConfig};
use rlean_data_tables::OptionUniverseRow;
use rlean_options::{OptionChain, OptionContract};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

/// Convert canonical option-universe rows into one filtered LEAN chain per
/// trading date. The provider may already push down the filter, but applying it
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
    let underlying = config
        .symbol
        .underlying
        .as_deref()
        .cloned()
        .unwrap_or_else(|| Symbol::create_equity(&metadata.underlying_ticker, &Market::usa()));
    // LEAN's BaseChainUniverseData.Time is the source-file date and EndTime is
    // the following midnight, when selection actually runs. If that midnight
    // falls on a closed date, OptionFilterUniverse advances the expiration
    // reference to the next trading day (for example Friday data selects
    // Monday contracts at Saturday midnight).
    let mut selection_date = date.succ_opt().unwrap_or(date);
    let exchange_hours = MarketHoursDatabase::global().exchange_hours(&underlying);
    while exchange_hours.session_bounds(selection_date).is_none() {
        let Some(next) = selection_date.succ_opt() else {
            break;
        };
        selection_date = next;
    }
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
    if let Some((min_strike, max_strike)) = lean_relative_strike_bounds(
        &unique_strikes,
        underlying_price,
        metadata.filter.min_strike_rank,
        metadata.filter.max_strike_rank,
    ) {
        contracts.retain(|contract| contract.strike >= min_strike && contract.strike <= max_strike);
    } else {
        contracts.clear();
    }

    let mut chain = OptionChain::new(config.symbol.clone(), underlying_price);
    for contract in contracts {
        chain.add_contract(contract);
    }
    Ok((date, chain))
}

/// Exact port of C# LEAN `OptionFilterUniverse.Strikes`.
fn lean_relative_strike_bounds(
    unique_strikes: &[Decimal],
    underlying_price: Decimal,
    min_strike_rank: i32,
    max_strike_rank: i32,
) -> Option<(Decimal, Decimal)> {
    let (index, exact_price_found) = match unique_strikes.binary_search(&underlying_price) {
        Ok(index) => (index as i32, true),
        Err(index) if index == unique_strikes.len() => return None,
        Err(index) => (index as i32, false),
    };

    let mut min_index = index + min_strike_rank;
    let mut max_index = index + max_strike_rank;
    if !exact_price_found {
        if min_strike_rank < 0 && max_strike_rank > 0 {
            max_index -= 1;
        } else if min_strike_rank > 0 {
            min_index -= 1;
            max_index -= 1;
        }
    }

    if min_index < 0 {
        min_index = 0;
    } else if min_index >= unique_strikes.len() as i32 {
        return None;
    }
    if max_index < 0 {
        return None;
    }
    if max_index >= unique_strikes.len() as i32 {
        max_index = unique_strikes.len() as i32 - 1;
    }
    (min_index <= max_index).then(|| {
        (
            unique_strikes[min_index as usize],
            unique_strikes[max_index as usize],
        )
    })
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

    #[test]
    fn relative_strikes_match_lean_when_underlying_is_between_strikes() {
        let strikes = [
            Decimal::new(739, 0),
            Decimal::new(740, 0),
            Decimal::new(741, 0),
            Decimal::new(742, 0),
        ];

        assert_eq!(
            lean_relative_strike_bounds(&strikes, Decimal::new(7403, 1), -1, 1),
            Some((Decimal::new(740, 0), Decimal::new(741, 0)))
        );
    }

    #[test]
    fn friday_universe_uses_monday_as_expiration_reference() {
        let source_date = NaiveDate::from_ymd_opt(2026, 7, 17).unwrap();
        let monday = NaiveDate::from_ymd_opt(2026, 7, 20).unwrap();
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
                row(source_date, Some(monday), Some(Decimal::new(500, 0))),
            ],
        )
        .unwrap();

        assert_eq!(chains[0].1.contracts.len(), 1);
    }
}
