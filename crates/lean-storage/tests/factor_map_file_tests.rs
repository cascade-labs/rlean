/// Integration tests for factor file and map file support in lean-storage.
///
/// These tests verify:
/// 1. Parquet round-trips (write → read) preserve all field values.
/// 2. Path generation matches LEAN's canonical directory layout.
use chrono::NaiveDate;
use lean_storage::{
    factor_file_path, map_file_path, path_resolver::PathResolver, FactorFileEntry, MapFileEntry,
    ParquetReader, ParquetWriter, WriterConfig,
};
use tempfile::TempDir;

// ─── helpers ────────────────────────────────────────────────────────────────

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

// ─── Factor file Parquet round-trip ─────────────────────────────────────────

#[test]
fn factor_file_parquet_round_trip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("spy.parquet");

    let entries = vec![
        FactorFileEntry {
            date: date(2024, 1, 1),
            price_factor: 1.0,
            split_factor: 1.0,
            reference_price: 0.0,
        },
        FactorFileEntry {
            date: date(2020, 8, 31),
            price_factor: 1.0,
            split_factor: 0.25,
            reference_price: 128.96,
        },
        FactorFileEntry {
            date: date(2014, 6, 9),
            price_factor: 1.0,
            split_factor: 1.0 / 28.0,
            reference_price: 92.44,
        },
    ];

    let writer = ParquetWriter::new(WriterConfig::default());
    writer.write_factor_file(&entries, &path).unwrap();

    assert!(path.exists(), "Parquet file should have been created");

    let reader = ParquetReader::new();
    let read_back = reader.read_factor_file(&path).unwrap();

    assert_eq!(read_back.len(), entries.len());
    for (orig, got) in entries.iter().zip(read_back.iter()) {
        assert_eq!(orig.date, got.date, "date mismatch");
        assert!(
            (orig.price_factor - got.price_factor).abs() < 1e-9,
            "price_factor mismatch"
        );
        assert!(
            (orig.split_factor - got.split_factor).abs() < 1e-9,
            "split_factor mismatch"
        );
        assert!(
            (orig.reference_price - got.reference_price).abs() < 1e-4,
            "reference_price mismatch"
        );
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

    let entries = vec![FactorFileEntry {
        date: date(2024, 1, 1),
        price_factor: 1.0,
        split_factor: 1.0,
        reference_price: 0.0,
    }];
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
        MapFileEntry {
            date: date(1993, 1, 29),
            ticker: "SPY".to_string(),
        },
        MapFileEntry {
            date: date(2050, 12, 31),
            ticker: "SPY".to_string(),
        },
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

    let entries = vec![MapFileEntry {
        date: date(1993, 1, 29),
        ticker: "SPY".to_string(),
    }];
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
    assert_eq!(
        p,
        PathBuf::from("/data/equity/usa/factor_files/spy.parquet")
    );
}

/// Factor file path with lowercase ticker input.
#[test]
fn factor_file_path_lowercases_ticker() {
    use std::path::PathBuf;
    let root = PathBuf::from("/data");
    let p = factor_file_path(&root, "usa", "AAPL");
    assert_eq!(
        p,
        PathBuf::from("/data/equity/usa/factor_files/aapl.parquet")
    );
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
    assert_eq!(
        resolver.factor_file("usa", "SPY"),
        factor_file_path(&root, "usa", "SPY")
    );
}

/// `PathResolver::map_file` returns the same path as the free function.
#[test]
fn path_resolver_map_file_matches_free_function() {
    use std::path::PathBuf;
    let root = PathBuf::from("/data");
    let resolver = PathResolver::new(&root);
    assert_eq!(
        resolver.map_file("usa", "SPY"),
        map_file_path(&root, "usa", "SPY")
    );
}
