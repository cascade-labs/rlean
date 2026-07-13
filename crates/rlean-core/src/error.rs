use thiserror::Error;

#[derive(Debug, Error)]
pub enum LeanError {
    #[error("Symbol not found: {0}")]
    SymbolNotFound(String),

    #[error("Invalid resolution: {0}")]
    InvalidResolution(String),

    #[error("Invalid security type: {0}")]
    InvalidSecurityType(String),

    #[error("Invalid market: {0}")]
    InvalidMarket(String),

    #[error("Data error: {0}")]
    DataError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Insufficient data: needed {needed}, available {available}")]
    InsufficientData { needed: usize, available: usize },

    #[error("Order error: {0}")]
    OrderError(String),

    #[error("Brokerage error: {0}")]
    BrokerageError(String),

    #[error("Algorithm error: {0}")]
    AlgorithmError(String),

    #[error("Configuration error: {0}")]
    ConfigError(String),

    #[error("Unsupported operation: {0}")]
    Unsupported(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, LeanError>;
