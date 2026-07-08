use chrono::NaiveDate;
use lean_storage::{FactorFileEntry, IcebergStore, MapFile, MapFileEntry, MapFileResolver};
use tempfile::TempDir;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

#[tokio::test]
async fn factor_file_round_trip_through_iceberg() {
    let dir = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(dir.path()).await.unwrap();
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
    ];

    store
        .append_factor_file("usa", "SPY", &entries)
        .await
        .unwrap();
    let read_back = store.scan_factor_file("usa", "SPY").await.unwrap();

    assert_eq!(read_back.len(), entries.len());
    assert_eq!(read_back[0].date, date(2020, 8, 31));
    assert!((read_back[0].split_factor - 0.25).abs() < 1e-9);
    assert_eq!(read_back[1].date, date(2024, 1, 1));
}

#[tokio::test]
async fn map_file_round_trip_through_iceberg() {
    let dir = TempDir::new().unwrap();
    let store = IcebergStore::connect_local(dir.path()).await.unwrap();
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

    store.append_map_file("usa", "spy", &entries).await.unwrap();
    let read_back = store.scan_map_file("usa", "SPY").await.unwrap();

    assert_eq!(read_back, entries);
}

#[test]
fn map_file_resolver_resolves_ticker_to_owning_map_file_like_lean() {
    let resolver = MapFileResolver::new(vec![MapFile::new(
        "bbby",
        vec![
            MapFileEntry {
                date: date(2002, 5, 30),
                ticker: "OSTK".to_string(),
            },
            MapFileEntry {
                date: date(2025, 8, 28),
                ticker: "BYON".to_string(),
            },
            MapFileEntry {
                date: date(2050, 12, 31),
                ticker: "BBBY".to_string(),
            },
        ],
    )])
    .unwrap();

    let map_file = resolver.resolve_map_file("BYON", date(2025, 8, 28));

    assert_eq!(map_file.permtick, "BBBY");
    assert_eq!(
        map_file.mapped_ticker_at(date(2025, 8, 28), Some("BYON")),
        Some("BYON")
    );
    assert_eq!(
        map_file.mapped_ticker_at(date(2025, 8, 29), Some("BYON")),
        Some("BBBY")
    );
}

#[test]
fn map_file_resolver_uses_next_row_then_last_row_like_lean_binary_search() {
    let resolver = MapFileResolver::new(vec![MapFile::new(
        "entity",
        vec![
            MapFileEntry {
                date: date(2024, 1, 31),
                ticker: "AAA".to_string(),
            },
            MapFileEntry {
                date: date(2024, 2, 29),
                ticker: "BBB".to_string(),
            },
        ],
    )])
    .unwrap();

    assert_eq!(
        resolver.resolve_map_file("AAA", date(2024, 1, 15)).permtick,
        "ENTITY"
    );
    assert_eq!(
        resolver.resolve_map_file("BBB", date(2024, 3, 1)).permtick,
        "ENTITY"
    );
}
