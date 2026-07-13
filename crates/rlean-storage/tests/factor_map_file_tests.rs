use chrono::NaiveDate;
use rlean_storage::iceberg_store::{FACTOR_FILES, MAP_FILES};
use rlean_storage::{FactorFileEntry, IcebergStore, MapFile, MapFileEntry, MapFileResolver};
use rlean_storage::{RestCatalogConfig, SigV4Config};

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

#[tokio::test]
#[ignore = "needs a live REST Iceberg catalog; set RLEAN_TEST_CATALOG"]
async fn factor_file_round_trip_through_iceberg() {
    let Some(store) = connect_test_store().await else {
        return;
    };
    // The scratch namespace persists across runs, so start from an empty table.
    store.reset_table(FACTOR_FILES).await.unwrap();
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
#[ignore = "needs a live REST Iceberg catalog; set RLEAN_TEST_CATALOG"]
async fn map_file_round_trip_through_iceberg() {
    let Some(store) = connect_test_store().await else {
        return;
    };
    // The scratch namespace persists across runs, so start from an empty table.
    store.reset_table(MAP_FILES).await.unwrap();
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

/// Connect to a REST Iceberg catalog for integration testing.
///
/// These tests need a live REST catalog (e.g. AWS S3 Tables): they are marked
/// `#[ignore]` and only run when opted in. Set `RLEAN_TEST_CATALOG` to the
/// catalog base URI to enable them; `RLEAN_TEST_WAREHOUSE` supplies the
/// warehouse identifier, and `RLEAN_TEST_SIGV4_REGION` (+ optional
/// `RLEAN_TEST_SIGV4_NAME`, default `s3tables`) turns on SigV4 signing.
/// `RLEAN_TEST_NAMESPACE` selects the Iceberg namespace (default `lean_dev`, an
/// isolated scratch namespace that never touches the production `lean` tables).
/// When `RLEAN_TEST_CATALOG` is unset the helper returns `None` and the test skips.
///
/// The scratch namespace persists across runs, so each gated test that asserts
/// on a table's full contents resets that table (drop + recreate) after
/// connecting to start from a known-empty state.
async fn connect_test_store() -> Option<IcebergStore> {
    let uri = std::env::var("RLEAN_TEST_CATALOG")
        .ok()
        .filter(|v| !v.is_empty())?;
    let warehouse = std::env::var("RLEAN_TEST_WAREHOUSE")
        .expect("RLEAN_TEST_WAREHOUSE must be set when RLEAN_TEST_CATALOG is");
    let sigv4 = std::env::var("RLEAN_TEST_SIGV4_REGION")
        .ok()
        .filter(|region| !region.is_empty())
        .map(|region| SigV4Config {
            region,
            signing_name: std::env::var("RLEAN_TEST_SIGV4_NAME")
                .ok()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "s3tables".to_string()),
        });
    // Default to an isolated scratch namespace so gated tests never write into
    // the production `lean` tables; override with RLEAN_TEST_NAMESPACE.
    let namespace = std::env::var("RLEAN_TEST_NAMESPACE")
        .ok()
        .filter(|ns| !ns.is_empty())
        .unwrap_or_else(|| "lean_dev".to_string());
    Some(
        IcebergStore::connect(RestCatalogConfig {
            uri,
            warehouse,
            sigv4,
            namespace,
            data_refresh_secs: 0,
        })
        .await
        .expect("failed to connect to the test REST catalog"),
    )
}
