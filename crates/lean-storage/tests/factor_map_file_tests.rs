/// Integration tests for factor file and map file support in lean-storage.
///
/// These tests verify:
/// 1. CSV parsing matches LEAN's C# CorporateFactorRow / MapFileRow format exactly.
/// 2. Parquet round-trips (write → read) preserve all field values.
/// 3. Path generation matches LEAN's canonical directory layout.

use chrono::NaiveDate;
use lean_storage::{
    FactorFileEntry, MapFileEntry,
    ParquetReader, ParquetWriter, WriterConfig,
    factor_file_path, map_file_path,
    lean_csv_reader::LeanCsvReader,
    path_resolver::PathResolver,
};
use tempfile::TempDir;

// ─── helpers ────────────────────────────────────────────────────────────────

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

// ─── Factor file CSV parsing ─────────────────────────────────────────────────

/// LEAN factor file CSV format (CorporateFactorRow.GetFileFormat):
///   `{yyyyMMdd},{price_factor},{split_factor},{reference_price}`
///
/// The sentinel row (newest date, all factors = 1.0) marks the last verified date.
#[test]
fn factor_csv_parses_sentinel_row() {
    // Sentinel row: today, all factors 1.0, reference price 0
    let csv = "20240101,1.0000000,1.00000000,0\n";
    let entries = LeanCsvReader::read_factor_file_from_csv(csv.as_bytes());

    assert_eq!(entries.len(), 1);
    let e = &entries[0];
    assert_eq!(e.date, date(2024, 1, 1));
    assert!((e.price_factor - 1.0).abs() < 1e-9);
    assert!((e.split_factor - 1.0).abs() < 1e-9);
    assert!((e.reference_price - 0.0).abs() < 1e-9);
}

/// Parse a factor file with a split row (AAPL 4-for-1 split 2020-08-31).
/// split_factor = 1/4 = 0.25, price_factor = 1.0, reference_price = unadjusted close.
#[test]
fn factor_csv_parses_split_row() {
    let csv = "\
20240101,1.0000000,1.00000000,0\n\
20200831,1.0000000,0.25000000,128.96\n\
";
    let entries = LeanCsvReader::read_factor_file_from_csv(csv.as_bytes());

    assert_eq!(entries.len(), 2);

    let sentinel = &entries[0];
    assert_eq!(sentinel.date, date(2024, 1, 1));
    assert!((sentinel.split_factor - 1.0).abs() < 1e-9);

    let split = &entries[1];
    assert_eq!(split.date, date(2020, 8, 31));
    assert!((split.price_factor - 1.0).abs() < 1e-9);
    assert!((split.split_factor - 0.25).abs() < 1e-9);
    assert!((split.reference_price - 128.96).abs() < 1e-4);
}

/// Lines containing "inf" or "e+" are skipped (LEAN overflows).
#[test]
fn factor_csv_skips_inf_lines() {
    let csv = "\
20240101,1.0000000,1.00000000,0\n\
20100101,inf,inf,0\n\
20050601,1.6e+6,0.25000000,0\n\
";
    let entries = LeanCsvReader::read_factor_file_from_csv(csv.as_bytes());
    // Only the sentinel row should survive.
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].date, date(2024, 1, 1));
}

/// Comment lines (starting with `#`) and blank lines are skipped.
#[test]
fn factor_csv_skips_comments_and_blanks() {
    let csv = "\
# This is a comment\n\
\n\
20240101,1.0000000,1.00000000,0\n\
\n\
";
    let entries = LeanCsvReader::read_factor_file_from_csv(csv.as_bytes());
    assert_eq!(entries.len(), 1);
}

/// A factor row where price_factor * split_factor == 0 is skipped.
#[test]
fn factor_csv_skips_zero_scale_rows() {
    let csv = "\
20240101,1.0000000,1.00000000,0\n\
20200101,0.0,1.0,100\n\
";
    let entries = LeanCsvReader::read_factor_file_from_csv(csv.as_bytes());
    // The row with price_factor = 0 is dropped.
    assert_eq!(entries.len(), 1);
}

/// The reference_price column is optional; it defaults to 0.0 when absent.
#[test]
fn factor_csv_parses_without_reference_price() {
    // Only 3 columns (no reference_price)
    let csv = "20240101,1.0000000,1.00000000\n";
    let entries = LeanCsvReader::read_factor_file_from_csv(csv.as_bytes());
    assert_eq!(entries.len(), 1);
    assert!((entries[0].reference_price - 0.0).abs() < 1e-9);
}

// ─── Map file CSV parsing ────────────────────────────────────────────────────

/// LEAN map file CSV format (MapFileRow.ToCsv):
///   `{yyyyMMdd},{ticker_lower}[,{exchange}[,{mapping_mode}]]`
///
/// The ticker is stored lowercase in the file; we store it uppercase.
#[test]
fn map_csv_parses_simple_rows() {
    // A typical SPY map file: listed 1993-01-29 to (far future 20501231)
    let csv = "\
19930129,spy\n\
20501231,spy\n\
";
    let entries = LeanCsvReader::read_map_file_from_csv(csv.as_bytes());
    assert_eq!(entries.len(), 2);

    assert_eq!(entries[0].date, date(1993, 1, 29));
    assert_eq!(entries[0].ticker, "SPY");

    assert_eq!(entries[1].date, date(2050, 12, 31));
    assert_eq!(entries[1].ticker, "SPY");
}

/// Optional exchange and mapping_mode columns are ignored.
#[test]
fn map_csv_ignores_extra_columns() {
    // 4-column format: date,ticker,exchange,mapping_mode
    let csv = "20200101,googl,Q,0\n";
    let entries = LeanCsvReader::read_map_file_from_csv(csv.as_bytes());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].ticker, "GOOGL");
}

/// Ticker from CSV is always stored uppercase regardless of case in file.
#[test]
fn map_csv_uppercases_ticker() {
    let csv = "20200101,googl\n";
    let entries = LeanCsvReader::read_map_file_from_csv(csv.as_bytes());
    assert_eq!(entries[0].ticker, "GOOGL");
}

/// Comment lines and blank lines are skipped.
#[test]
fn map_csv_skips_comments_and_blanks() {
    let csv = "\
# map file header\n\
\n\
19930129,spy\n\
";
    let entries = LeanCsvReader::read_map_file_from_csv(csv.as_bytes());
    assert_eq!(entries.len(), 1);
}

/// A renamed ticker scenario: stock traded as GOOG then GOOGL.
#[test]
fn map_csv_parses_ticker_rename() {
    let csv = "\
20040819,goog\n\
20141002,googl\n\
20501231,googl\n\
";
    let entries = LeanCsvReader::read_map_file_from_csv(csv.as_bytes());
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].ticker, "GOOG");
    assert_eq!(entries[1].ticker, "GOOGL");
    assert_eq!(entries[2].ticker, "GOOGL");
}

// ─── Factor file Parquet round-trip ─────────────────────────────────────────

#[test]
fn factor_file_parquet_round_trip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("spy.parquet");

    let entries = vec![
        FactorFileEntry { date: date(2024, 1, 1), price_factor: 1.0,  split_factor: 1.0,  reference_price: 0.0 },
        FactorFileEntry { date: date(2020, 8, 31), price_factor: 1.0,  split_factor: 0.25, reference_price: 128.96 },
        FactorFileEntry { date: date(2014, 6, 9),  price_factor: 1.0,  split_factor: 1.0 / 28.0, reference_price: 92.44 },
    ];

    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_factor_file(&entries, &path).unwrap();

    assert!(path.exists(), "Parquet file should have been created");

    let reader = ParquetReader::new();
    let read_back = reader.read_factor_file(&path).unwrap();

    assert_eq!(read_back.len(), entries.len());
    for (orig, got) in entries.iter().zip(read_back.iter()) {
        assert_eq!(orig.date, got.date, "date mismatch");
        assert!((orig.price_factor - got.price_factor).abs() < 1e-9, "price_factor mismatch");
        assert!((orig.split_factor - got.split_factor).abs() < 1e-9, "split_factor mismatch");
        assert!((orig.reference_price - got.reference_price).abs() < 1e-4, "reference_price mismatch");
    }
}

/// `read_factor_file` returns an empty `Vec` (no error) when the file is missing.
#[test]
fn factor_file_read_returns_empty_when_missing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nonexistent.parquet");
    let reader = ParquetReader::new();
    let entries = reader.read_factor_file(&path).unwrap();
    assert!(entries.is_empty());
}

/// `write_factor_file` creates parent directories as needed.
#[test]
fn factor_file_write_creates_parent_directories() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("equity/usa/factor_files/spy.parquet");

    let entries = vec![
        FactorFileEntry { date: date(2024, 1, 1), price_factor: 1.0, split_factor: 1.0, reference_price: 0.0 },
    ];
    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_factor_file(&entries, &path).unwrap();
    assert!(path.exists());
}

// ─── Map file Parquet round-trip ─────────────────────────────────────────────

#[test]
fn map_file_parquet_round_trip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("spy.parquet");

    let entries = vec![
        MapFileEntry { date: date(1993, 1, 29), ticker: "SPY".to_string() },
        MapFileEntry { date: date(2050, 12, 31), ticker: "SPY".to_string() },
    ];

    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_map_file(&entries, &path).unwrap();

    assert!(path.exists(), "Parquet file should have been created");

    let reader = ParquetReader::new();
    let read_back = reader.read_map_file(&path).unwrap();

    assert_eq!(read_back.len(), entries.len());
    for (orig, got) in entries.iter().zip(read_back.iter()) {
        assert_eq!(orig.date, got.date, "date mismatch");
        assert_eq!(orig.ticker, got.ticker, "ticker mismatch");
    }
}

/// `read_map_file` returns an empty `Vec` (no error) when the file is missing.
#[test]
fn map_file_read_returns_empty_when_missing() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nonexistent.parquet");
    let reader = ParquetReader::new();
    let entries = reader.read_map_file(&path).unwrap();
    assert!(entries.is_empty());
}

/// `write_map_file` creates parent directories as needed.
#[test]
fn map_file_write_creates_parent_directories() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("equity/usa/map_files/spy.parquet");

    let entries = vec![
        MapFileEntry { date: date(1993, 1, 29), ticker: "SPY".to_string() },
    ];
    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_map_file(&entries, &path).unwrap();
    assert!(path.exists());
}

// ─── Path generation ────────────────────────────────────────────────────────

/// Factor file path: `{root}/equity/{market}/factor_files/{ticker_lower}.parquet`
#[test]
fn factor_file_path_matches_lean_convention() {
    use std::path::PathBuf;
    let root = PathBuf::from("/data");
    let p = factor_file_path(&root, "usa", "SPY");
    assert_eq!(p, PathBuf::from("/data/equity/usa/factor_files/spy.parquet"));
}

/// Factor file path with lowercase ticker input.
#[test]
fn factor_file_path_lowercases_ticker() {
    use std::path::PathBuf;
    let root = PathBuf::from("/data");
    let p = factor_file_path(&root, "usa", "AAPL");
    assert_eq!(p, PathBuf::from("/data/equity/usa/factor_files/aapl.parquet"));
}

/// Map file path: `{root}/equity/{market}/map_files/{ticker_lower}.parquet`
#[test]
fn map_file_path_matches_lean_convention() {
    use std::path::PathBuf;
    let root = PathBuf::from("/data");
    let p = map_file_path(&root, "usa", "SPY");
    assert_eq!(p, PathBuf::from("/data/equity/usa/map_files/spy.parquet"));
}

/// `PathResolver::factor_file` returns the same path as the free function.
#[test]
fn path_resolver_factor_file_matches_free_function() {
    use std::path::PathBuf;
    let root = PathBuf::from("/data");
    let resolver = PathResolver::new(&root);
    assert_eq!(resolver.factor_file("usa", "SPY"), factor_file_path(&root, "usa", "SPY"));
}

/// `PathResolver::map_file` returns the same path as the free function.
#[test]
fn path_resolver_map_file_matches_free_function() {
    use std::path::PathBuf;
    let root = PathBuf::from("/data");
    let resolver = PathResolver::new(&root);
    assert_eq!(resolver.map_file("usa", "SPY"), map_file_path(&root, "usa", "SPY"));
}

// ─── CSV → Parquet full pipeline ─────────────────────────────────────────────

/// Parse a C# factor file CSV and write/read it back through Parquet.
#[test]
fn factor_file_csv_to_parquet_pipeline() {
    // This CSV mirrors what LEAN's CorporateFactorRow.GetFileFormat() produces
    // for AAPL: sentinel + two historical splits.
    let csv = "\
20240101,1.0000000,1.00000000,0\n\
20200831,1.0000000,0.25000000,128.96\n\
20140609,1.0000000,0.03571429,95.22\n\
";

    let parsed = LeanCsvReader::read_factor_file_from_csv(csv.as_bytes());
    assert_eq!(parsed.len(), 3);

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("aapl.parquet");

    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_factor_file(&parsed, &path).unwrap();

    let reader = ParquetReader::new();
    let round_tripped = reader.read_factor_file(&path).unwrap();

    assert_eq!(round_tripped.len(), 3);
    assert_eq!(round_tripped[0].date, date(2024, 1, 1));
    assert_eq!(round_tripped[1].date, date(2020, 8, 31));
    assert_eq!(round_tripped[2].date, date(2014, 6, 9));

    // split_factor = 1/7 = 0.14285714...  ~ 0.03571429
    let expected_sf = 1.0 / 28.0;
    assert!((round_tripped[2].split_factor - expected_sf).abs() < 1e-6,
        "expected split_factor≈{:.8}, got {:.8}", expected_sf, round_tripped[2].split_factor);
}

/// Parse a C# map file CSV and write/read it back through Parquet.
#[test]
fn map_file_csv_to_parquet_pipeline() {
    // Realistic GOOGL map file (GOOG renamed to GOOGL in 2014).
    let csv = "\
20040819,goog\n\
20141002,googl\n\
20501231,googl\n\
";

    let parsed = LeanCsvReader::read_map_file_from_csv(csv.as_bytes());
    assert_eq!(parsed.len(), 3);

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("googl.parquet");

    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_map_file(&parsed, &path).unwrap();

    let reader = ParquetReader::new();
    let round_tripped = reader.read_map_file(&path).unwrap();

    assert_eq!(round_tripped.len(), 3);
    assert_eq!(round_tripped[0].ticker, "GOOG");
    assert_eq!(round_tripped[1].ticker, "GOOGL");
    assert_eq!(round_tripped[2].ticker, "GOOGL");
}
