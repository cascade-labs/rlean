use lean_algorithm::algorithm::IAlgorithm;
use lean_core::{DateTime, Result as LeanResult};
use lean_data::Slice;
use lean_orders::OrderEvent;
use tracing::{error, info};

pub struct AlgorithmManager {
    pub algorithm: Box<dyn IAlgorithm>,
}

impl AlgorithmManager {
    pub fn new(algorithm: Box<dyn IAlgorithm>) -> Self {
        AlgorithmManager { algorithm }
    }

    pub fn initialize(&mut self) -> LeanResult<()> {
        info!("Initializing algorithm: {}", self.algorithm.name());
        self.algorithm.initialize()
    }

    pub fn on_data(&mut self, slice: &Slice) {
        self.algorithm.on_data(slice);
    }

    pub fn on_order_event(&mut self, event: &OrderEvent) {
        self.algorithm.on_order_event(event);
    }

    pub fn on_end_of_day(&mut self, symbol: Option<lean_core::Symbol>) {
        self.algorithm.on_end_of_day(symbol);
    }

    pub fn on_end_of_algorithm(&mut self) {
        self.algorithm.on_end_of_algorithm();
    }
}
