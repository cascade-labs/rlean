use anyhow::{Result, anyhow};
use iceberg::scan::FileScanTask;
use iceberg::spec::{DataFile, Literal, PartitionSpec, PrimitiveLiteral, Struct};
use lean_core::{Resolution, SecurityType};
use std::collections::{BTreeSet, HashMap};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MarketPartitionKey {
    pub security_type: String,
    pub market: String,
    pub resolution: String,
    pub symbol_sid: u64,
    pub day: i32,
}

impl MarketPartitionKey {
    pub fn new(
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        symbol_sid: u64,
        day: i32,
    ) -> Self {
        Self {
            security_type: security_type.to_string().to_lowercase(),
            market: market.to_lowercase(),
            resolution: resolution.folder_name().to_string(),
            symbol_sid,
            day,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MarketPartitionPrefix {
    security_type: String,
    market: String,
    resolution: String,
    symbol_sid: u64,
}

impl From<&MarketPartitionKey> for MarketPartitionPrefix {
    fn from(key: &MarketPartitionKey) -> Self {
        Self {
            security_type: key.security_type.clone(),
            market: key.market.clone(),
            resolution: key.resolution.clone(),
            symbol_sid: key.symbol_sid,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MarketPartitionIndex {
    pub snapshot_id: Option<i64>,
    days_by_prefix: HashMap<MarketPartitionPrefix, BTreeSet<i32>>,
    file_paths_by_key: HashMap<MarketPartitionKey, BTreeSet<String>>,
}

impl MarketPartitionIndex {
    pub fn new(snapshot_id: Option<i64>) -> Self {
        Self {
            snapshot_id,
            days_by_prefix: HashMap::new(),
            file_paths_by_key: HashMap::new(),
        }
    }

    pub fn insert(&mut self, key: MarketPartitionKey) {
        self.days_by_prefix
            .entry((&key).into())
            .or_default()
            .insert(key.day);
    }

    pub fn insert_file_path(&mut self, key: MarketPartitionKey, file_path: impl Into<String>) {
        self.insert(key.clone());
        self.file_paths_by_key
            .entry(key)
            .or_default()
            .insert(file_path.into());
    }

    pub fn insert_data_file(&mut self, data_file: &DataFile, spec: &PartitionSpec) -> Result<()> {
        if data_file.record_count() == 0 {
            return Ok(());
        }
        let fields = MarketPartitionFields::from_spec(spec)?;
        if let Some(key) = market_partition_key(data_file.partition(), &fields)? {
            self.insert_file_path(key, data_file.file_path());
        }
        Ok(())
    }

    pub fn insert_file_scan_task(
        &mut self,
        task: &FileScanTask,
        default_spec: &PartitionSpec,
    ) -> Result<()> {
        if task.record_count == Some(0) {
            return Ok(());
        }
        let Some(partition) = task.partition.as_ref() else {
            return Ok(());
        };
        let spec = task.partition_spec.as_deref().unwrap_or(default_spec);
        let fields = MarketPartitionFields::from_spec(spec)?;
        if let Some(key) = market_partition_key(partition, &fields)? {
            self.insert_file_path(key, task.data_file_path());
        }
        Ok(())
    }

    pub fn insert_file_scan_task_with_fields(
        &mut self,
        task: &FileScanTask,
        default_fields: &MarketPartitionFields,
    ) -> Result<()> {
        if task.record_count == Some(0) {
            return Ok(());
        }
        let Some(partition) = task.partition.as_ref() else {
            return Ok(());
        };
        if task.partition_spec.is_some() {
            let spec = task.partition_spec.as_deref().expect("checked partition spec");
            let fields = MarketPartitionFields::from_spec(spec)?;
            if let Some(key) = market_partition_key(partition, &fields)? {
                self.insert_file_path(key, task.data_file_path());
            }
        } else if let Some(key) = market_partition_key(partition, default_fields)? {
            self.insert_file_path(key, task.data_file_path());
        }
        Ok(())
    }

    pub fn days_for(
        &self,
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        symbol_sid: u64,
        day_start: i32,
        day_end: i32,
    ) -> BTreeSet<i32> {
        let exact = MarketPartitionPrefix {
            security_type: security_type.to_string().to_lowercase(),
            market: market.to_lowercase(),
            resolution: resolution.folder_name().to_string(),
            symbol_sid,
        };
        let mut days: BTreeSet<i32> = self
            .days_by_prefix
            .get(&exact)
            .map(|days| days.range(day_start..=day_end).copied().collect())
            .unwrap_or_default();
        if symbol_sid != 0 {
            let legacy = MarketPartitionPrefix {
                symbol_sid: 0,
                ..exact
            };
            if let Some(legacy_days) = self.days_by_prefix.get(&legacy) {
                days.extend(legacy_days.range(day_start..=day_end).copied());
            }
        }
        days
    }

    pub fn file_paths_for(
        &self,
        security_type: SecurityType,
        market: &str,
        resolution: Resolution,
        symbol_sid: u64,
        day_start: i32,
        day_end: i32,
    ) -> BTreeSet<String> {
        let mut paths = BTreeSet::new();
        for day in self.days_for(
            security_type,
            market,
            resolution,
            symbol_sid,
            day_start,
            day_end,
        ) {
            let key = MarketPartitionKey::new(security_type, market, resolution, symbol_sid, day);
            if let Some(day_paths) = self.file_paths_by_key.get(&key) {
                paths.extend(day_paths.iter().cloned());
            }
            if symbol_sid != 0 {
                let legacy_key = MarketPartitionKey::new(security_type, market, resolution, 0, day);
                if let Some(day_paths) = self.file_paths_by_key.get(&legacy_key) {
                    paths.extend(day_paths.iter().cloned());
                }
            }
        }
        paths
    }
}

#[derive(Debug, Clone)]
pub struct MarketPartitionFields {
    len: usize,
    security_type: usize,
    market: usize,
    resolution: usize,
    symbol_sid: Option<usize>,
    day: usize,
}

impl MarketPartitionFields {
    pub fn from_spec(spec: &PartitionSpec) -> Result<Self> {
        Ok(Self {
            len: spec.fields().len(),
            security_type: field_index(spec, "security_type")?,
            market: field_index(spec, "market")?,
            resolution: field_index(spec, "resolution")?,
            symbol_sid: spec
                .fields()
                .iter()
                .position(|field| field.name == "symbol_sid"),
            day: field_index(spec, "day")?,
        })
    }
}

pub fn market_partition_key(
    partition: &Struct,
    field_indexes: &MarketPartitionFields,
) -> Result<Option<MarketPartitionKey>> {
    let fields = partition.fields();
    if fields.len() != field_indexes.len {
        return Ok(None);
    }
    let security_type = string_field(fields, field_indexes.security_type)?;
    let market = string_field(fields, field_indexes.market)?;
    let resolution = string_field(fields, field_indexes.resolution)?;
    let symbol_sid = match field_indexes.symbol_sid {
        Some(index) => u64_field(fields, index)?,
        None => 0,
    };
    let day = i32_field(fields, field_indexes.day)?;
    Ok(Some(MarketPartitionKey {
        security_type: security_type.to_lowercase(),
        market: market.to_lowercase(),
        resolution,
        symbol_sid,
        day,
    }))
}

fn field_index(spec: &PartitionSpec, name: &str) -> Result<usize> {
    spec.fields()
        .iter()
        .position(|field| field.name == name)
        .ok_or_else(|| anyhow!("market partition field {name} missing"))
}

fn string_field(fields: &[Option<Literal>], index: usize) -> Result<String> {
    match fields.get(index).and_then(Option::as_ref) {
        Some(Literal::Primitive(PrimitiveLiteral::String(value))) => Ok(value.clone()),
        Some(value) => Err(anyhow!("expected string partition field at {index}, got {value:?}")),
        None => Err(anyhow!("missing string partition field at {index}")),
    }
}

fn u64_field(fields: &[Option<Literal>], index: usize) -> Result<u64> {
    match fields.get(index).and_then(Option::as_ref) {
        Some(Literal::Primitive(PrimitiveLiteral::Long(value))) => Ok(*value as u64),
        Some(Literal::Primitive(PrimitiveLiteral::Int(value))) => Ok(*value as u64),
        Some(value) => Err(anyhow!("expected integer partition field at {index}, got {value:?}")),
        None => Err(anyhow!("missing integer partition field at {index}")),
    }
}

fn i32_field(fields: &[Option<Literal>], index: usize) -> Result<i32> {
    match fields.get(index).and_then(Option::as_ref) {
        Some(Literal::Primitive(PrimitiveLiteral::Int(value))) => Ok(*value),
        Some(Literal::Primitive(PrimitiveLiteral::Long(value))) => Ok((*value).try_into()?),
        Some(value) => Err(anyhow!("expected date partition field at {index}, got {value:?}")),
        None => Err(anyhow!("missing date partition field at {index}")),
    }
}
