# Per-run tool budget

RunBudget limits optional memory work independently for each run. It allows
eight calls and thirty seconds per tracked run, with a fifteen-second deadline
for one call. Unused latency is refunded when an action completes or its
future is cancelled. A timeout opens the run's circuit. The table tracks at
most 128 runs and evicts the oldest record with no call in flight.

The host implements `tools::budget::Clock` with its runtime's monotonic time
and timer future. At the crate root, this trait is re-exported as
`BudgetClock` to distinguish it from `lifecycle::turn::Clock`. This module
does not link Tokio or any tool-runtime result type.

Refusal::Denied means the action was never polled and can be exposed to the
host failure ladder as a refusal. Refusal::TimedOut means the action ran and
may already have changed memory; the host must report an ordinary error and
must not attach its pre-execution refusal metadata.
