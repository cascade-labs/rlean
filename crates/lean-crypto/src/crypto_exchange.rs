use chrono::Timelike;

/// Crypto exchanges trade 24/7/365 (no market hours).
#[derive(Debug, Clone)]
pub struct CryptoExchange {
    pub name: String,
    pub timezone: String,
    /// Maintenance windows (UTC hour ranges when exchange may be down)
    pub maintenance_windows: Vec<(u32, u32)>, // (start_hour, end_hour)
}

impl CryptoExchange {
    pub fn binance() -> Self {
        Self {
            name: "Binance".to_string(),
            timezone: "UTC".to_string(),
            maintenance_windows: vec![],
        }
    }

    pub fn coinbase() -> Self {
        Self {
            name: "Coinbase".to_string(),
            timezone: "UTC".to_string(),
            maintenance_windows: vec![],
        }
    }

    pub fn bybit() -> Self {
        Self {
            name: "Bybit".to_string(),
            timezone: "UTC".to_string(),
            maintenance_windows: vec![],
        }
    }

    pub fn kraken() -> Self {
        Self {
            name: "Kraken".to_string(),
            timezone: "UTC".to_string(),
            maintenance_windows: vec![],
        }
    }

    /// Crypto markets are always open (24/7)
    pub fn is_open(&self, _utc_now: chrono::DateTime<chrono::Utc>) -> bool {
        true
    }

    /// Returns true if there's a scheduled maintenance window
    pub fn is_maintenance(&self, utc_now: chrono::DateTime<chrono::Utc>) -> bool {
        let hour = utc_now.hour();
        self.maintenance_windows
            .iter()
            .any(|(start, end)| hour >= *start && hour < *end)
    }
}
