use crate::time::{NanosecondTimestamp, TimeSpan};
use crate::{SecurityType, Symbol, SymbolOptionsExt};
use chrono::{Datelike, NaiveDate, TimeZone, Timelike};
use chrono_tz::Tz;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// A single session: open and close times as offsets from midnight (nanos).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketSession {
    pub open: TimeSpan,
    pub close: TimeSpan,
}

impl MarketSession {
    pub fn new(open_hour: u8, open_min: u8, close_hour: u8, close_min: u8) -> Self {
        MarketSession {
            open: TimeSpan::from_secs(open_hour as i64 * 3600 + open_min as i64 * 60),
            close: TimeSpan::from_secs(close_hour as i64 * 3600 + close_min as i64 * 60),
        }
    }
}

/// Per-weekday sessions. A day with no sessions is a market holiday.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaySchedule {
    pub sessions: Vec<MarketSession>,
}

impl DaySchedule {
    pub fn open(session: MarketSession) -> Self {
        DaySchedule {
            sessions: vec![session],
        }
    }
    pub fn closed() -> Self {
        DaySchedule { sessions: vec![] }
    }
    pub fn is_open(&self) -> bool {
        !self.sessions.is_empty()
    }
}

/// Full exchange hours definition. Mirrors LEAN's `SecurityExchangeHours`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExchangeHours {
    pub timezone: String,
    pub schedule: [DaySchedule; 7], // 0 = Sunday
    pub holidays: HashSet<NaiveDate>,
    pub early_closes: std::collections::HashMap<NaiveDate, TimeSpan>,
    pub late_opens: std::collections::HashMap<NaiveDate, TimeSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarketHoursKey {
    pub market: String,
    pub symbol: String,
    pub security_type: SecurityType,
}

impl MarketHoursKey {
    pub fn new(
        market: impl Into<String>,
        symbol: impl Into<String>,
        security_type: SecurityType,
    ) -> Self {
        Self {
            market: market.into(),
            symbol: symbol.into(),
            security_type,
        }
    }

    pub fn from_symbol(symbol: &Symbol) -> Self {
        Self::new(
            symbol.market().as_str().to_ascii_lowercase(),
            Self::database_symbol(symbol),
            symbol.security_type(),
        )
    }

    fn database_symbol(symbol: &Symbol) -> String {
        if symbol.security_type().is_option_like() {
            symbol
                .underlying
                .as_ref()
                .map(|underlying| underlying.permtick.to_ascii_uppercase())
                .unwrap_or_else(|| symbol.permtick.trim_start_matches('?').to_ascii_uppercase())
        } else {
            symbol.permtick.to_ascii_uppercase()
        }
    }
}

#[derive(Debug, Clone)]
pub struct MarketHoursDatabase {
    entries: HashMap<MarketHoursKey, Arc<ExchangeHours>>,
    defaults: HashMap<SecurityType, Arc<ExchangeHours>>,
}

static GLOBAL_MARKET_HOURS_DATABASE: Lazy<Arc<MarketHoursDatabase>> =
    Lazy::new(|| Arc::new(MarketHoursDatabase::from_builtin_defaults()));

impl MarketHoursDatabase {
    pub fn global() -> Arc<Self> {
        GLOBAL_MARKET_HOURS_DATABASE.clone()
    }

    pub fn from_builtin_defaults() -> Self {
        let us_equity = Arc::new(ExchangeHours::us_equity());
        let forex = Arc::new(ExchangeHours::forex_24h());
        let crypto = Arc::new(ExchangeHours::crypto_24_7());

        let mut defaults = HashMap::new();
        defaults.insert(SecurityType::Equity, us_equity.clone());
        defaults.insert(SecurityType::Option, us_equity.clone());
        defaults.insert(SecurityType::IndexOption, us_equity.clone());
        defaults.insert(SecurityType::Forex, forex);
        defaults.insert(SecurityType::Crypto, crypto.clone());
        defaults.insert(SecurityType::CryptoFuture, crypto.clone());
        defaults.insert(SecurityType::Base, crypto);

        Self {
            entries: HashMap::new(),
            defaults,
        }
    }

    pub fn with_entry(mut self, key: MarketHoursKey, exchange_hours: Arc<ExchangeHours>) -> Self {
        self.entries.insert(key, exchange_hours);
        self
    }

    pub fn exchange_hours(&self, symbol: &Symbol) -> Arc<ExchangeHours> {
        let key = MarketHoursKey::from_symbol(symbol);
        self.entries
            .get(&key)
            .or_else(|| self.defaults.get(&key.security_type))
            .cloned()
            .unwrap_or_else(|| Arc::new(ExchangeHours::crypto_24_7()))
    }

    pub fn is_open_date(&self, symbol: &Symbol, date: NaiveDate) -> bool {
        date.and_hms_opt(12, 0, 0)
            .map(|midday| self.exchange_hours(symbol).is_open_at_local_naive(midday))
            .unwrap_or(false)
    }

    /// Returns the exchange-local expiration frontier for an option contract.
    ///
    /// This is the rlean equivalent of C# LEAN
    /// `OptionSymbol.TryGetExpirationDateTime`: contracts expire at the last
    /// regular market close on their expiration trading day, not at midnight.
    /// If the encoded expiration date is closed, the previous trading day's
    /// close is used.
    pub fn option_expiration_time(&self, symbol: &Symbol) -> Option<NanosecondTimestamp> {
        let option = symbol.option_symbol_id()?;
        let hours = self.exchange_hours(symbol);
        let mut trading_day = option.expiry;

        for _ in 0..14 {
            if let Some((_, close)) = hours.session_bounds(trading_day) {
                return Some(close);
            }
            trading_day = trading_day.pred_opt()?;
        }
        None
    }

    /// C# LEAN `OptionSymbol.IsOptionContractExpired` parity.
    pub fn is_option_contract_expired(
        &self,
        symbol: &Symbol,
        current_time_utc: NanosecondTimestamp,
    ) -> bool {
        self.option_expiration_time(symbol)
            .is_some_and(|expiration| current_time_utc >= expiration)
    }

    /// Walk back `bar_count` regular trading days from `end_date` (exclusive)
    /// using the symbol's exchange calendar, returning the resulting start date.
    ///
    /// Mirrors LEAN's `HistoryRequestFactory.GetStartTimeAlgoTz` bar-count path:
    /// warmup should replay N *trading* bars, not N calendar days. Used to size
    /// indicator warmup windows so a 200-bar SMA sees 200 open sessions.
    pub fn warmup_start_date(
        &self,
        symbol: &Symbol,
        bar_count: usize,
        end_date: NaiveDate,
    ) -> NaiveDate {
        let exchange_hours = self.exchange_hours(symbol);
        let mut remaining = bar_count;
        let mut date = end_date;
        // Cap the search so a pathological calendar can't loop forever.
        let max_lookback_days = bar_count.saturating_mul(4).saturating_add(16) as i64;
        for _ in 0..max_lookback_days {
            if remaining == 0 {
                break;
            }
            date = match date.pred_opt() {
                Some(previous) => previous,
                None => break,
            };
            if let Some(midday) = date.and_hms_opt(12, 0, 0) {
                if exchange_hours.is_open_at_local_naive(midday) {
                    remaining -= 1;
                }
            }
        }
        date
    }
}

impl ExchangeHours {
    /// Convert an exchange-local calendar date at midnight to UTC.
    ///
    /// Universe data is stamped in the exchange time zone in C# LEAN. Using
    /// UTC midnight directly would surface a US universe on the prior
    /// algorithm date (20:00 ET during daylight saving time).
    pub fn local_midnight_utc(&self, date: NaiveDate) -> Option<NanosecondTimestamp> {
        let timezone: Tz = self.timezone.parse().ok()?;
        let midnight = timezone
            .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
            .earliest()?;
        Some(NanosecondTimestamp::from(
            midnight.with_timezone(&chrono::Utc),
        ))
    }

    /// Returns the first regular-session open and last regular-session close for
    /// `date` in UTC. Holidays and closed weekdays return `None`; late-open and
    /// early-close overrides are applied before converting through the exchange
    /// time zone.
    pub fn session_bounds(
        &self,
        date: NaiveDate,
    ) -> Option<(NanosecondTimestamp, NanosecondTimestamp)> {
        if self.holidays.contains(&date) {
            return None;
        }
        let schedule = &self.schedule[date.weekday().num_days_from_sunday() as usize];
        let first = schedule.sessions.first()?;
        let last = schedule.sessions.last()?;
        let open = self.late_opens.get(&date).copied().unwrap_or(first.open);
        let close = self.early_closes.get(&date).copied().unwrap_or(last.close);
        let midnight_utc = self.local_midnight_utc(date)?;
        Some((midnight_utc + open, midnight_utc + close))
    }

    pub fn us_equity() -> Self {
        let regular = MarketSession::new(9, 30, 16, 0);
        let (holidays, early_closes) = Self::us_equity_calendar();
        ExchangeHours {
            timezone: "America/New_York".into(),
            schedule: [
                DaySchedule::closed(),      // Sunday
                DaySchedule::open(regular), // Monday
                DaySchedule::open(regular), // Tuesday
                DaySchedule::open(regular), // Wednesday
                DaySchedule::open(regular), // Thursday
                DaySchedule::open(regular), // Friday
                DaySchedule::closed(),      // Saturday
            ],
            holidays,
            early_closes,
            late_opens: std::collections::HashMap::new(),
        }
    }

    pub fn forex_24h() -> Self {
        let session = MarketSession::new(0, 0, 23, 59);
        ExchangeHours {
            timezone: "UTC".into(),
            schedule: [
                DaySchedule::closed(), // Sunday (forex opens Sunday 5pm ET)
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::closed(), // Saturday
            ],
            holidays: HashSet::new(),
            early_closes: std::collections::HashMap::new(),
            late_opens: std::collections::HashMap::new(),
        }
    }

    pub fn crypto_24_7() -> Self {
        let session = MarketSession::new(0, 0, 23, 59);
        ExchangeHours {
            timezone: "UTC".into(),
            schedule: [
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
                DaySchedule::open(session),
            ],
            holidays: HashSet::new(),
            early_closes: std::collections::HashMap::new(),
            late_opens: std::collections::HashMap::new(),
        }
    }

    pub fn is_open_at(&self, ts: NanosecondTimestamp) -> bool {
        let tz: Tz = self.timezone.parse().unwrap_or(chrono_tz::UTC);
        let local = ts.to_tz(tz);
        let date = local.date_naive();

        if self.holidays.contains(&date) {
            return false;
        }

        let dow = local.weekday().num_days_from_sunday() as usize;
        let schedule = &self.schedule[dow];

        if !schedule.is_open() {
            return false;
        }

        let secs_since_midnight =
            local.hour() as i64 * 3600 + local.minute() as i64 * 60 + local.second() as i64;
        let day_nanos = secs_since_midnight * 1_000_000_000;

        // Check early close override
        let close_override = self.early_closes.get(&date);
        let open_override = self.late_opens.get(&date);

        schedule.sessions.iter().any(|s| {
            let open = open_override.map(|o| o.nanos).unwrap_or(s.open.nanos);
            let close = close_override.map(|c| c.nanos).unwrap_or(s.close.nanos);
            day_nanos >= open && day_nanos < close
        })
    }

    pub fn is_open_at_local_naive(&self, local: chrono::NaiveDateTime) -> bool {
        let tz: Tz = self.timezone.parse().unwrap_or(chrono_tz::UTC);
        let local = match tz.from_local_datetime(&local) {
            chrono::LocalResult::Single(local) => local,
            chrono::LocalResult::Ambiguous(earliest, _) => earliest,
            chrono::LocalResult::None => return false,
        };
        self.is_open_at(NanosecondTimestamp::from(local.with_timezone(&chrono::Utc)))
    }

    pub fn next_open(&self, from: NanosecondTimestamp) -> Option<NanosecondTimestamp> {
        // Search up to 10 days ahead
        let tz: Tz = self.timezone.parse().unwrap_or(chrono_tz::UTC);
        let start = from.to_tz(tz);

        for day_offset in 0i64..10 {
            let candidate_date = (start + chrono::Duration::days(day_offset)).date_naive();
            if self.holidays.contains(&candidate_date) {
                continue;
            }
            let dow = candidate_date.weekday().num_days_from_sunday() as usize;
            let schedule = &self.schedule[dow];
            if let Some(session) = schedule.sessions.first() {
                let open_nanos = session.open.nanos;
                let local_dt = tz
                    .from_local_datetime(&candidate_date.and_hms_opt(0, 0, 0).unwrap())
                    .unwrap();
                let utc_dt: chrono::DateTime<chrono::Utc> = local_dt.with_timezone(&chrono::Utc);
                let candidate =
                    NanosecondTimestamp(NanosecondTimestamp::from(utc_dt).0 + open_nanos);
                if candidate > from {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn us_equity_calendar() -> (HashSet<NaiveDate>, HashMap<NaiveDate, TimeSpan>) {
        let calendar = finance_dates::calendar_for_exchange("XNYS")
            .expect("finance-dates must provide the XNYS exchange calendar");
        let mut holidays = HashSet::new();
        let mut early_closes = HashMap::new();

        // Nanosecond timestamps span roughly 1678-2262. Materializing the
        // library's rules once keeps the existing serializable ExchangeHours
        // contract while making finance-dates the source of calendar truth.
        for year in 1678..=2262 {
            holidays.extend(calendar.holidays(year).iter().copied());
            let mut date = NaiveDate::from_ymd_opt(year, 1, 1)
                .expect("calendar materialization year must be valid");
            while date.year() == year {
                if let Some(close) = calendar.early_close_for(date) {
                    early_closes.insert(
                        date,
                        TimeSpan::from_secs(i64::from(close.num_seconds_from_midnight())),
                    );
                }
                let Some(next) = date.succ_opt() else {
                    break;
                };
                date = next;
            }
        }

        (holidays, early_closes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Market;

    #[test]
    fn local_naive_open_check_uses_exchange_definition() {
        let hours = ExchangeHours::us_equity();

        assert!(hours.is_open_at_local_naive(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 2)
                .unwrap()
                .and_hms_opt(10, 0, 0)
                .unwrap()
        ));
        assert!(!hours.is_open_at_local_naive(
            chrono::NaiveDate::from_ymd_opt(2024, 1, 6)
                .unwrap()
                .and_hms_opt(10, 0, 0)
                .unwrap()
        ));
    }

    #[test]
    fn market_hours_database_reuses_default_exchange_hours() {
        let db = MarketHoursDatabase::from_builtin_defaults();
        let spy = Symbol::create_equity("SPY", &Market::usa());
        let aapl = Symbol::create_equity("AAPL", &Market::usa());

        let spy_hours = db.exchange_hours(&spy);
        let aapl_hours = db.exchange_hours(&aapl);

        assert!(Arc::ptr_eq(&spy_hours, &aapl_hours));
    }

    #[test]
    fn market_hours_database_checks_equity_open_dates() {
        let db = MarketHoursDatabase::from_builtin_defaults();
        let spy = Symbol::create_equity("SPY", &Market::usa());

        assert!(db.is_open_date(&spy, chrono::NaiveDate::from_ymd_opt(2024, 1, 2).unwrap()));
        assert!(!db.is_open_date(&spy, chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap()));
        assert!(!db.is_open_date(&spy, chrono::NaiveDate::from_ymd_opt(2024, 1, 6).unwrap()));
    }

    #[test]
    fn market_hours_database_uses_historical_nyse_calendar() {
        let db = MarketHoursDatabase::from_builtin_defaults();
        let spy = Symbol::create_equity("SPY", &Market::usa());

        for date in [
            NaiveDate::from_ymd_opt(2003, 11, 27).unwrap(),
            NaiveDate::from_ymd_opt(2003, 12, 25).unwrap(),
            NaiveDate::from_ymd_opt(2004, 1, 1).unwrap(),
            NaiveDate::from_ymd_opt(2004, 6, 11).unwrap(),
            NaiveDate::from_ymd_opt(2012, 10, 29).unwrap(),
        ] {
            assert!(!db.is_open_date(&spy, date), "{date} must be closed");
        }
    }

    #[test]
    fn session_bounds_apply_exchange_timezone_and_close_overrides() {
        let mut hours = ExchangeHours::us_equity();
        let date = NaiveDate::from_ymd_opt(2024, 11, 29).unwrap();
        hours
            .early_closes
            .insert(date, TimeSpan::from_secs(13 * 60 * 60));

        let (open, close) = hours.session_bounds(date).unwrap();

        assert_eq!(
            open,
            NanosecondTimestamp::from(
                chrono::Utc
                    .with_ymd_and_hms(2024, 11, 29, 14, 30, 0)
                    .single()
                    .unwrap()
            )
        );
        assert_eq!(
            close,
            NanosecondTimestamp::from(
                chrono::Utc
                    .with_ymd_and_hms(2024, 11, 29, 18, 0, 0)
                    .single()
                    .unwrap()
            )
        );
    }

    #[test]
    fn local_midnight_uses_exchange_timezone_and_dst() {
        let hours = ExchangeHours::us_equity();

        assert_eq!(
            hours
                .local_midnight_utc(NaiveDate::from_ymd_opt(2024, 7, 18).unwrap())
                .unwrap()
                .to_utc(),
            chrono::Utc
                .with_ymd_and_hms(2024, 7, 18, 4, 0, 0)
                .single()
                .unwrap()
        );
        assert_eq!(
            hours
                .local_midnight_utc(NaiveDate::from_ymd_opt(2024, 12, 18).unwrap())
                .unwrap()
                .to_utc(),
            chrono::Utc
                .with_ymd_and_hms(2024, 12, 18, 5, 0, 0)
                .single()
                .unwrap()
        );
    }

    #[test]
    fn option_contract_expires_at_exchange_close_not_start_of_expiry_date() {
        let database = MarketHoursDatabase::from_builtin_defaults();
        let underlying = Symbol::create_equity("SPY", &Market::usa());
        let expiry = NaiveDate::from_ymd_opt(2024, 7, 24).unwrap();
        let option = Symbol::create_option_osi(
            underlying,
            rust_decimal_macros::dec!(531),
            expiry,
            crate::OptionRight::Call,
            crate::OptionStyle::American,
            &Market::usa(),
        );
        let hours = database.exchange_hours(&option);
        let (open, close) = hours.session_bounds(expiry).unwrap();

        assert!(!database.is_option_contract_expired(&option, open));
        assert!(!database.is_option_contract_expired(&option, close - TimeSpan::from_secs(60)));
        assert!(database.is_option_contract_expired(&option, close));
    }
}
