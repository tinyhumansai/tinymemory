//! Runtime-neutral per-run limits for optional memory tool calls.
//!
//! A host supplies a monotonic clock; this module links no runtime. Each
//! tracked run can make eight calls and spend up to thirty seconds, with a
//! fifteen-second deadline on one call. Unused time is refunded when an
//! action finishes or its future is dropped. A timeout opens that run's
//! circuit. At most 128 run records are retained; an idle oldest record can
//! be evicted when a new run arrives.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{Either, select};

const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const RUN_TIME_BUDGET: Duration = Duration::from_secs(30);
const MAX_CALLS: usize = 8;
const MAX_TRACKED_RUNS: usize = 128;

/// A monotonic clock and timer supplied by the host runtime.
///
/// Both methods must use the same monotonic time base. The timer must become
/// ready at or after its deadline.
pub trait Clock: Send + Sync {
    /// The current monotonic time.
    fn now(&self) -> Duration;

    /// A future that resolves at or after deadline.
    fn sleep_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

/// Why a call did not produce an action result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The action was not polled because this run could not reserve a call.
    Denied(DenialReason),
    /// The action was polled, but its call deadline elapsed first.
    ///
    /// It may have changed memory before timing out. Hosts must report this as
    /// an ordinary tool error, not as a refusal to run.
    TimedOut,
}

/// A limit that denied a call before the action was polled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenialReason {
    /// The run already used its eight calls.
    CallLimit,
    /// The run has no latency allowance left.
    LatencyBudget,
    /// A previous call timed out and opened this run's circuit.
    CircuitOpen,
    /// The table is full and every tracked run still has an active call.
    TrackingFull,
}

#[derive(Default)]
struct Runs {
    states: HashMap<String, RunState>,
    order: VecDeque<String>,
}

struct RunState {
    calls: usize,
    remaining: Duration,
    disabled: bool,
    in_flight: usize,
}

impl Default for RunState {
    fn default() -> Self {
        Self {
            calls: 0,
            remaining: RUN_TIME_BUDGET,
            disabled: false,
            in_flight: 0,
        }
    }
}

/// Per-run reservation state for a host's memory calls.
pub struct RunBudget {
    clock: Arc<dyn Clock>,
    runs: Mutex<Runs>,
}

impl std::fmt::Debug for RunBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let runs = self
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        f.debug_struct("RunBudget")
            .field("tracked_runs", &runs.states.len())
            .finish_non_exhaustive()
    }
}

impl RunBudget {
    /// A budget with the standard call, time, and tracking limits.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            runs: Mutex::new(Runs::default()),
        }
    }

    /// Runs action under run_id's call and latency allowance.
    ///
    /// A missing run id still gets a per-call deadline, without creating a
    /// tracked record. The action's own result is left untouched inside T;
    /// a timeout is distinguishable from a denied call through Refusal.
    ///
    /// # Errors
    ///
    /// Refusal::Denied means the action was never polled. Refusal::TimedOut
    /// means it was polled and may have completed a side effect before timing
    /// out.
    pub async fn run<T, F>(&self, run_id: Option<&str>, action: F) -> Result<T, Refusal>
    where
        T: Send,
        F: Future<Output = T> + Send,
    {
        let (allowance, tracked_run) = self.reserve(run_id)?;
        let started = self.clock.now();
        let deadline = started.saturating_add(allowance);
        let mut reservation = Reservation {
            budget: self,
            run_id: tracked_run,
            started,
            allowance,
            timed_out: false,
        };

        // Poll the action first. This matches the host's Tokio timeout
        // behavior: an already-ready action wins a timer tie and a
        // non-yielding action may finish after the deadline.
        let action = std::pin::pin!(action);
        let timer = self.clock.sleep_until(deadline);
        let timer = std::pin::pin!(timer);
        match select(action, timer).await {
            Either::Left((value, _timer)) => Ok(value),
            Either::Right((_elapsed, _action)) => {
                reservation.timed_out = true;
                Err(Refusal::TimedOut)
            }
        }
    }

    fn reserve(&self, run_id: Option<&str>) -> Result<(Duration, Option<String>), Refusal> {
        let Some(run_id) = run_id else {
            return Ok((CALL_TIMEOUT, None));
        };
        let mut runs = self
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !runs.states.contains_key(run_id) {
            if runs.order.len() == MAX_TRACKED_RUNS {
                let Some(oldest_idle) = runs
                    .order
                    .iter()
                    .position(|id| runs.states[id].in_flight == 0)
                else {
                    return Err(Refusal::Denied(DenialReason::TrackingFull));
                };
                let Some(evicted) = runs.order.remove(oldest_idle) else {
                    return Err(Refusal::Denied(DenialReason::TrackingFull));
                };
                runs.states.remove(&evicted);
            }
            runs.order.push_back(run_id.to_string());
        }

        let state = runs.states.entry(run_id.to_string()).or_default();
        if state.disabled {
            return Err(Refusal::Denied(DenialReason::CircuitOpen));
        }
        if state.calls >= MAX_CALLS {
            return Err(Refusal::Denied(DenialReason::CallLimit));
        }
        if state.remaining.is_zero() {
            return Err(Refusal::Denied(DenialReason::LatencyBudget));
        }

        let allowance = state.remaining.min(CALL_TIMEOUT);
        state.calls += 1;
        state.remaining -= allowance;
        state.in_flight += 1;
        Ok((allowance, Some(run_id.to_string())))
    }
}

struct Reservation<'a> {
    budget: &'a RunBudget,
    run_id: Option<String>,
    started: Duration,
    allowance: Duration,
    timed_out: bool,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let Some(run_id) = self.run_id.as_deref() else {
            return;
        };
        let elapsed = self.budget.clock.now().saturating_sub(self.started);
        let mut runs = self
            .budget
            .runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(state) = runs.states.get_mut(run_id) {
            state.remaining += self.allowance.saturating_sub(elapsed);
            state.disabled |= self.timed_out;
            state.in_flight -= 1;
        }
    }
}
