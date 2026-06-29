//! SDK-owned market-data subscription helpers.

use lean_core::{Market, SecurityType};
use lean_data::CustomDataQuery;
use std::collections::HashMap;

pub fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToString::to_string)
        .collect()
}

pub fn custom_query_from_properties(properties: &HashMap<String, String>) -> CustomDataQuery {
    let mut query = CustomDataQuery::default();
    if let Some(symbols) = properties.get("symbols") {
        query.symbols = Some(split_csv(symbols));
    }
    if let Some(columns) = properties.get("columns") {
        query.columns = Some(split_csv(columns));
    }
    for (key, value) in properties {
        if let Some(column) = key.strip_prefix("eq_") {
            query
                .string_equals
                .insert(column.to_string(), value.to_string());
        } else if let Some(column) = key.strip_prefix("in_") {
            query.string_in.insert(column.to_string(), split_csv(value));
        } else if let Some(column) = key.strip_prefix("min_") {
            if let Ok(v) = value.parse::<f64>() {
                query.numeric_min.insert(column.to_string(), v);
            }
        } else if let Some(column) = key.strip_prefix("max_") {
            if let Ok(v) = value.parse::<f64>() {
                query.numeric_max.insert(column.to_string(), v);
            }
        }
    }
    query.properties = properties.clone();
    query
}

pub fn custom_query(
    symbols: Option<Vec<String>>,
    columns: Option<Vec<String>>,
    properties: HashMap<String, String>,
) -> CustomDataQuery {
    let mut query = CustomDataQuery {
        symbols,
        columns,
        ..Default::default()
    };
    query = query.merge(&custom_query_from_properties(&properties));
    query.properties.extend(properties);
    query
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptoUniverseSpec {
    pub universe: String,
    pub market: Market,
    pub security_type: SecurityType,
    pub properties: HashMap<String, String>,
}

pub fn crypto_universe_spec(universe: &str, market: Option<&str>) -> CryptoUniverseSpec {
    let universe = normalize_hyperliquid_universe(universe);
    let market = Market::new(market.unwrap_or(Market::HYPERLIQUID));
    let security_type = hyperliquid_universe_security_type(&universe);
    let mut properties = HashMap::new();
    properties.insert("universe".to_string(), universe.clone());
    properties.insert("market".to_string(), market.as_str().to_string());
    properties.insert("security_type".to_string(), security_type.to_string());
    CryptoUniverseSpec {
        universe,
        market,
        security_type,
        properties,
    }
}

pub fn normalize_hyperliquid_universe(value: &str) -> String {
    let cleaned = value
        .trim()
        .replace(['-', '.', ':', ' '], "_")
        .to_ascii_uppercase();
    match cleaned.as_str() {
        "PERP" | "PERPS" | "CRYPTOFUTURE" | "CRYPTO_FUTURE" | "CRYPTO_PERPS" => {
            "CRYPTO_PERP".to_string()
        }
        "SPOT" | "CRYPTO" => "CRYPTO_SPOT".to_string(),
        "HIP3_TRADING_XYZ" => "HIP3_XYZ".to_string(),
        other => other.to_string(),
    }
}

pub fn hyperliquid_universe_security_type(universe: &str) -> SecurityType {
    if universe == "CRYPTO_SPOT" {
        SecurityType::Crypto
    } else {
        SecurityType::CryptoFuture
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_csv_trims_and_discards_empty_values() {
        assert_eq!(
            split_csv(" SPY, ,AAPL,, MSFT "),
            vec!["SPY".to_string(), "AAPL".to_string(), "MSFT".to_string()]
        );
    }

    #[test]
    fn custom_query_from_properties_maps_filter_prefixes() {
        let properties = HashMap::from([
            ("symbols".to_string(), "SPY,AAPL".to_string()),
            ("columns".to_string(), "open,close".to_string()),
            ("eq_region".to_string(), "US".to_string()),
            ("in_sector".to_string(), "tech, finance".to_string()),
            ("min_volume".to_string(), "1000".to_string()),
            ("max_price".to_string(), "bad-number".to_string()),
        ]);

        let query = custom_query_from_properties(&properties);

        assert_eq!(
            query.symbols,
            Some(vec!["SPY".to_string(), "AAPL".to_string()])
        );
        assert_eq!(
            query.columns,
            Some(vec!["open".to_string(), "close".to_string()])
        );
        assert_eq!(query.string_equals.get("region"), Some(&"US".to_string()));
        assert_eq!(
            query.string_in.get("sector"),
            Some(&vec!["tech".to_string(), "finance".to_string()])
        );
        assert_eq!(query.numeric_min.get("volume"), Some(&1000.0));
        assert!(!query.numeric_max.contains_key("price"));
        assert_eq!(query.properties, properties);
    }

    #[test]
    fn crypto_universe_spec_normalizes_hyperliquid_aliases() {
        let perp = crypto_universe_spec("crypto-perps", None);
        assert_eq!(perp.universe, "CRYPTO_PERP");
        assert_eq!(perp.market, Market::new(Market::HYPERLIQUID));
        assert_eq!(perp.security_type, SecurityType::CryptoFuture);
        assert_eq!(
            perp.properties.get("security_type"),
            Some(&SecurityType::CryptoFuture.to_string())
        );

        let spot = crypto_universe_spec(" spot ", Some("custom-market"));
        assert_eq!(spot.universe, "CRYPTO_SPOT");
        assert_eq!(spot.market, Market::new("custom-market"));
        assert_eq!(spot.security_type, SecurityType::Crypto);
    }
}
