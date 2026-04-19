use crate::symbol_changed::SymbolChangedEvent;
use crate::{Delisting, Dividend, Split};
use serde::{Deserialize, Serialize};

/// Enum wrapping all corporate action / auxiliary event types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AuxiliaryData {
    Dividend(Dividend),
    Split(Split),
    Delisting(Delisting),
    SymbolChanged(SymbolChangedEvent),
}

impl AuxiliaryData {
    pub fn time(&self) -> lean_core::DateTime {
        match self {
            AuxiliaryData::Dividend(d) => d.time,
            AuxiliaryData::Split(s) => s.time,
            AuxiliaryData::Delisting(d) => d.time,
            AuxiliaryData::SymbolChanged(s) => s.time,
        }
    }
}
