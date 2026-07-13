use crate::filter_universe::OptionFilterUniverse;
pub use rlean_data::OptionChain;

pub trait OptionChainExt {
    fn filter_universe(&self) -> OptionFilterUniverse;
}

impl OptionChainExt for OptionChain {
    fn filter_universe(&self) -> OptionFilterUniverse {
        OptionFilterUniverse::new(
            self.contracts.values().cloned().collect(),
            self.underlying_price,
        )
    }
}
