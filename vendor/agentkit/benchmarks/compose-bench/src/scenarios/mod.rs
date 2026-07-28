mod calendar_scheduling;
mod config_migration;
mod crm_hygiene;
mod log_incident;
mod revenue_report;
mod support_triage;

use crate::scenario::Scenario;

/// Every benchmark scenario, in report order.
pub fn all() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(support_triage::SupportTriage),
        Box::new(revenue_report::RevenueReport),
        Box::new(log_incident::LogIncident),
        Box::new(crm_hygiene::CrmHygiene),
        Box::new(calendar_scheduling::CalendarScheduling),
        Box::new(config_migration::ConfigMigration),
    ]
}
