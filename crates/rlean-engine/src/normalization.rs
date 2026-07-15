use rlean_core::{DataNormalizationMode, DateTime, SecurityType};
use rlean_data_tables::{FactorFileEntry, QuoteBar, TradeBar};
use rust_decimal::Decimal;

/// LEAN-style price scale frontier: use `EndTime` when a bar crosses calendar days.
pub fn price_scale_frontier(time: DateTime, end_time: DateTime) -> chrono::NaiveDate {
    let time_date = time.date_utc();
    let end_date = end_time.date_utc();
    if time_date != end_date {
        let end_time_of_day = end_time.to_utc().time();
        if end_time_of_day > chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap() {
            return end_date;
        }
    }
    time_date
}

pub fn factor_for_entry(rows: &[FactorFileEntry], date: chrono::NaiveDate) -> (Decimal, Decimal) {
    if rows.is_empty() {
        return (Decimal::ONE, Decimal::ONE);
    }
    if let Some(row) = rows
        .iter()
        .filter(|row| row.date < date)
        .max_by_key(|row| row.date)
    {
        return (row.price_factor, row.split_factor);
    }
    rows.iter()
        .min_by_key(|row| row.date)
        .map(|row| (row.price_factor, row.split_factor))
        .unwrap_or((Decimal::ONE, Decimal::ONE))
}

pub fn normalize_trade_bar(
    bar: &mut TradeBar,
    normalization_mode: DataNormalizationMode,
    factor_rows: &[FactorFileEntry],
) {
    if normalization_mode == DataNormalizationMode::Raw
        || !matches!(bar.symbol.security_type(), SecurityType::Equity)
    {
        return;
    }
    let frontier = price_scale_frontier(bar.time, bar.end_time);
    apply_factor_to_trade_bar(bar, factor_rows, frontier, normalization_mode);
}

pub fn normalize_quote_bar(
    bar: &mut QuoteBar,
    normalization_mode: DataNormalizationMode,
    factor_rows: &[FactorFileEntry],
) {
    if normalization_mode == DataNormalizationMode::Raw
        || !matches!(bar.symbol.security_type(), SecurityType::Equity)
    {
        return;
    }
    let frontier = price_scale_frontier(bar.time, bar.end_time);
    let (price_factor, split_factor) = factor_for_entry(factor_rows, frontier);
    let scale = normalization_scale(normalization_mode, price_factor, split_factor);
    if scale == Decimal::ONE {
        return;
    }
    if let Some(bid) = bar.bid.as_mut() {
        bid.open *= scale;
        bid.high *= scale;
        bid.low *= scale;
        bid.close *= scale;
    }
    if let Some(ask) = bar.ask.as_mut() {
        ask.open *= scale;
        ask.high *= scale;
        ask.low *= scale;
        ask.close *= scale;
    }
}

fn normalization_scale(
    normalization_mode: DataNormalizationMode,
    price_factor: Decimal,
    split_factor: Decimal,
) -> Decimal {
    match normalization_mode {
        DataNormalizationMode::Raw => Decimal::ONE,
        DataNormalizationMode::SplitAdjusted => split_factor,
        DataNormalizationMode::Adjusted
        | DataNormalizationMode::TotalReturn
        | DataNormalizationMode::ForwardPanamaCanal
        | DataNormalizationMode::BackwardPanamaCanal => price_factor * split_factor,
    }
}

fn apply_factor_to_trade_bar(
    bar: &mut TradeBar,
    rows: &[FactorFileEntry],
    date: chrono::NaiveDate,
    normalization_mode: DataNormalizationMode,
) {
    let (price_factor, split_factor) = factor_for_entry(rows, date);
    let scale = normalization_scale(normalization_mode, price_factor, split_factor);
    if scale == Decimal::ONE {
        return;
    }
    bar.open *= scale;
    bar.high *= scale;
    bar.low *= scale;
    bar.close *= scale;
    if !split_factor.is_zero() && split_factor != Decimal::ONE {
        bar.volume /= split_factor;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use rlean_core::{Market, Symbol, TimeSpan};
    use rlean_data_tables::{TradeBar, TradeBarData};
    use rust_decimal_macros::dec;

    fn dt(date: NaiveDate, hour: u32) -> DateTime {
        DateTime::from(Utc.from_utc_datetime(&date.and_hms_opt(hour, 0, 0).unwrap()))
    }

    #[test]
    fn price_scale_frontier_uses_end_time_for_cross_midnight_daily_bar() {
        let day1 = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let time = dt(day1, 20);
        let end_time = dt(day2, 20);
        assert_eq!(price_scale_frontier(time, end_time), day2);
    }

    #[test]
    fn price_scale_frontier_uses_time_for_same_day_bar() {
        let day = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let time = dt(day, 10);
        let end_time = dt(day, 11);
        assert_eq!(price_scale_frontier(time, end_time), day);
    }

    #[test]
    fn normalize_trade_bar_applies_factor_from_frontier_date() {
        let day0 = NaiveDate::from_ymd_opt(2024, 1, 14).unwrap();
        let day1 = NaiveDate::from_ymd_opt(2024, 1, 15).unwrap();
        let day2 = NaiveDate::from_ymd_opt(2024, 1, 16).unwrap();
        let symbol = Symbol::create_equity("SPY", &Market::usa());
        let mut bar = TradeBar::new(
            symbol,
            dt(day1, 20),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(100), dec!(100), dec!(100), dec!(100), dec!(1000)),
        );
        let rows = vec![
            FactorFileEntry {
                date: day0,
                price_factor: dec!(1),
                split_factor: dec!(1),
                reference_price: dec!(0),
            },
            FactorFileEntry {
                date: day1,
                price_factor: dec!(2),
                split_factor: dec!(1),
                reference_price: dec!(0),
            },
            FactorFileEntry {
                date: day2,
                price_factor: dec!(4),
                split_factor: dec!(1),
                reference_price: dec!(0),
            },
        ];
        normalize_trade_bar(&mut bar, DataNormalizationMode::Adjusted, &rows);
        assert_eq!(bar.close, dec!(200));
    }

    /// Issue #27: with the refetched DPST factor file, a pre-split bar must be
    /// scaled up by the 1:10 reverse-split factor so it lines up with post-split
    /// prices (no phantom 10x jump). Missing factors would leave it unadjusted.
    #[test]
    fn dpst_reverse_split_scales_pre_split_bar() {
        // Reverse split effective 2023-06-05: split_factor 10 applies to all
        // dates before it (newest-first factor file, base row far in the past).
        let split_date = NaiveDate::from_ymd_opt(2023, 6, 5).unwrap();
        let pre_split = NaiveDate::from_ymd_opt(2023, 5, 30).unwrap();
        let rows = vec![
            FactorFileEntry {
                date: split_date,
                price_factor: dec!(1),
                split_factor: dec!(1),
                reference_price: dec!(0),
            },
            FactorFileEntry {
                date: NaiveDate::from_ymd_opt(1900, 1, 1).unwrap(),
                price_factor: dec!(1),
                split_factor: dec!(10),
                reference_price: dec!(0),
            },
        ];
        // A pre-split raw close of ~$5 must become ~$50 (× 10) after adjustment.
        let symbol = Symbol::create_equity("DPST", &Market::usa());
        let mut bar = TradeBar::new(
            symbol,
            dt(pre_split, 20),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(5), dec!(5), dec!(5), dec!(5), dec!(1000)),
        );
        normalize_trade_bar(&mut bar, DataNormalizationMode::Adjusted, &rows);
        assert_eq!(
            bar.close,
            dec!(50),
            "pre-split bar must scale up by the 10x reverse split"
        );

        // Empty factor rows (the bug) leave the price raw — no adjustment.
        let mut raw_bar = TradeBar::new(
            Symbol::create_equity("DPST", &Market::usa()),
            dt(pre_split, 20),
            TimeSpan::ONE_DAY,
            TradeBarData::new(dec!(5), dec!(5), dec!(5), dec!(5), dec!(1000)),
        );
        normalize_trade_bar(&mut raw_bar, DataNormalizationMode::Adjusted, &[]);
        assert_eq!(
            raw_bar.close,
            dec!(5),
            "no factor rows leaves the price unadjusted"
        );
    }
}
