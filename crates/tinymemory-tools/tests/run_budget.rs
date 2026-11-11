//! The runtime-neutral per-run budget: call and latency caps, refund,
//! cancellation, timeout circuit, and bounded run tracking.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use tinymemory_tools::tools::budget::{Clock, Refusal, RunBudget};
use tokio::sync::{oneshot, watch};

fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.as_mut().poll(&mut context)
}

#[derive(Clone)]
struct ManualClock {
    millis: Arc<AtomicU64>,
    changed: watch::Sender<u64>,
}

impl ManualClock {
    fn new() -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            millis: Arc::new(AtomicU64::new(0)),
            changed,
        }
    }

    fn advance(&self, amount: Duration) {
        let millis = amount.as_millis() as u64;
        let now = self.millis.fetch_add(millis, Ordering::SeqCst) + millis;
        self.changed.send_replace(now);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.millis.load(Ordering::SeqCst))
    }

    fn sleep_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        let mut changed = self.changed.subscribe();
        Box::pin(async move {
            while Duration::from_millis(*changed.borrow()) < deadline {
                if changed.changed().await.is_err() {
                    return;
                }
            }
        })
    }
}

fn budget(clock: &ManualClock) -> Arc<RunBudget> {
    Arc::new(RunBudget::new(Arc::new(clock.clone())))
}

#[tokio::test]
async fn the_ninth_run_call_is_denied_without_polling_the_action() {
    let clock = ManualClock::new();
    let budget = budget(&clock);
    for _ in 0..8 {
        budget
            .run(Some("run"), async {})
            .await
            .expect("call within the run cap");
    }

    let polled = Arc::new(AtomicBool::new(false));
    let action_polled = polled.clone();
    let result = budget
        .run(Some("run"), async move {
            action_polled.store(true, Ordering::SeqCst);
        })
        .await;
    assert!(matches!(result, Err(Refusal::Denied(_))));
    assert!(
        !polled.load(Ordering::SeqCst),
        "a denied action is never polled"
    );
}

#[tokio::test]
async fn completed_calls_refund_unused_latency_and_share_the_run_deadline() {
    let clock = ManualClock::new();
    let budget = budget(&clock);
    for _ in 0..2 {
        let clock = clock.clone();
        budget
            .run(Some("run"), async move {
                clock.advance(Duration::from_secs(12));
            })
            .await
            .expect("each call uses less than its reservation");
    }
    assert_eq!(clock.now(), Duration::from_secs(24));

    let clock_after_refunds = clock.clone();
    budget
        .run(Some("run"), async move {
            clock_after_refunds.advance(Duration::from_secs(6));
        })
        .await
        .expect("the remaining aggregate latency is available");
    let polled = Arc::new(AtomicBool::new(false));
    let action_polled = polled.clone();
    let result = budget
        .run(Some("run"), async move {
            action_polled.store(true, Ordering::SeqCst);
        })
        .await;
    assert!(matches!(result, Err(Refusal::Denied(_))));
    assert!(!polled.load(Ordering::SeqCst));
}

#[tokio::test]
async fn timeout_is_an_attempted_action_and_opens_only_that_runs_circuit() {
    let clock = ManualClock::new();
    let budget = budget(&clock);
    let (started, observed) = oneshot::channel();
    let call_budget = budget.clone();
    let call = tokio::spawn(async move {
        call_budget
            .run(Some("timed-out"), async move {
                let _ = started.send(());
                futures::future::pending::<()>().await;
            })
            .await
    });
    observed.await.expect("action was polled");
    clock.advance(Duration::from_secs(15));
    assert!(matches!(
        call.await.expect("budget task"),
        Err(Refusal::TimedOut)
    ));

    let polled = Arc::new(AtomicBool::new(false));
    let action_polled = polled.clone();
    assert!(matches!(
        budget
            .run(Some("timed-out"), async move {
                action_polled.store(true, Ordering::SeqCst);
            })
            .await,
        Err(Refusal::Denied(_))
    ));
    assert!(!polled.load(Ordering::SeqCst));
    budget
        .run(Some("next-run"), async {})
        .await
        .expect("another run gets a fresh circuit");
}

#[tokio::test]
async fn a_ready_action_can_finish_while_crossing_the_deadline_without_yielding() {
    let clock = ManualClock::new();
    let budget = budget(&clock);
    let action_clock = clock.clone();
    let result = budget
        .run(Some("tie"), async move {
            action_clock.advance(Duration::from_secs(15));
        })
        .await;
    result.expect("a ready action can complete while crossing the deadline");
}

#[tokio::test]
async fn an_already_pending_action_wins_a_simultaneously_ready_timer() {
    let clock = ManualClock::new();
    let budget = budget(&clock);
    let (started, observed) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    let mut call = Box::pin(budget.run(Some("tie"), async move {
        let _ = started.send(());
        let _ = finished.await;
    }));
    assert!(poll_once(call.as_mut()).is_pending());
    observed.await.expect("action was polled");
    // Match the host's Tokio timeout: poll the action before the timer, so a
    // pending action that becomes ready on the deadline completes successfully.
    clock.advance(Duration::from_secs(15));
    finish.send(()).expect("action completion queued");
    assert!(matches!(poll_once(call.as_mut()), Poll::Ready(Ok(()))));
}

#[tokio::test]
async fn cancelling_an_action_refunds_its_unspent_reservation() {
    let clock = ManualClock::new();
    let budget = budget(&clock);
    let (started, observed) = oneshot::channel();
    let mut call = Box::pin(budget.run(Some("run"), async move {
        let _ = started.send(());
        futures::future::pending::<()>().await
    }));
    assert!(poll_once(call.as_mut()).is_pending());
    observed.await.expect("action was polled");
    drop(call);

    let clock_after_reservation = clock.clone();
    budget
        .run(Some("run"), async move {
            clock_after_reservation.advance(Duration::from_secs(12));
        })
        .await
        .expect("the cancelled call returned its reservation");

    let (started, observed) = oneshot::channel();
    let (finish, finished) = oneshot::channel();
    let mut call = Box::pin(budget.run(Some("run"), async move {
        let _ = started.send(());
        let _ = finished.await;
    }));
    assert!(poll_once(call.as_mut()).is_pending());
    observed.await.expect("follow-up action was polled");
    clock.advance(Duration::from_secs(4));
    assert!(
        poll_once(call.as_mut()).is_pending(),
        "four seconds is below the refunded fifteen-second allowance"
    );
    finish.send(()).expect("follow-up action completion queued");
    call.await.expect("cancelled allowance was refunded");
}

#[tokio::test]
async fn concurrent_calls_reserve_from_the_same_run_budget() {
    let clock = ManualClock::new();
    let budget = budget(&clock);
    let (first_started, first_observed) = oneshot::channel();
    let first_budget = budget.clone();
    let first = tokio::spawn(async move {
        first_budget
            .run(Some("parallel"), async move {
                let _ = first_started.send(());
                futures::future::pending::<()>().await;
            })
            .await
    });
    first_observed.await.expect("first reservation started");

    let (second_started, second_observed) = oneshot::channel();
    let second_budget = budget.clone();
    let second = tokio::spawn(async move {
        second_budget
            .run(Some("parallel"), async move {
                let _ = second_started.send(());
                futures::future::pending::<()>().await;
            })
            .await
    });
    second_observed.await.expect("second reservation started");

    let polled = Arc::new(AtomicBool::new(false));
    let action_polled = polled.clone();
    assert!(matches!(
        budget
            .run(Some("parallel"), async move {
                action_polled.store(true, Ordering::SeqCst);
            })
            .await,
        Err(Refusal::Denied(_))
    ));
    assert!(!polled.load(Ordering::SeqCst));

    first.abort();
    let _ = first.await;
    let (third_started, third_observed) = oneshot::channel();
    let third_budget = budget.clone();
    let third = tokio::spawn(async move {
        third_budget
            .run(Some("parallel"), async move {
                let _ = third_started.send(());
                futures::future::pending::<()>().await;
            })
            .await
    });
    third_observed
        .await
        .expect("cancelled reservation was refunded");
    second.abort();
    third.abort();
    let _ = second.await;
    let _ = third.await;
}

#[tokio::test]
async fn unscoped_calls_are_timed_but_do_not_leave_a_circuit_record() {
    let clock = ManualClock::new();
    let budget = budget(&clock);
    let (started, observed) = oneshot::channel();
    let call_budget = budget.clone();
    let call = tokio::spawn(async move {
        call_budget
            .run(None, async move {
                let _ = started.send(());
                futures::future::pending::<()>().await;
            })
            .await
    });
    observed.await.expect("action was polled");
    clock.advance(Duration::from_secs(15));
    assert!(matches!(
        call.await.expect("budget task"),
        Err(Refusal::TimedOut)
    ));
    budget
        .run(None, async {})
        .await
        .expect("an unscoped timeout does not disable later calls");
}

#[tokio::test]
async fn the_run_table_evicts_idle_entries_but_refuses_when_all_128_are_in_flight() {
    let idle_clock = ManualClock::new();
    let idle_budget = budget(&idle_clock);
    for index in 0..128 {
        for _ in 0..8 {
            idle_budget
                .run(Some(&format!("idle-{index}")), async {})
                .await
                .expect("fill an idle run's call budget");
        }
    }
    idle_budget
        .run(Some("newest-run"), async {})
        .await
        .expect("the 129th id evicts the oldest idle record");
    idle_budget
        .run(Some("idle-0"), async {})
        .await
        .expect("the evicted oldest run starts with a fresh budget");
    let recent_polled = Arc::new(AtomicBool::new(false));
    let action_polled = recent_polled.clone();
    assert!(matches!(
        idle_budget
            .run(Some("idle-127"), async move {
                action_polled.store(true, Ordering::SeqCst);
            })
            .await,
        Err(Refusal::Denied(_))
    ));
    assert!(!recent_polled.load(Ordering::SeqCst));

    let clock = ManualClock::new();
    let budget = budget(&clock);
    let mut calls = Vec::new();
    for index in 0..128 {
        let call_budget = budget.clone();
        let (started, observed) = oneshot::channel();
        calls.push(tokio::spawn(async move {
            call_budget
                .run(Some(&format!("active-{index}")), async move {
                    let _ = started.send(());
                    futures::future::pending::<()>().await;
                })
                .await
        }));
        observed
            .await
            .expect("active reservation reached its action");
    }

    let polled = Arc::new(AtomicBool::new(false));
    let action_polled = polled.clone();
    assert!(matches!(
        budget
            .run(Some("active-128"), async move {
                action_polled.store(true, Ordering::SeqCst);
            })
            .await,
        Err(Refusal::Denied(_))
    ));
    assert!(!polled.load(Ordering::SeqCst));
    for call in calls {
        call.abort();
        let _ = call.await;
    }
}
