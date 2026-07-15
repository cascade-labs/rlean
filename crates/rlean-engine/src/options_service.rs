use rlean_core::Symbol;
use rlean_data_tables::TradeBar;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

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
