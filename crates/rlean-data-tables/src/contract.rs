use std::sync::Arc;

use arrow_schema::{DataType, Schema};

pub const DECIMAL_PRECISION: u8 = 38;
pub const DECIMAL_SCALE: i8 = 18;

pub fn decimal_type() -> DataType {
    DataType::Decimal128(DECIMAL_PRECISION, DECIMAL_SCALE)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionTransform {
    Identity,
    Month,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartitionField {
    pub source: &'static str,
    pub transform: PartitionTransform,
}

impl PartitionField {
    pub const fn identity(source: &'static str) -> Self {
        Self {
            source,
            transform: PartitionTransform::Identity,
        }
    }

    pub const fn month(source: &'static str) -> Self {
        Self {
            source,
            transform: PartitionTransform::Month,
        }
    }
}

/// Catalog-independent definition owned by a persisted Rust row type.
pub struct TableDefinition {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Arc<Schema>,
    pub field_descriptions: &'static [FieldDescription],
    pub partition_fields: &'static [PartitionField],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldDescription {
    pub name: &'static str,
    pub description: &'static str,
}

impl FieldDescription {
    pub const fn new(name: &'static str, description: &'static str) -> Self {
        Self { name, description }
    }
}

pub trait TableContract {
    const TABLE_NAME: &'static str;
    const PARTITION_FIELDS: &'static [PartitionField];
    const DESCRIPTION: &'static str = "";
    const FIELD_DESCRIPTIONS: &'static [FieldDescription] = &[];

    fn schema() -> Arc<Schema>;

    fn table_definition() -> TableDefinition {
        TableDefinition {
            name: Self::TABLE_NAME,
            description: Self::DESCRIPTION,
            schema: Self::schema(),
            field_descriptions: Self::FIELD_DESCRIPTIONS,
            partition_fields: Self::PARTITION_FIELDS,
        }
    }
}
