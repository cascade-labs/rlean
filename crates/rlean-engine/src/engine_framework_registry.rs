use rlean_algorithm::FrameworkModelRegistry;
use rlean_alpha::{ActiveInsightSnapshot, IAlphaModel};
use rlean_core::DateTime;
use rlean_execution::IExecutionModel;
use rlean_portfolio_construction::IPortfolioConstructionModel;
use rlean_risk::risk_management::RiskManagementModel;
use std::sync::{Arc, Mutex};

use crate::framework::{FrameworkState, InsightObserver};

fn empty_insight_snapshot() -> ActiveInsightSnapshot {
    ActiveInsightSnapshot {
        active: Arc::from([]),
        closed: Arc::from([]),
        closed_version: 0,
        total_count: 0,
    }
}

struct SnapshotInsightObserver {
    snapshot: Arc<Mutex<ActiveInsightSnapshot>>,
}

impl InsightObserver for SnapshotInsightObserver {
    fn on_insights(&self, snapshot: ActiveInsightSnapshot, _utc_now: DateTime) {
        *self.snapshot.lock().unwrap() = snapshot;
    }
}

pub struct EngineFrameworkRegistry {
    framework: Arc<Mutex<FrameworkState>>,
    snapshot: Arc<Mutex<ActiveInsightSnapshot>>,
}

impl EngineFrameworkRegistry {
    pub fn new(framework: Arc<Mutex<FrameworkState>>) -> Self {
        Self {
            framework,
            snapshot: Arc::new(Mutex::new(empty_insight_snapshot())),
        }
    }
}

impl FrameworkModelRegistry for EngineFrameworkRegistry {
    fn add_alpha_model(&self, model: Box<dyn IAlphaModel>) {
        self.ensure_insight_observer();
        self.framework.lock().unwrap().alpha_models.push(model);
    }

    fn set_portfolio_construction_model(&self, model: Box<dyn IPortfolioConstructionModel>) {
        self.ensure_insight_observer();
        self.framework.lock().unwrap().pcm = model;
    }

    fn set_execution_model(&self, model: Box<dyn IExecutionModel>) {
        self.ensure_insight_observer();
        self.framework.lock().unwrap().exec_model = model;
    }

    fn set_risk_management_model(&self, model: Box<dyn RiskManagementModel>) {
        self.ensure_insight_observer();
        self.framework.lock().unwrap().risk_model = model;
    }

    fn insight_snapshot(&self) -> Arc<Mutex<ActiveInsightSnapshot>> {
        self.snapshot.clone()
    }

    fn cancel_insights(&self, symbols: &[rlean_core::Symbol], utc_now: DateTime) {
        self.ensure_insight_observer();
        self.framework
            .lock()
            .unwrap()
            .cancel_insights(symbols, utc_now);
    }

    fn ensure_insight_observer(&self) {
        let mut framework = self.framework.lock().unwrap();
        if framework.has_observer() {
            return;
        }
        framework.set_observer(Arc::new(SnapshotInsightObserver {
            snapshot: self.snapshot.clone(),
        }));
    }
}
