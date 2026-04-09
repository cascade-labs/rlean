use crate::insight::{Insight, InsightDirection};
use lean_core::Symbol;
use lean_core::TimeSpan;
use lean_data::Slice;
use rust_decimal::Decimal;

/// Base trait for all alpha models. Mirrors C# `IAlphaModel`.
pub trait IAlphaModel: Send + Sync {
    /// Called each time new data arrives. Returns zero or more Insights.
    fn update(&mut self, slice: &Slice, securities: &[Symbol]) -> Vec<Insight>;

    /// Called when securities are added to or removed from the universe.
    #[allow(unused_variables)]
    fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {}

    /// Human-readable name for this model.
    fn name(&self) -> &str {
        "AlphaModel"
    }
}

// ---------------------------------------------------------------------------
// Composite
// ---------------------------------------------------------------------------

/// Combines multiple alpha models, concatenating their insights.
pub struct CompositeAlphaModel {
    pub models: Vec<Box<dyn IAlphaModel>>,
}

impl CompositeAlphaModel {
    pub fn new() -> Self {
        Self { models: Vec::new() }
    }

    pub fn add(mut self, model: impl IAlphaModel + 'static) -> Self {
        self.models.push(Box::new(model));
        self
    }
}

impl Default for CompositeAlphaModel {
    fn default() -> Self {
        Self::new()
    }
}

impl IAlphaModel for CompositeAlphaModel {
    fn update(&mut self, slice: &Slice, securities: &[Symbol]) -> Vec<Insight> {
        self.models
            .iter_mut()
            .flat_map(|m| m.update(slice, securities))
            .collect()
    }

    fn on_securities_changed(&mut self, added: &[Symbol], removed: &[Symbol]) {
        for m in &mut self.models {
            m.on_securities_changed(added, removed);
        }
    }

    fn name(&self) -> &str {
        "CompositeAlphaModel"
    }
}

// ---------------------------------------------------------------------------
// Null / trivial models
// ---------------------------------------------------------------------------

/// Emits no insights.
pub struct NullAlphaModel;

impl IAlphaModel for NullAlphaModel {
    fn update(&mut self, _slice: &Slice, _securities: &[Symbol]) -> Vec<Insight> {
        vec![]
    }

    fn name(&self) -> &str {
        "NullAlphaModel"
    }
}

/// Always emits the same direction insight for every security in the universe.
pub struct ConstantAlphaModel {
    pub direction: InsightDirection,
    pub period: TimeSpan,
    pub magnitude: Option<Decimal>,
}

impl IAlphaModel for ConstantAlphaModel {
    fn update(&mut self, _slice: &Slice, securities: &[Symbol]) -> Vec<Insight> {
        securities
            .iter()
            .map(|s| {
                Insight::new(
                    s.clone(),
                    self.direction,
                    self.period,
                    self.magnitude,
                    None,
                    self.name(),
                )
            })
            .collect()
    }

    fn name(&self) -> &str {
        "ConstantAlphaModel"
    }
}
