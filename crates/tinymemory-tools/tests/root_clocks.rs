//! Public crate-root aliases for the lifecycle and tool-budget clocks.

use tinymemory_tools::{BudgetClock, Clock};

fn accepts_lifecycle_clock(_: Option<&dyn Clock>) {}
fn accepts_budget_clock(_: Option<&dyn BudgetClock>) {}

#[test]
fn root_exports_keep_lifecycle_and_budget_clocks_distinct() {
    accepts_lifecycle_clock(None);
    accepts_budget_clock(None);
}
