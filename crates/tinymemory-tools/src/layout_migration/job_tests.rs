use std::sync::atomic::Ordering;

use tinymemory_api::{ListRequest, MemoryEngine, MetaFilter};

use super::super::service::scan;
use super::super::test_host::{FakeClaim, FakeHost, count, fact};
use super::*;

async fn go(host: &FakeHost, trigger: Trigger) -> Outcome {
    run(host, trigger, || async { false }).await.unwrap()
}

#[tokio::test]
async fn no_legacy_memory_switches_at_once_and_nothing_else() {
    let host = FakeHost::with(0).await;
    assert_eq!(go(&host, Trigger::Auto).await, Outcome::NothingToMove);
    assert!(host.is_switched());
    assert_eq!(go(&host, Trigger::Auto).await, Outcome::Done, "settled");
}

#[tokio::test]
async fn an_automatic_run_waits_while_moving_is_not_free() {
    let host = FakeHost::with(3).await;
    host.free.store(false, Ordering::SeqCst);
    assert_eq!(go(&host, Trigger::Auto).await, Outcome::NotFree);
    assert!(!host.is_switched());
    assert_eq!(count(host.tree.as_ref()).await, 0);
    assert_eq!(
        go(&host, Trigger::Manual { takeover: false }).await,
        Outcome::Done,
        "the user's own start does not wait for a free period"
    );
}

#[tokio::test]
async fn a_shared_legacy_tree_moves_only_with_consent() {
    let mut host = FakeHost::with(3).await;
    host.claim = Some(FakeClaim::default());
    assert_eq!(go(&host, Trigger::Auto).await, Outcome::NeedsTakeover);
    assert_eq!(
        go(&host, Trigger::Manual { takeover: false }).await,
        Outcome::NeedsTakeover
    );
    assert_eq!(
        go(&host, Trigger::Manual { takeover: true }).await,
        Outcome::Done
    );
    assert_eq!(
        go(&host, Trigger::Auto).await,
        Outcome::Done,
        "consent is kept"
    );
}

#[tokio::test]
async fn a_free_run_moves_switches_and_cleans_up() {
    let host = FakeHost::with(120).await;
    assert_eq!(go(&host, Trigger::Auto).await, Outcome::Done);
    assert!(host.is_switched());
    assert_eq!(count(host.tree.as_ref()).await, 120);
    assert_eq!(count(host.legacy.as_ref()).await, 0);
}

#[tokio::test]
async fn a_free_period_ending_mid_run_pauses_before_the_switch_and_resumes() {
    let host = FakeHost::with(120).await;
    // Free at the start and for the first page, then not.
    host.free_for.store(2, Ordering::SeqCst);
    assert_eq!(go(&host, Trigger::Auto).await, Outcome::Paused);
    assert!(!host.is_switched(), "reads stay on the full legacy tree");
    assert!(count(host.tree.as_ref()).await < 120);

    host.free_for.store(usize::MAX, Ordering::SeqCst);
    assert_eq!(go(&host, Trigger::Auto).await, Outcome::Done);
    assert_eq!(count(host.tree.as_ref()).await, 120, "no item twice");
    assert_eq!(count(host.legacy.as_ref()).await, 0);
}

#[tokio::test]
async fn a_write_racing_the_switch_is_caught_up() {
    let host = FakeHost::with(5).await;
    *host.racing_write.lock().unwrap() = Some(fact("written as the switch happened"));
    assert_eq!(go(&host, Trigger::Auto).await, Outcome::Done);
    let texts: Vec<String> = host
        .tree
        .list(ListRequest::new(MetaFilter::default(), 50))
        .await
        .unwrap()
        .items
        .into_iter()
        .map(|hit| hit.text)
        .collect();
    assert_eq!(texts.len(), 6);
    assert!(texts.iter().any(|t| t == "written as the switch happened"));
    assert_eq!(count(host.legacy.as_ref()).await, 0);
}

#[tokio::test]
async fn a_tree_another_account_took_is_left_alone() {
    let mut host = FakeHost::with(3).await;
    let claim = FakeClaim::default();
    // Another account took the tree before this one started.
    claim.other.store(true, Ordering::SeqCst);
    host.claim = Some(claim);
    assert_eq!(
        go(&host, Trigger::Manual { takeover: true }).await,
        Outcome::ClaimedElsewhere
    );
    assert_eq!(count(&*host.legacy).await, 3, "nothing moved");
    assert_eq!(count(&*host.tree).await, 0);
    assert!(
        host.switched.load(Ordering::SeqCst),
        "this account goes on in its own tree, not the one another took"
    );
    assert!(!scan(&host).await.unwrap().needed);
}

/// After an import that ended with its last batch not confirmed listed, an
/// item the listing shows only after the first pass is still moved, in the
/// same run: it copies again after a short wait instead of finishing, and
/// finishes (clearing the flag) once a pass moves nothing new.
#[tokio::test]
async fn after_an_unconfirmed_import_an_item_listed_late_is_still_moved() {
    let host = FakeHost::with(3).await;
    host.unconfirmed.store(true, Ordering::SeqCst);

    // The item is accepted by the import but listed only once the first
    // pass is over: stored while the run waits before its first recheck.
    let late = async {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while host.store.load().unwrap().rechecks == 0 {
            if std::time::Instant::now() > deadline {
                return false;
            }
            tokio::task::yield_now().await;
        }
        host.legacy.store(fact("listed late")).await.unwrap();
        true
    };
    let (outcome, stored_late) = tokio::join!(go(&host, Trigger::Manual { takeover: false }), late);

    assert!(stored_late, "the run waited to copy again");
    assert_eq!(outcome, Outcome::Done);
    assert_eq!(
        count(host.tree.as_ref()).await,
        4,
        "the late item was moved"
    );
    assert_eq!(count(host.legacy.as_ref()).await, 0);
    let state = host.store.load().unwrap();
    assert_eq!(
        state.rechecks, 2,
        "one forced recheck that moved the late item, one that moved nothing"
    );
    assert!(state.rechecked, "re-checking is over");
    assert!(
        host.listed_unconfirmed(),
        "the flag stays: later cleanups keep forgetting by id"
    );
    assert_eq!(
        go(&host, Trigger::Auto).await,
        Outcome::Done,
        "settled, not re-checked again"
    );
}
