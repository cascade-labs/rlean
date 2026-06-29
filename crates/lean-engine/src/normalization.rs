use lean_core::{DataNormalizationMode, DateTime, SecurityType, Symbol};
use lean_data::{QuoteBar, TradeBar};
use lean_storage::{FactorFileEntry, IcebergStore};
use rust_decimal::prelude::FromPrimitive;
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

pub fn read_factor_rows(store: &IcebergStore, symbol: &Symbol) -> Vec<FactorFileEntry> {
    if !matches!(symbol.security_type(), SecurityType::Equity) {
        return Vec::new();
    }
    let store = store.clone();
    let market = symbol.market().as_str().to_string();
    let ticker = symbol.permtick.to_string();
    block_on_background(async move { store.scan_factor_file(&market, &ticker).await })
        .unwrap_or_default()
}

fn block_on_background<F, T>(future: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| anyhow::anyhow!(e))?
            .block_on(future)
    })
    .join()
    .map_err(|_| anyhow::anyhow!("factor reader worker panicked"))?
}

pub fn factor_for_entry(rows: &[FactorFileEntry], date: chrono::NaiveDate) -> (f64, f64) {
    if rows.is_empty() {
        return (1.0, 1.0);
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
        .unwrap_or((1.0, 1.0))
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
    if (scale - 1.0).abs() < 1e-9 {
        return;
    }
    let price_scale = Decimal::from_f64(scale).unwrap_or(Decimal::ONE);
    if let Some(bid) = bar.bid.as_mut() {
        bid.open *= price_scale;
        bid.high *= price_scale;
        bid.low *= price_scale;
        bid.close *= price_scale;
    }
    if let Some(ask) = bar.ask.as_mut() {
        ask.open *= price_scale;
        ask.high *= price_scale;
        ask.low *= price_scale;
        ask.close *= price_scale;
    }
}

fn normalization_scale(
    normalization_mode: DataNormalizationMode,
    price_factor: f64,
    split_factor: f64,
) -> f64 {
    match normalization_mode {
        DataNormalizationMode::Raw => 1.0,
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
    if (scale - 1.0).abs() < 1e-9 {
        return;
    }
    let price_scale = Decimal::from_f64(scale).unwrap_or(Decimal::ONE);
    bar.open *= price_scale;
    bar.high *= price_scale;
    bar.low *= price_scale;
    bar.close *= price_scale;
    if split_factor != 0.0 && (split_factor - 1.0).abs() > 1e-9 {
        let volume_scale = Decimal::from_f64(1.0 / split_factor).unwrap_or(Decimal::ONE);
        bar.volume *= volume_scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use lean_core::{Market, Symbol, TimeSpan};
    use lean_data::{TradeBar, TradeBarData};
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
                price_factor: 1.0,
                split_factor: 1.0,
                reference_price: 0.0,
            },
            FactorFileEntry {
                date: day1,
                price_factor: 2.0,
                split_factor: 1.0,
                reference_price: 0.0,
            },
            FactorFileEntry {
                date: day2,
                price_factor: 4.0,
                split_factor: 1.0,
                reference_price: 0.0,
            },
        ];
        normalize_trade_bar(&mut bar, DataNormalizationMode::Adjusted, &rows);
        assert_eq!(bar.close, dec!(200));
    }
}
