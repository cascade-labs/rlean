use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use chrono::NaiveDate;
use lean_core::{LeanError, Result};

use crate::{MapFileEntry, ParquetReader};

#[derive(Debug, Clone, PartialEq)]
pub struct MapFile {
    pub permtick: String,
    pub rows: Vec<MapFileEntry>,
    pub first_date: Option<NaiveDate>,
    pub delisting_date: Option<NaiveDate>,
    pub first_ticker: String,
}

impl MapFile {
    pub fn new(permtick: impl Into<String>, mut rows: Vec<MapFileEntry>) -> Self {
        let permtick = permtick.into().to_uppercase();
        rows.sort_by_key(|row| row.date);
        rows.dedup_by(|a, b| a.date == b.date && a.ticker == b.ticker);

        let first_date = rows.first().map(|row| row.date);
        let delisting_date = rows.last().map(|row| row.date);
        let first_ticker = first_date
            .and_then(|date| mapped_ticker_at(&rows, date, Some(&permtick)))
            .map(str::to_string)
            .unwrap_or_else(|| permtick.clone());

        MapFile {
            permtick,
            rows,
            first_date,
            delisting_date,
            first_ticker,
        }
    }

    pub fn empty(permtick: impl Into<String>) -> Self {
        Self::new(permtick, Vec::new())
    }

    pub fn has_data(&self, date: NaiveDate) -> bool {
        match (self.first_date, self.delisting_date) {
            (Some(first), Some(last)) => date >= first && date <= last,
            _ => true,
        }
    }

    pub fn mapped_ticker_at<'a>(
        &'a self,
        date: NaiveDate,
        default_ticker: Option<&'a str>,
    ) -> Option<&'a str> {
        mapped_ticker_at(&self.rows, date, default_ticker)
    }
}

#[derive(Debug, Clone)]
struct MapFileRowEntry {
    entity_symbol: String,
    row: MapFileEntry,
}

/// Resolves the map file responsible for a mapped ticker at a point in time.
///
/// This mirrors C# LEAN's `MapFileResolver`: all map files are indexed by
/// permtick and by every mapped-symbol row, and `resolve_map_file` first maps
/// the input ticker/date to the owning permtick before returning that file.
#[derive(Debug, Clone, Default)]
pub struct MapFileResolver {
    by_permtick: HashMap<String, MapFile>,
    by_symbol: HashMap<String, BTreeMap<NaiveDate, MapFileRowEntry>>,
}

impl MapFileResolver {
    pub fn new(map_files: impl IntoIterator<Item = MapFile>) -> Result<Self> {
        let mut by_permtick = HashMap::new();
        let mut by_symbol: HashMap<String, BTreeMap<NaiveDate, MapFileRowEntry>> = HashMap::new();

        for map_file in map_files {
            let permtick = map_file.permtick.clone();
            for row in &map_file.rows {
                let symbol_entries = by_symbol.entry(row.ticker.clone()).or_default();
                if let Some(existing) = symbol_entries.get(&row.date) {
                    if existing.row.ticker != row.ticker {
                        return Err(LeanError::DataError(format!(
                            "attempted to assign different map history for {} on {}",
                            row.ticker, row.date
                        )));
                    }
                } else {
                    symbol_entries.insert(
                        row.date,
                        MapFileRowEntry {
                            entity_symbol: permtick.clone(),
                            row: row.clone(),
                        },
                    );
                }
            }
            by_permtick.insert(permtick, map_file);
        }

        Ok(Self {
            by_permtick,
            by_symbol,
        })
    }

    pub fn from_directory(reader: &ParquetReader, map_file_directory: &Path) -> Result<Self> {
        if !map_file_directory.exists() {
            return Ok(Self::default());
        }

        let mut map_files = Vec::new();
        for entry in std::fs::read_dir(map_file_directory)
            .map_err(|e| LeanError::DataError(format!("{}: {e}", map_file_directory.display())))?
        {
            let entry = entry.map_err(|e| LeanError::DataError(e.to_string()))?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("parquet") {
                continue;
            }
            let Some(permtick) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let rows = reader.read_map_file(&path)?;
            map_files.push(MapFile::new(permtick, rows));
        }

        Self::new(map_files)
    }

    pub fn get_by_permtick(&self, permtick: &str) -> Option<&MapFile> {
        self.by_permtick.get(&permtick.to_uppercase())
    }

    pub fn resolve_map_file(&self, symbol: &str, date: NaiveDate) -> MapFile {
        let mut permtick = symbol.to_uppercase();
        if let Some(entries) = self.by_symbol.get(&permtick) {
            if entries.is_empty() {
                return MapFile::empty(&permtick);
            }

            if let Some((_, entry)) = entries.range(date..).next() {
                permtick = entry.entity_symbol.clone();
            } else if let Some((_, entry)) = entries.iter().next_back() {
                permtick = entry.entity_symbol.clone();
            }
        }

        let Some(map_file) = self.by_permtick.get(&permtick) else {
            return MapFile::empty(permtick);
        };
        if map_file
            .first_date
            .is_some_and(|first_date| first_date > date)
        {
            return MapFile::empty(permtick);
        }
        map_file.clone()
    }
}

fn mapped_ticker_at<'a>(
    rows: &'a [MapFileEntry],
    date: NaiveDate,
    default_ticker: Option<&'a str>,
) -> Option<&'a str> {
    rows.iter()
        .filter(|row| row.date >= date)
        .min_by_key(|row| row.date)
        .map(|row| row.ticker.as_str())
        .or(default_ticker)
}
