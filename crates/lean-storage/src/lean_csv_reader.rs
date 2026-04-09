/// Reads LEAN's original CSV/zip data format and converts to our types.
/// This enables in-place migration from the C# LEAN data directory.
use lean_core::{Market, OptionRight, OptionStyle, Resolution, Symbol, SymbolOptionsExt};
use lean_data::{QuoteBar, Tick, TradeBar};
use lean_core::Result as LeanResult;
use crate::schema::{FactorFileEntry, MapFileEntry};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use chrono::NaiveDate;
use rust_decimal::Decimal;
use tracing::warn;

// Scale factor used in LEAN's equity / option CSV files.
// Raw integer values in the CSV are `price * 10_000`.
const OPTION_SCALE_FACTOR: Decimal = Decimal::from_parts(1, 0, 0, false, 4); // 0.0001

/// A row from a LEAN option universe CSV file.
///
/// These files live at `option/{market}/universes/{underlying}/{date}.csv`.
/// Each non-header row contains OHLCV + open-interest for one option contract.
/// Prices in this file are NOT scaled — they are in dollars.
#[derive(Debug, Clone)]
pub struct OptionUniverseRow {
    /// Full OSI-style symbol value (e.g. "SPY 20210115 C 00350000").
    pub symbol_value: String,
    pub open: rust_decimal::Decimal,
    pub high: rust_decimal::Decimal,
    pub low: rust_decimal::Decimal,
    pub close: rust_decimal::Decimal,
    pub volume: i64,
    pub open_interest: i64,
}

pub struct LeanCsvReader;

impl LeanCsvReader {
    /// Read trade bars from a LEAN-format CSV file (possibly inside a zip).
    pub fn read_trade_bars_from_csv<R: Read>(
        reader: R,
        symbol: Symbol,
        date: NaiveDate,
        resolution: Resolution,
    ) -> Vec<TradeBar> {
        let buf = BufReader::new(reader);
        let mut bars = Vec::new();

        for (line_no, line) in buf.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => { warn!("Line {} read error: {}", line_no, e); continue; }
            };
            let line = line.trim().to_string();
            if line.is_empty() || line.starts_with('#') { continue; }

            match TradeBar::from_lean_csv_line(&line, symbol.clone(), date, resolution) {
                Some(bar) => bars.push(bar),
                None => warn!("Failed to parse trade bar at line {}: {}", line_no, line),
            }
        }

        bars
    }

    /// Read quote bars from a LEAN-format CSV file.
    pub fn read_quote_bars_from_csv<R: Read>(
        reader: R,
        symbol: Symbol,
        date: NaiveDate,
        period: lean_core::TimeSpan,
    ) -> Vec<QuoteBar> {
        let buf = BufReader::new(reader);
        let mut bars = Vec::new();

        for (line_no, line) in buf.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => { warn!("Line {} read error: {}", line_no, e); continue; }
            };
            let line = line.trim().to_string();
            if line.is_empty() || line.starts_with('#') { continue; }

            match QuoteBar::from_lean_csv_line(&line, symbol.clone(), date, period) {
                Some(bar) => bars.push(bar),
                None => warn!("Failed to parse quote bar at line {}: {}", line_no, line),
            }
        }

        bars
    }

    /// Read ticks from a LEAN-format trade CSV file.
    pub fn read_trade_ticks_from_csv<R: Read>(
        reader: R,
        symbol: Symbol,
        date: NaiveDate,
    ) -> Vec<Tick> {
        let buf = BufReader::new(reader);
        let mut ticks = Vec::new();

        for (line_no, line) in buf.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => { warn!("Line {} read error: {}", line_no, e); continue; }
            };
            let line = line.trim().to_string();
            if line.is_empty() { continue; }

            match Tick::from_lean_trade_csv(&line, symbol.clone(), date) {
                Some(tick) => ticks.push(tick),
                None => warn!("Failed to parse tick at line {}: {}", line_no, line),
            }
        }

        ticks
    }

    /// Parse quote bars from a LEAN option daily quote CSV.
    ///
    /// The CSV format (for daily/hourly resolution) is:
    /// `{datetime_12char},{bid_open*10000},{bid_high*10000},{bid_low*10000},{bid_close*10000},{bid_size},{ask_open*10000},{ask_high*10000},{ask_low*10000},{ask_close*10000},{ask_size}`
    ///
    /// Prices are scaled integers (×10000); this method divides by 10000.
    /// `osi_symbol` must be the fully-specified contract `Symbol`.
    /// `date` is the bar date (used to construct the timestamp).
    pub fn read_option_quote_bars_from_csv<R: Read>(
        reader: R,
        osi_symbol: Symbol,
        date: NaiveDate,
    ) -> Vec<lean_data::QuoteBar> {
        let period = lean_core::TimeSpan::from_nanos(
            chrono::Duration::days(1).num_nanoseconds().unwrap_or(86_400_000_000_000),
        );
        let buf = BufReader::new(reader);
        let mut bars = Vec::new();

        for (line_no, line) in buf.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => { warn!("Line {} read error: {}", line_no, e); continue; }
            };
            let line = line.trim().to_string();
            if line.is_empty() || line.starts_with('#') { continue; }

            match Self::parse_option_quote_bar_line(&line, osi_symbol.clone(), date, period) {
                Some(bar) => bars.push(bar),
                None => warn!("Failed to parse option quote bar at line {}: {}", line_no, line),
            }
        }

        bars
    }

    /// Parse trade bars from a LEAN option daily trade CSV.
    ///
    /// The CSV format (for daily/hourly resolution) is:
    /// `{datetime_12char},{open*10000},{high*10000},{low*10000},{close*10000},{volume}`
    ///
    /// Prices are scaled integers (×10000); this method divides by 10000.
    pub fn read_option_trade_bars_from_csv<R: Read>(
        reader: R,
        osi_symbol: Symbol,
        date: NaiveDate,
    ) -> Vec<lean_data::TradeBar> {
        let period = lean_core::TimeSpan::from_nanos(
            chrono::Duration::days(1).num_nanoseconds().unwrap_or(86_400_000_000_000),
        );
        let buf = BufReader::new(reader);
        let mut bars = Vec::new();

        for (line_no, line) in buf.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => { warn!("Line {} read error: {}", line_no, e); continue; }
            };
            let line = line.trim().to_string();
            if line.is_empty() || line.starts_with('#') { continue; }

            match Self::parse_option_trade_bar_line(&line, osi_symbol.clone(), date, period) {
                Some(bar) => bars.push(bar),
                None => warn!("Failed to parse option trade bar at line {}: {}", line_no, line),
            }
        }

        bars
    }

    /// Parse a LEAN option universe CSV for a given date.
    ///
    /// The universe CSV lives at `option/{market}/universes/{underlying}/{date}.csv`.
    ///
    /// Row format (with optional header starting with `#`):
    /// `{expiry_yyyyMMdd},{strike},{right},{open},{high},{low},{close},{volume},{open_interest}[,{iv},{delta},{gamma},{vega},{theta},{rho}]`
    ///
    /// The first row may be the underlying's own quote row (starts with `,,,` / empty expiry).
    /// This method skips that row and returns only contract rows.
    ///
    /// Prices are in dollars (not scaled).
    pub fn read_option_universe_csv<R: Read>(
        reader: R,
        underlying: &str,
        date: NaiveDate,
    ) -> Vec<OptionUniverseRow> {
        let buf = BufReader::new(reader);
        let mut rows = Vec::new();

        for (line_no, line) in buf.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => { warn!("Line {} read error: {}", line_no, e); continue; }
            };
            let line = line.trim().to_string();
            if line.is_empty() || line.starts_with('#') { continue; }

            // If the first character is not a digit it is the underlying equity row — skip it.
            if !line.starts_with(|c: char| c.is_ascii_digit()) { continue; }

            match Self::parse_option_universe_row(&line, underlying, date) {
                Some(row) => rows.push(row),
                None => warn!("Failed to parse option universe row at line {}: {}", line_no, line),
            }
        }

        rows
    }

    /// Read all option quote bars from a LEAN-format option zip file.
    ///
    /// LEAN stores each option contract as a separate CSV entry inside the zip.
    /// Each entry name encodes the contract details (underlying, tick type, style, right,
    /// scaled strike, expiry) — see `GenerateZipEntryName` in `LeanData.cs`.
    ///
    /// This helper iterates every entry whose name ends in `.csv`, parses the contract
    /// symbol from the filename, then calls [`read_option_quote_bars_from_csv`] on its
    /// contents.  All resulting bars are returned as a flat `Vec`.
    pub fn read_option_quotes_from_zip(
        zip_path: &Path,
        underlying: &str,
        date: NaiveDate,
        market: Market,
    ) -> LeanResult<Vec<lean_data::QuoteBar>> {
        use std::fs::File;
        let file = File::open(zip_path)?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        let mut bars = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

            let entry_name = entry.name().to_owned();
            if !entry_name.ends_with(".csv") { continue; }

            // Parse the contract symbol from the zip entry filename.
            let osi_symbol = match Self::parse_option_symbol_from_entry_name(
                &entry_name, underlying, &market,
            ) {
                Some(s) => s,
                None => {
                    warn!("Could not parse option symbol from zip entry: {}", entry_name);
                    continue;
                }
            };

            bars.extend(Self::read_option_quote_bars_from_csv(&mut entry, osi_symbol, date));
        }

        Ok(bars)
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Parse one option quote bar line (daily/hourly LEAN format).
    ///
    /// Format: `{datetime_12char},{bo*S},{bh*S},{bl*S},{bc*S},{bid_sz},{ao*S},{ah*S},{al*S},{ac*S},{ask_sz}`
    /// where `S = 10000`.
    fn parse_option_quote_bar_line(
        line: &str,
        symbol: Symbol,
        date: NaiveDate,
        period: lean_core::TimeSpan,
    ) -> Option<lean_data::QuoteBar> {
        let parts: Vec<&str> = line.splitn(12, ',').collect();
        if parts.len() < 11 { return None; }

        let time = Self::parse_daily_datetime(parts[0].trim(), date)?;

        let parse_scaled = |s: &str| -> Option<Decimal> {
            let raw: Decimal = s.trim().parse().ok()?;
            Some(raw * OPTION_SCALE_FACTOR)
        };

        // Bid side — only populate if any field is non-empty.
        let bid = if parts[1..=4].iter().any(|s| !s.trim().is_empty()) {
            Some(lean_data::Bar::new(
                parse_scaled(parts[1])?,
                parse_scaled(parts[2])?,
                parse_scaled(parts[3])?,
                parse_scaled(parts[4])?,
            ))
        } else {
            None
        };
        let last_bid_size: Decimal = parts[5].trim().parse().unwrap_or(Decimal::ZERO);

        // Ask side — only populate if any field is non-empty.
        let ask = if parts[6..=9].iter().any(|s| !s.trim().is_empty()) {
            Some(lean_data::Bar::new(
                parse_scaled(parts[6])?,
                parse_scaled(parts[7])?,
                parse_scaled(parts[8])?,
                parse_scaled(parts[9])?,
            ))
        } else {
            None
        };
        let last_ask_size: Decimal = parts[10].trim().parse().unwrap_or(Decimal::ZERO);

        Some(lean_data::QuoteBar::new(
            symbol,
            time,
            period,
            bid,
            ask,
            last_bid_size,
            last_ask_size,
        ))
    }

    /// Parse one option trade bar line (daily/hourly LEAN format).
    ///
    /// Format: `{datetime_12char},{open*S},{high*S},{low*S},{close*S},{volume}` where `S = 10000`.
    fn parse_option_trade_bar_line(
        line: &str,
        symbol: Symbol,
        date: NaiveDate,
        period: lean_core::TimeSpan,
    ) -> Option<lean_data::TradeBar> {
        let parts: Vec<&str> = line.splitn(7, ',').collect();
        if parts.len() < 6 { return None; }

        let time = Self::parse_daily_datetime(parts[0].trim(), date)?;

        let parse_scaled = |s: &str| -> Option<Decimal> {
            let raw: Decimal = s.trim().parse().ok()?;
            Some(raw * OPTION_SCALE_FACTOR)
        };

        let open = parse_scaled(parts[1])?;
        let high = parse_scaled(parts[2])?;
        let low  = parse_scaled(parts[3])?;
        let close = parse_scaled(parts[4])?;
        let volume: Decimal = parts[5].trim().parse().unwrap_or(Decimal::ZERO);

        Some(lean_data::TradeBar::new(
            symbol,
            time,
            period,
            open,
            high,
            low,
            close,
            volume,
        ))
    }

    /// Parse one row from a LEAN option universe CSV (contract row only).
    ///
    /// Row format:
    /// `{expiry_yyyyMMdd},{strike},{right},{open},{high},{low},{close},{volume},{open_interest}[,...]`
    fn parse_option_universe_row(
        line: &str,
        underlying: &str,
        date: NaiveDate,
    ) -> Option<OptionUniverseRow> {
        let parts: Vec<&str> = line.splitn(15, ',').collect();
        if parts.len() < 9 { return None; }

        let expiry = NaiveDate::parse_from_str(parts[0].trim(), "%Y%m%d").ok()?;
        let strike: Decimal = parts[1].trim().parse().ok()?;
        let right = match parts[2].trim().to_ascii_uppercase().as_str() {
            "C" | "CALL" => OptionRight::Call,
            "P" | "PUT"  => OptionRight::Put,
            _ => return None,
        };

        let open: Decimal  = parts[3].trim().parse().ok()?;
        let high: Decimal  = parts[4].trim().parse().ok()?;
        let low: Decimal   = parts[5].trim().parse().ok()?;
        let close: Decimal = parts[6].trim().parse().ok()?;
        let volume: i64    = parts[7].trim().parse().unwrap_or(0);
        let open_interest: i64 = parts[8].trim().parse().unwrap_or(0);

        // Build a human-readable symbol value that matches LEAN's OSI convention.
        let symbol_value = lean_core::format_option_ticker(underlying, strike, expiry, right);

        let _ = date; // date is the processing date; not needed for the symbol value itself

        Some(OptionUniverseRow {
            symbol_value,
            open,
            high,
            low,
            close,
            volume,
            open_interest,
        })
    }

    /// Parse the bar timestamp for daily/hourly LEAN option data.
    ///
    /// Daily/hourly entries use the "TwelveCharacter" format: `yyyyMMdd HH:mm`
    /// (14 chars without space, actually `yyyyMMdd HH:mm` = 14 chars but LEAN
    /// formats it as `yyyyMMdd HH:mm` using `"yyyyMMdd HH:mm"` — 14 characters).
    ///
    /// LEAN's `DateFormat.TwelveCharacter = "yyyyMMdd HH:mm"` is 14 characters.
    /// When parsing fails we fall back to treating the value as milliseconds
    /// since midnight (for sub-daily resolution data embedded in daily zips).
    fn parse_daily_datetime(s: &str, date: NaiveDate) -> Option<lean_core::DateTime> {
        use chrono::{TimeZone, Utc, NaiveDateTime};

        // Try the LEAN "TwelveCharacter" format first (daily/hourly).
        // LEAN calls it TwelveCharacter but it is actually 14 chars: "yyyyMMdd HH:mm"
        if let Ok(ndt) = NaiveDateTime::parse_from_str(s, "%Y%m%d %H:%M") {
            let utc = Utc.from_utc_datetime(&ndt);
            return Some(lean_core::NanosecondTimestamp(
                utc.timestamp() * 1_000_000_000,
            ));
        }

        // Fall back: milliseconds since midnight (minute/second/tick resolution).
        if let Ok(ms) = s.parse::<i64>() {
            let midnight = Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap());
            return Some(lean_core::NanosecondTimestamp(
                midnight.timestamp() * 1_000_000_000 + ms * 1_000_000,
            ));
        }

        None
    }

    /// Derive a LEAN option `Symbol` from a zip entry filename.
    ///
    /// LEAN daily option entry name format (equity option):
    /// `{underlying}_{ticktype}_{style}_{right}_{strike_scaled}_{expiry_yyyymmdd}.csv`
    ///
    /// Example: `spy_quote_american_call_3500000_20210115.csv`
    ///
    /// This method only handles equity option entries; it returns `None` for
    /// any entry that does not match the expected structure.
    fn parse_option_symbol_from_entry_name(
        entry_name: &str,
        underlying: &str,
        market: &Market,
    ) -> Option<Symbol> {
        // Strip the `.csv` extension.
        let stem = entry_name.strip_suffix(".csv")?;
        let parts: Vec<&str> = stem.split('_').collect();

        // Expected parts: [underlying, ticktype, style, right, strike_scaled, expiry]
        // Minimum 6 parts; underlying may itself contain underscores in some edge cases
        // but for equities it is always a single token.
        if parts.len() < 6 { return None; }

        // Walk from the end to pick up fixed-position tail fields.
        // parts[-1] = expiry (8 digits)
        // parts[-2] = strike_scaled (integer)
        // parts[-3] = right ("call" / "put")
        // parts[-4] = style ("american" / "european")
        // parts[-5] = ticktype ("trade" / "quote" / "openinterest")
        // parts[0..parts.len()-5] = underlying (often single token)
        let n = parts.len();
        if n < 6 { return None; }

        let expiry_str  = parts[n - 1];
        let strike_str  = parts[n - 2];
        let right_str   = parts[n - 3];
        let style_str   = parts[n - 4];
        // parts[n-5] is tick type — we only need it to confirm it's an option entry
        // parts[0..n-5] is the underlying name

        let expiry = NaiveDate::parse_from_str(expiry_str, "%Y%m%d").ok()?;

        let strike_scaled: i64 = strike_str.parse().ok()?;
        let strike = Decimal::from(strike_scaled) * OPTION_SCALE_FACTOR;

        let right = match right_str.to_ascii_lowercase().as_str() {
            "call" => OptionRight::Call,
            "put"  => OptionRight::Put,
            _ => return None,
        };

        let style = match style_str.to_ascii_lowercase().as_str() {
            "american" => OptionStyle::American,
            "european" => OptionStyle::European,
            _ => return None,
        };

        let underlying_sym = Symbol::create_equity(underlying, market);
        Some(Symbol::create_option_osi(
            underlying_sym,
            strike,
            expiry,
            right,
            style,
            market,
        ))
    }

    /// Read from a zip file containing a single LEAN CSV.
    pub fn read_trade_bars_from_zip(
        path: &Path,
        symbol: Symbol,
        date: NaiveDate,
        resolution: Resolution,
    ) -> LeanResult<Vec<TradeBar>> {
        use std::fs::File;
        let file = File::open(path)?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;

        // LEAN zips contain one file named after the date
        let mut bars = Vec::new();
        for i in 0..archive.len() {
            let entry = archive.by_index(i)
                .map_err(|e| lean_core::LeanError::DataError(e.to_string()))?;
            if entry.name().ends_with(".csv") {
                bars.extend(Self::read_trade_bars_from_csv(entry, symbol.clone(), date, resolution));
            }
        }

        Ok(bars)
    }

    /// Read factor file entries from a LEAN-format CSV.
    ///
    /// LEAN factor file CSV format per row:
    ///   `{yyyyMMdd},{price_factor},{split_factor},{reference_price}`
    ///
    /// Lines containing `"inf"` or `"e+"` are skipped (following C# LEAN's
    /// `CorporateFactorRow.Parse` behaviour).  Lines with a zero combined
    /// price-scale factor (`price_factor * split_factor == 0`) are also skipped.
    ///
    /// The `reference_price` column is optional; it defaults to `0.0` when absent.
    pub fn read_factor_file_from_csv<R: Read>(reader: R) -> Vec<FactorFileEntry> {
        let buf = BufReader::new(reader);
        let mut entries = Vec::new();

        for (line_no, line) in buf.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => { warn!("FactorFile CSV line {} read error: {}", line_no, e); continue; }
            };
            let line = line.trim().to_string();
            if line.is_empty() || line.starts_with('#') { continue; }

            // Skip lines with overflow markers (LEAN convention)
            if line.contains("inf") || line.contains("e+") { continue; }

            let parts: Vec<&str> = line.splitn(5, ',').collect();
            if parts.len() < 3 {
                warn!("FactorFile CSV line {}: too few columns: {}", line_no, line);
                continue;
            }

            let date = match NaiveDate::parse_from_str(parts[0].trim(), "%Y%m%d") {
                Ok(d) => d,
                Err(_) => {
                    warn!("FactorFile CSV line {}: bad date '{}': {}", line_no, parts[0], line);
                    continue;
                }
            };

            let price_factor: f64 = match parts[1].trim().parse() {
                Ok(v) => v,
                Err(_) => { warn!("FactorFile CSV line {}: bad price_factor: {}", line_no, line); continue; }
            };

            let split_factor: f64 = match parts[2].trim().parse() {
                Ok(v) => v,
                Err(_) => { warn!("FactorFile CSV line {}: bad split_factor: {}", line_no, line); continue; }
            };

            // Skip rows where the combined price scale factor is zero
            if price_factor * split_factor == 0.0 { continue; }

            let reference_price: f64 = if parts.len() >= 4 {
                parts[3].trim().parse().unwrap_or(0.0)
            } else {
                0.0
            };

            entries.push(FactorFileEntry { date, price_factor, split_factor, reference_price });
        }

        entries
    }

    /// Read map file entries from a LEAN-format CSV.
    ///
    /// LEAN map file CSV format per row:
    ///   `{yyyyMMdd},{ticker}[,{exchange}[,{mapping_mode}]]`
    ///
    /// Only the date and ticker columns are extracted; the optional exchange
    /// and mapping-mode columns are ignored.  The ticker is stored in uppercase
    /// to match the conventions used by `MapFileEntry`.
    pub fn read_map_file_from_csv<R: Read>(reader: R) -> Vec<MapFileEntry> {
        let buf = BufReader::new(reader);
        let mut entries = Vec::new();

        for (line_no, line) in buf.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(e) => { warn!("MapFile CSV line {} read error: {}", line_no, e); continue; }
            };
            let line = line.trim().to_string();
            if line.is_empty() || line.starts_with('#') { continue; }

            let parts: Vec<&str> = line.splitn(5, ',').collect();
            if parts.len() < 2 {
                warn!("MapFile CSV line {}: too few columns: {}", line_no, line);
                continue;
            }

            let date = match NaiveDate::parse_from_str(parts[0].trim(), "%Y%m%d") {
                Ok(d) => d,
                Err(_) => {
                    warn!("MapFile CSV line {}: bad date '{}': {}", line_no, parts[0], line);
                    continue;
                }
            };

            let ticker = parts[1].trim().to_uppercase();
            if ticker.is_empty() {
                warn!("MapFile CSV line {}: empty ticker: {}", line_no, line);
                continue;
            }

            entries.push(MapFileEntry { date, ticker });
        }

        entries
    }

    /// Convert an entire LEAN data directory tree to parquet.
    pub async fn migrate_directory(
        lean_data_root: &Path,
        parquet_data_root: &Path,
        symbol: &Symbol,
        resolution: Resolution,
    ) -> LeanResult<usize> {
        use crate::{ParquetWriter, WriterConfig, path_resolver::PathResolver};
        use chrono::NaiveDate;
        use std::fs;
        use tracing::info;

        let writer = ParquetWriter::new(WriterConfig::default());
        let resolver = PathResolver::new(parquet_data_root);
        let mut total = 0usize;

        // Walk lean_data_root for matching CSV/zip files
        let sec_type = format!("{}", symbol.security_type()).to_lowercase();
        let market = symbol.market().as_str().to_lowercase();
        let ticker = symbol.value.to_lowercase();
        let res_folder = resolution.folder_name();

        let csv_dir = lean_data_root
            .join(&sec_type)
            .join(&market)
            .join(res_folder)
            .join(&ticker);

        if !csv_dir.exists() {
            return Ok(0);
        }

        for entry in fs::read_dir(&csv_dir)? {
            let entry = entry?;
            let path = entry.path();
            let fname = path.file_name().unwrap_or_default().to_string_lossy();

            // Parse date from filename like 20231027_trade.csv
            let date_str = fname.split('_').next().unwrap_or("");
            let date = match NaiveDate::parse_from_str(date_str, "%Y%m%d") {
                Ok(d) => d,
                Err(_) => continue,
            };

            let file = fs::File::open(&path)?;
            let bars = Self::read_trade_bars_from_csv(file, symbol.clone(), date, resolution);

            if !bars.is_empty() {
                let data_path = resolver.trade_bar(symbol, resolution, date);
                writer.write_trade_bars_at(&bars, &data_path)?;
                total += bars.len();
                info!("Migrated {} bars for {} {}", bars.len(), symbol, date);
            }
        }

        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use lean_core::{Market, OptionRight, OptionStyle, SymbolOptionsExt};
    use rust_decimal_macros::dec;

    fn spy_market() -> Market {
        Market::usa()
    }

    fn spy_underlying() -> Symbol {
        Symbol::create_equity("SPY", &spy_market())
    }

    fn make_spy_call(strike: Decimal, expiry: NaiveDate) -> Symbol {
        Symbol::create_option_osi(
            spy_underlying(),
            strike,
            expiry,
            OptionRight::Call,
            OptionStyle::American,
            &spy_market(),
        )
    }

    // -----------------------------------------------------------------------
    // Option quote bar CSV
    // -----------------------------------------------------------------------

    /// LEAN daily option quote CSV line (equity option, scaled ×10000):
    /// `20210115 16:00,7500,8000,7000,7600,10,7700,8100,7100,7700,8`
    ///
    /// Expected (after ÷10000):
    ///   bid: O=0.75, H=0.80, L=0.70, C=0.76, size=10
    ///   ask: O=0.77, H=0.81, L=0.71, C=0.77, size=8
    #[test]
    fn test_parse_option_quote_bar_line() {
        let csv = "20210115 16:00,7500,8000,7000,7600,10,7700,8100,7100,7700,8\n";
        let expiry = NaiveDate::from_ymd_opt(2021, 1, 15).unwrap();
        let symbol = make_spy_call(dec!(2.50), expiry);
        let date = expiry;

        let bars = LeanCsvReader::read_option_quote_bars_from_csv(
            csv.as_bytes(),
            symbol.clone(),
            date,
        );

        assert_eq!(bars.len(), 1, "Expected exactly one bar");
        let bar = &bars[0];

        let bid = bar.bid.as_ref().expect("Bid should be populated");
        assert_eq!(bid.open,  dec!(0.75));
        assert_eq!(bid.high,  dec!(0.80));
        assert_eq!(bid.low,   dec!(0.70));
        assert_eq!(bid.close, dec!(0.76));
        assert_eq!(bar.last_bid_size, dec!(10));

        let ask = bar.ask.as_ref().expect("Ask should be populated");
        assert_eq!(ask.open,  dec!(0.77));
        assert_eq!(ask.high,  dec!(0.81));
        assert_eq!(ask.low,   dec!(0.71));
        assert_eq!(ask.close, dec!(0.77));
        assert_eq!(bar.last_ask_size, dec!(8));
    }

    /// A CSV with a header comment and multiple contract lines should parse all
    /// non-header lines and skip the header.
    #[test]
    fn test_option_quote_csv_skips_header() {
        let csv = "# some header comment\n\
                   20210115 16:00,7500,8000,7000,7600,10,7700,8100,7100,7700,8\n\
                   20210115 16:00,4000,4500,3900,4200,5,4100,4600,4000,4300,3\n";
        let expiry = NaiveDate::from_ymd_opt(2021, 1, 15).unwrap();
        let symbol = make_spy_call(dec!(3.50), expiry);

        let bars = LeanCsvReader::read_option_quote_bars_from_csv(
            csv.as_bytes(),
            symbol.clone(),
            expiry,
        );

        assert_eq!(bars.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Option trade bar CSV
    // -----------------------------------------------------------------------

    /// LEAN daily option trade CSV line (scaled ×10000):
    /// `20210115 16:00,7600,8100,7100,7700,50`
    ///
    /// Expected (after ÷10000): O=0.76, H=0.81, L=0.71, C=0.77, V=50
    #[test]
    fn test_parse_option_trade_bar_line() {
        let csv = "20210115 16:00,7600,8100,7100,7700,50\n";
        let expiry = NaiveDate::from_ymd_opt(2021, 1, 15).unwrap();
        let symbol = make_spy_call(dec!(3.50), expiry);

        let bars = LeanCsvReader::read_option_trade_bars_from_csv(
            csv.as_bytes(),
            symbol.clone(),
            expiry,
        );

        assert_eq!(bars.len(), 1);
        let bar = &bars[0];
        assert_eq!(bar.open,   dec!(0.76));
        assert_eq!(bar.high,   dec!(0.81));
        assert_eq!(bar.low,    dec!(0.71));
        assert_eq!(bar.close,  dec!(0.77));
        assert_eq!(bar.volume, dec!(50));
    }

    /// Verify that the scale factor applied to option trade bars matches the C# constant
    /// `_scaleFactor = 1/10000m`.  A raw value of 10000 should produce exactly 1.0.
    #[test]
    fn test_option_trade_bar_scale_factor_is_one_over_ten_thousand() {
        let csv = "20210115 16:00,10000,10000,10000,10000,1\n";
        let expiry = NaiveDate::from_ymd_opt(2021, 1, 15).unwrap();
        let symbol = make_spy_call(dec!(1.00), expiry);

        let bars = LeanCsvReader::read_option_trade_bars_from_csv(
            csv.as_bytes(),
            symbol.clone(),
            expiry,
        );

        assert_eq!(bars.len(), 1);
        assert_eq!(bars[0].open,  dec!(1.0));
        assert_eq!(bars[0].close, dec!(1.0));
    }

    // -----------------------------------------------------------------------
    // Option universe CSV
    // -----------------------------------------------------------------------

    /// Minimal option universe CSV (no greeks columns):
    /// Header + one contract row.
    #[test]
    fn test_parse_option_universe_csv_basic() {
        // Format: {expiry},{strike},{right},{open},{high},{low},{close},{volume},{open_interest}
        let csv = "#expiry,strike,right,open,high,low,close,volume,open_interest\n\
                   20210115,350,C,2.50,3.10,2.30,2.80,100,500\n";

        let date = NaiveDate::from_ymd_opt(2021, 1, 14).unwrap();
        let rows = LeanCsvReader::read_option_universe_csv(csv.as_bytes(), "SPY", date);

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.open,  dec!(2.50));
        assert_eq!(row.high,  dec!(3.10));
        assert_eq!(row.low,   dec!(2.30));
        assert_eq!(row.close, dec!(2.80));
        assert_eq!(row.volume, 100);
        assert_eq!(row.open_interest, 500);
    }

    /// The underlying equity row (starts with a comma / non-digit) should be skipped.
    #[test]
    fn test_option_universe_csv_skips_underlying_row() {
        // First row is the underlying (starts with commas for empty expiry/strike/right)
        let csv = "#expiry,strike,right,open,high,low,close,volume,open_interest\n\
                   ,,,450.00,455.00,448.00,452.00,5000000,\n\
                   20210115,350,C,2.50,3.10,2.30,2.80,100,500\n\
                   20210115,355,P,1.80,2.20,1.70,2.00,80,300\n";

        let date = NaiveDate::from_ymd_opt(2021, 1, 14).unwrap();
        let rows = LeanCsvReader::read_option_universe_csv(csv.as_bytes(), "SPY", date);

        // Only the two contract rows should be returned (underlying row skipped)
        assert_eq!(rows.len(), 2);
    }

    /// The symbol_value should be formatted as an OSI ticker.
    /// SPY, $350 call, expiry 2021-01-15 → "SPY210115C00350000"
    #[test]
    fn test_option_universe_csv_symbol_value_osi_format() {
        let csv = "20210115,350,C,2.50,3.10,2.30,2.80,100,500\n";
        let date = NaiveDate::from_ymd_opt(2021, 1, 14).unwrap();
        let rows = LeanCsvReader::read_option_universe_csv(csv.as_bytes(), "SPY", date);

        assert_eq!(rows.len(), 1);
        // OSI format: {UNDERLYING}{YYMMDD}{C|P}{strike*1000:08}
        // SPY, expiry 2021-01-15, call, strike 350 → SPY210115C00350000
        assert_eq!(rows[0].symbol_value, "SPY210115C00350000");
    }

    // -----------------------------------------------------------------------
    // Zip entry name parser
    // -----------------------------------------------------------------------

    /// Daily equity option zip entry:
    /// `spy_quote_american_call_3500000_20210115.csv`
    /// Expected: SPY, American call, strike=350.00, expiry=2021-01-15
    #[test]
    fn test_parse_option_symbol_from_entry_name_call() {
        let market = spy_market();
        let sym = LeanCsvReader::parse_option_symbol_from_entry_name(
            "spy_quote_american_call_3500000_20210115.csv",
            "SPY",
            &market,
        );
        assert!(sym.is_some(), "Should parse successfully");
        let sym = sym.unwrap();

        let sid = &sym.id;
        assert_eq!(sid.option_right, Some(OptionRight::Call));
        assert_eq!(sid.option_style, Some(OptionStyle::American));
        // Strike: 3500000 * 0.0001 = 350.0
        assert_eq!(sid.strike, Some(dec!(350.0)));
        assert_eq!(sid.expiry, Some(NaiveDate::from_ymd_opt(2021, 1, 15).unwrap()));
    }

    #[test]
    fn test_parse_option_symbol_from_entry_name_put() {
        let market = spy_market();
        let sym = LeanCsvReader::parse_option_symbol_from_entry_name(
            "spy_quote_american_put_3500000_20210115.csv",
            "SPY",
            &market,
        );
        assert!(sym.is_some());
        let sym = sym.unwrap();
        assert_eq!(sym.id.option_right, Some(OptionRight::Put));
    }

    #[test]
    fn test_parse_option_symbol_from_entry_name_invalid_returns_none() {
        let market = spy_market();
        // Too few parts
        let result = LeanCsvReader::parse_option_symbol_from_entry_name(
            "spy_quote.csv",
            "SPY",
            &market,
        );
        assert!(result.is_none());
    }
}
