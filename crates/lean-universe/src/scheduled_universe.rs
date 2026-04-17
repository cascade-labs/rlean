use chrono::NaiveDate;
use lean_core::Symbol;

/// How often the universe is re-evaluated
#[derive(Debug, Clone, Copy)]
pub enum UniverseSchedule {
    Daily,
    Weekly,
    Monthly,
    Quarterly,
}

/// Universe that is re-selected on a schedule. Mirrors C# ScheduledUniverseSelectionModel.
pub struct ScheduledUniverseSelectionModel {
    pub schedule: UniverseSchedule,
    pub selector: Box<dyn Fn(NaiveDate) -> Vec<Symbol> + Send + Sync>,
    last_selection_date: Option<NaiveDate>,
}

impl ScheduledUniverseSelectionModel {
    pub fn new(
        schedule: UniverseSchedule,
        selector: impl Fn(NaiveDate) -> Vec<Symbol> + Send + Sync + 'static,
    ) -> Self {
        Self {
            schedule,
            selector: Box::new(selector),
            last_selection_date: None,
        }
    }

    /// Returns true if the universe should be re-evaluated on the given date
    pub fn should_select(&self, date: NaiveDate) -> bool {
        use chrono::Datelike;
        match self.last_selection_date {
            None => true,
            Some(last) => match self.schedule {
                UniverseSchedule::Daily => date > last,
                UniverseSchedule::Weekly => {
                    date.iso_week().week() != last.iso_week().week() || date.year() != last.year()
                }
                UniverseSchedule::Monthly => {
                    date.month() != last.month() || date.year() != last.year()
                }
                UniverseSchedule::Quarterly => {
                    let quarter = |d: NaiveDate| (d.month() - 1) / 3;
                    quarter(date) != quarter(last) || date.year() != last.year()
                }
            },
        }
    }

    /// Run selection for the given date if schedule says so
    pub fn select(&mut self, date: NaiveDate) -> Option<Vec<Symbol>> {
        if self.should_select(date) {
            self.last_selection_date = Some(date);
            Some((self.selector)(date))
        } else {
            None
        }
    }
}
