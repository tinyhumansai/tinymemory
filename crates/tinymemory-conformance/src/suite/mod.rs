//! The behavioural assertions every driver must satisfy.
//!
//! [`assert_provider`] is the entry point: hand it any bound
//! [`MemoryProvider`] and it drives the contract. Each sub-assertion is also
//! public, so a driver that is mid-implementation can run the parts it claims
//! to support and get a useful failure rather than an unrelated one.
//!
//! # What this is for
//!
//! `audit_provider` already checks that a driver's advertised capabilities
//! match its reachable accessors. That is a structural check: it proves the
//! shape is honest, not that the behaviour is. Nothing before this module
//! checked that two drivers answer the same question the same way, which is
//! precisely the claim "swap the engine" rests on.
//!
//! # Conventions
//!
//! Every assertion namespaces its fixtures under a unique prefix and cleans up
//! after itself, so the suite can run against a driver that already holds data
//! and against a shared live service. Assertions panic with a message naming
//! the driver, because a conformance failure is a bug report and the driver id
//! is the first thing its author needs.

use std::sync::Arc;

use tinymemory_api::capabilities::Capability;
use tinymemory_api::error::MemoryError;
use tinymemory_api::provider::{audit_provider, ExportRecord, MemoryProvider, SourceScope};
use tinymemory_api::recall::OwnedRecallOpts;
use tinymemory_api::types::{MemoryCategory, MemoryTaint};

/// Runs every assertion in the suite.
///
/// # Panics
///
/// Panics on the first violation, naming the driver and what it did instead.
pub async fn assert_provider(provider: Arc<dyn MemoryProvider>) {
    let p = provider.as_ref();

    // Every driver, retaining or not.
    assert_capability_audit(p);
    assert_forget_is_idempotent(p).await;
    assert_namespaces_are_isolated(p).await;
    assert_export_cursor_terminates(p).await;

    // The contract permits a driver that accepts writes and discards them —
    // `NullMemoryProvider` is exactly that, and it is a legitimate binding for a
    // deployment that wants the ports wired and nothing retained. There is no
    // capability that declares it, so the suite probes for it rather than
    // assuming, and reports which half it ran.
    //
    // This is deliberately a probe and not a flag the caller passes: a driver
    // that *intends* to retain and silently does not is the failure mode worth
    // catching, and a caller-supplied flag would let it through.
    if !retains_writes(p).await {
        return;
    }

    assert_store_get_round_trip(p).await;
    assert_upsert_replaces_rather_than_duplicates(p).await;
    assert_list_filters_narrow(p).await;
    assert_taint_is_preserved(p).await;
    assert_recall_respects_limit_and_namespace(p).await;
    assert_recall_respects_source_scope(p).await;
    assert_export_import_round_trip(p).await;
    assert_awkward_content_round_trips(p).await;
    assert_kv_round_trip(p).await;
}

/// Whether this driver reads back what it stores.
///
/// `false` means `/dev/null` semantics, which the contract allows. The storage
/// assertions are vacuous for such a driver and [`assert_provider`] skips them;
/// the contract-shape assertions still apply and are not skipped.
///
/// # Panics
///
/// Panics if the probe itself errors — accepting a write and then failing the
/// read is a fault, distinct from accepting a write and discarding it.
pub async fn retains_writes(provider: &dyn MemoryProvider) -> bool {
    let who = provider.driver_id();
    let ns = ns(provider, "probe");
    provider
        .store(
            &ns,
            "probe",
            "probe",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .unwrap_or_else(|e| panic!("{who}: store failed during the retention probe: {e}"));
    let seen = provider
        .get(&ns, "probe")
        .await
        .unwrap_or_else(|e| panic!("{who}: get failed during the retention probe: {e}"))
        .is_some();
    cleanup(provider, &ns, &["probe"]).await;
    seen
}

/// The advertised capability set equals the reachable one.
///
/// # Panics
///
/// Panics when a driver advertises a family it cannot serve, or serves one it
/// does not advertise.
pub fn assert_capability_audit(provider: &dyn MemoryProvider) {
    if let Err(audit) = audit_provider(provider) {
        panic!(
            "driver `{}` failed its capability audit: {audit}",
            provider.driver_id()
        );
    }
    // The three mandatory families are not optional, whatever else is claimed.
    let caps = provider.capabilities();
    for mandatory in Capability::MANDATORY {
        assert!(
            caps.contains(mandatory),
            "driver `{}` does not advertise the mandatory `{}` family",
            provider.driver_id(),
            mandatory.as_str()
        );
    }
}

/// A stored entry comes back with its fields intact.
///
/// # Panics
///
/// Panics on any field that does not survive the round trip.
pub async fn assert_store_get_round_trip(provider: &dyn MemoryProvider) {
    let ns = ns(provider, "round-trip");
    provider
        .store(
            &ns,
            "k1",
            "the quick brown fox",
            MemoryCategory::Core,
            Some("session-1"),
            MemoryTaint::Internal,
        )
        .await
        .unwrap_or_else(|e| panic!("{}: store failed: {e}", provider.driver_id()));

    let got = provider
        .get(&ns, "k1")
        .await
        .unwrap_or_else(|e| panic!("{}: get failed: {e}", provider.driver_id()))
        .unwrap_or_else(|| panic!("{}: stored entry was not returned", provider.driver_id()));

    let who = provider.driver_id();
    assert_eq!(got.key, "k1", "{who}: key not preserved");
    assert_eq!(
        got.content, "the quick brown fox",
        "{who}: content not preserved"
    );
    assert_eq!(
        got.namespace.as_deref(),
        Some(ns.as_str()),
        "{who}: namespace not preserved"
    );
    assert_eq!(
        got.category,
        MemoryCategory::Core,
        "{who}: category not preserved"
    );
    assert_eq!(
        got.session_id.as_deref(),
        Some("session-1"),
        "{who}: session not preserved"
    );

    // A key that was never stored is `Ok(None)`, never an error.
    let missing = provider
        .get(&ns, "never-stored")
        .await
        .unwrap_or_else(|e| panic!("{who}: get of a missing key errored instead of Ok(None): {e}"));
    assert!(
        missing.is_none(),
        "{who}: get returned an entry for a key never stored"
    );

    cleanup(provider, &ns, &["k1"]).await;
}

/// Storing twice at one `(namespace, key)` replaces rather than duplicates.
///
/// # Panics
///
/// Panics when the second store creates a second row or fails to replace.
pub async fn assert_upsert_replaces_rather_than_duplicates(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let ns = ns(provider, "upsert");
    for content in ["first", "second"] {
        provider
            .store(
                &ns,
                "same-key",
                content,
                MemoryCategory::Core,
                None,
                MemoryTaint::Internal,
            )
            .await
            .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));
    }
    let listed = provider
        .list(Some(&ns), None, None)
        .await
        .unwrap_or_else(|e| panic!("{who}: list failed: {e}"));
    assert_eq!(
        listed.len(),
        1,
        "{who}: a re-store at the same key duplicated the row"
    );
    assert_eq!(
        listed[0].content, "second",
        "{who}: the second store did not replace the first"
    );
    cleanup(provider, &ns, &["same-key"]).await;
}

/// `forget` reports whether the entry existed and is safe to call twice.
///
/// # Panics
///
/// Panics when a repeat `forget` errors or misreports.
pub async fn assert_forget_is_idempotent(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let ns = ns(provider, "forget");
    provider
        .store(
            &ns,
            "k",
            "v",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));

    let first = provider
        .forget(&ns, "k")
        .await
        .unwrap_or_else(|e| panic!("{who}: forget failed: {e}"));
    let second = provider
        .forget(&ns, "k")
        .await
        .unwrap_or_else(|e| panic!("{who}: repeat forget errored instead of Ok(false): {e}"));

    // A driver that discards writes (the `null` reference) legitimately reports
    // `false` both times; what no driver may do is report `true` for an entry it
    // does not hold.
    assert!(
        !second,
        "{who}: forget reported true for an already-forgotten key"
    );
    if first {
        let gone = provider
            .get(&ns, "k")
            .await
            .unwrap_or_else(|e| panic!("{who}: get failed: {e}"));
        assert!(
            gone.is_none(),
            "{who}: forget reported true but the entry is still readable"
        );
    }
}

/// One namespace's entries do not appear in another's.
///
/// # Panics
///
/// Panics when a namespace filter leaks an entry from a sibling namespace.
pub async fn assert_namespaces_are_isolated(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let (a, b) = (ns(provider, "iso-a"), ns(provider, "iso-b"));
    provider
        .store(
            &a,
            "k",
            "belongs to a",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));

    let from_b = provider
        .list(Some(&b), None, None)
        .await
        .unwrap_or_else(|e| panic!("{who}: list failed: {e}"));
    assert!(
        from_b.is_empty(),
        "{who}: listing namespace b returned entries from a: {from_b:?}"
    );

    let get_b = provider
        .get(&b, "k")
        .await
        .unwrap_or_else(|e| panic!("{who}: get failed: {e}"));
    assert!(
        get_b.is_none(),
        "{who}: the same key in a sibling namespace resolved to a's entry"
    );

    cleanup(provider, &a, &["k"]).await;
}

/// Each `list` filter narrows, and `None` everywhere narrows nothing.
///
/// # Panics
///
/// Panics when a filter fails to narrow or narrows the wrong rows.
pub async fn assert_list_filters_narrow(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let ns = ns(provider, "filters");
    provider
        .store(
            &ns,
            "core-a",
            "x",
            MemoryCategory::Core,
            Some("s1"),
            MemoryTaint::Internal,
        )
        .await
        .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));
    provider
        .store(
            &ns,
            "daily-b",
            "y",
            MemoryCategory::Daily,
            Some("s2"),
            MemoryTaint::Internal,
        )
        .await
        .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));

    let all = provider
        .list(Some(&ns), None, None)
        .await
        .unwrap_or_else(|e| panic!("{who}: list failed: {e}"));
    assert_eq!(all.len(), 2, "{who}: expected both entries with no filter");

    let by_category = provider
        .list(Some(&ns), Some(&MemoryCategory::Core), None)
        .await
        .unwrap_or_else(|e| panic!("{who}: list failed: {e}"));
    assert_eq!(
        by_category.len(),
        1,
        "{who}: the category filter did not narrow"
    );
    assert_eq!(
        by_category[0].key, "core-a",
        "{who}: the category filter kept the wrong row"
    );

    let by_session = provider
        .list(Some(&ns), None, Some("s2"))
        .await
        .unwrap_or_else(|e| panic!("{who}: list failed: {e}"));
    assert_eq!(
        by_session.len(),
        1,
        "{who}: the session filter did not narrow"
    );
    assert_eq!(
        by_session[0].key, "daily-b",
        "{who}: the session filter kept the wrong row"
    );

    let summaries = provider
        .namespaces()
        .await
        .unwrap_or_else(|e| panic!("{who}: namespaces failed: {e}"));
    let summary = summaries
        .iter()
        .find(|s| s.namespace == ns)
        .unwrap_or_else(|| panic!("{who}: namespaces omitted a namespace containing two rows"));
    assert_eq!(summary.count, 2, "{who}: namespace summary miscounted");

    cleanup(provider, &ns, &["core-a", "daily-b"]).await;
}

/// Provenance survives a store, and is not re-stamped.
///
/// This is the security-relevant one. A driver that returns `Internal` for
/// content stored as `ExternalSync` has laundered external content into
/// internal-trust content, and every downstream policy gate keyed on taint is
/// then wrong.
///
/// # Panics
///
/// Panics when taint does not survive.
pub async fn assert_taint_is_preserved(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let ns = ns(provider, "taint");
    for (key, taint) in [
        ("internal", MemoryTaint::Internal),
        ("external", MemoryTaint::ExternalSync),
    ] {
        provider
            .store(&ns, key, "content", MemoryCategory::Core, None, taint)
            .await
            .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));
        if let Some(got) = provider
            .get(&ns, key)
            .await
            .unwrap_or_else(|e| panic!("{who}: get failed: {e}"))
        {
            assert_eq!(
                got.taint, taint,
                "{who}: taint was re-stamped on `{key}` — stored {taint:?}, read back {:?}",
                got.taint
            );
        }
    }
    cleanup(provider, &ns, &["internal", "external"]).await;
}

/// `recall` honours its limit and its namespace filter.
///
/// # Panics
///
/// Panics when recall exceeds the limit or crosses a namespace.
pub async fn assert_recall_respects_limit_and_namespace(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let (mine, theirs) = (ns(provider, "recall-a"), ns(provider, "recall-b"));
    let keys = ["r1", "r2", "r3"];
    for key in keys {
        provider
            .store(
                &mine,
                key,
                "shared needle text",
                MemoryCategory::Core,
                None,
                MemoryTaint::Internal,
            )
            .await
            .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));
    }
    provider
        .store(
            &theirs,
            "other",
            "shared needle text",
            MemoryCategory::Core,
            None,
            MemoryTaint::Internal,
        )
        .await
        .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));

    let opts = OwnedRecallOpts {
        namespace: Some(mine.clone()),
        ..Default::default()
    };
    let hits = provider
        .recall("needle", 2, &opts, None)
        .await
        .unwrap_or_else(|e| panic!("{who}: recall failed: {e}"));
    assert!(
        hits.len() <= 2,
        "{who}: recall returned {} hits for a limit of 2",
        hits.len()
    );
    assert_eq!(
        hits.len(),
        2,
        "{who}: recall returned too few matching rows; an empty recall must not conform"
    );
    for hit in &hits {
        assert_eq!(
            hit.namespace.as_deref(),
            Some(mine.as_str()),
            "{who}: recall crossed a namespace boundary"
        );
    }

    cleanup(provider, &mine, &keys).await;
    cleanup(provider, &theirs, &["other"]).await;
}

/// A present, empty source scope fails closed.
///
/// # Panics
///
/// Panics when a driver ignores an empty [`SourceScope`] and returns content.
pub async fn assert_recall_respects_source_scope(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let ns = ns(provider, "recall-scope");
    provider
        .store(
            &ns,
            "scoped",
            "source scoped needle",
            MemoryCategory::Core,
            None,
            MemoryTaint::ExternalSync,
        )
        .await
        .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));
    let opts = OwnedRecallOpts {
        namespace: Some(ns.clone()),
        ..Default::default()
    };
    match provider
        .recall("needle", 8, &opts, Some(&SourceScope::default()))
        .await
    {
        Ok(hits) => assert!(
            hits.is_empty(),
            "{who}: recall ignored an empty source scope and returned {hits:?}"
        ),
        // A driver whose recall path cannot apply the predicate must refuse
        // the call. This is still fail-closed; answering it unscoped is not.
        Err(MemoryError::Invalid(_)) => {}
        Err(other) => panic!("{who}: scoped recall failed with the wrong error class: {other}"),
    }
    cleanup(provider, &ns, &["scoped"]).await;
}

/// Exported records re-import with their taint intact.
///
/// # Panics
///
/// Panics when a round trip loses a record or its provenance.
pub async fn assert_export_import_round_trip(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let ns = ns(provider, "portability");
    provider
        .store(
            &ns,
            "p1",
            "portable",
            MemoryCategory::Core,
            None,
            MemoryTaint::ExternalSync,
        )
        .await
        .unwrap_or_else(|e| panic!("{who}: store failed: {e}"));

    let mut mine: Vec<ExportRecord> = Vec::new();
    let mut cursor: Option<String> = None;
    // Bounded: a driver whose cursor never terminates is a hang, and a hang in
    // a conformance suite reads as an infrastructure problem rather than a bug.
    for _ in 0..64 {
        let page = provider
            .export_page(cursor.as_deref(), 32)
            .await
            .unwrap_or_else(|e| panic!("{who}: export_page failed: {e}"));
        mine.extend(
            page.records
                .iter()
                .filter(|r| r.namespace.as_deref() == Some(ns.as_str()))
                .cloned(),
        );
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert!(
        !mine.is_empty(),
        "{who}: a stored entry did not appear in any export page"
    );

    let exported = mine
        .iter()
        .find(|r| r.taint == MemoryTaint::ExternalSync)
        .unwrap_or_else(|| panic!("{who}: export dropped the record's ExternalSync taint"));
    assert_eq!(exported.taint, MemoryTaint::ExternalSync);

    let removed = provider
        .forget(&ns, "p1")
        .await
        .unwrap_or_else(|e| panic!("{who}: forget before import failed: {e}"));
    assert!(
        removed,
        "{who}: forget before import reported that the exported record was absent"
    );
    let absent = provider
        .get(&ns, "p1")
        .await
        .unwrap_or_else(|e| panic!("{who}: get after forget failed: {e}"));
    assert!(
        absent.is_none(),
        "{who}: record remained readable before import, so the restore was not verified"
    );
    let outcome = provider
        .import_records(mine.clone())
        .await
        .unwrap_or_else(|e| panic!("{who}: import failed: {e}"));
    assert_eq!(
        outcome.failed, 0,
        "{who}: import rejected its own export: {:?}",
        outcome.errors
    );
    if outcome.failed > 0 {
        assert!(
            !outcome.errors.is_empty(),
            "{who}: reported failures with no diagnosable reason"
        );
    }

    assert_eq!(
        outcome.imported as usize,
        mine.len(),
        "{who}: import did not report every accepted record"
    );
    let back = provider
        .get(&ns, "p1")
        .await
        .unwrap_or_else(|e| panic!("{who}: get after import failed: {e}"))
        .unwrap_or_else(|| panic!("{who}: import reported success but restored no record"));
    assert_eq!(
        back.taint,
        MemoryTaint::ExternalSync,
        "{who}: import re-stamped provenance instead of persisting what it was given"
    );
    cleanup(provider, &ns, &["p1"]).await;
}

/// The export cursor terminates on `None`, not on an empty page.
///
/// # Panics
///
/// Panics when a driver signals completion with an empty page while still
/// handing back a cursor, or rejects nothing for a cursor it never issued.
pub async fn assert_export_cursor_terminates(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let page = provider
        .export_page(None, 8)
        .await
        .unwrap_or_else(|e| panic!("{who}: export_page failed: {e}"));
    if page.records.is_empty() {
        assert!(
            page.next_cursor.is_none(),
            "{who}: an empty page handed back a cursor — a caller following it cannot terminate"
        );
    }
    // A cursor this driver never issued must be refused rather than silently
    // restarting the export from the beginning, which would duplicate rows.
    let bogus = provider.export_page(Some("!not-a-cursor!"), 8).await;
    assert!(
        matches!(bogus, Err(MemoryError::Invalid(_))),
        "{who}: an unrecognised cursor must return Invalid, got {bogus:?}"
    );
}

/// Unicode, empty, and oversized content survive a round trip.
///
/// # Panics
///
/// Panics when any of them is mangled.
pub async fn assert_awkward_content_round_trips(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let ns = ns(provider, "awkward");
    let cases: [(&str, String); 4] = [
        ("unicode", "héllo — 👋 まいど".to_string()),
        ("empty", String::new()),
        ("large", "x".repeat(64 * 1024)),
        ("newlines", "a\nb\r\nc\0d".to_string()),
    ];
    let mut accepted: Vec<&str> = Vec::new();
    for (key, content) in &cases {
        // A driver may refuse a shape outright — `MemoryCore::store` documents
        // `Invalid` "for caller input the driver rejects", and the TinyCortex
        // engine uses that to refuse empty content. What a driver may *not* do
        // is accept a value and hand back something else.
        //
        // §A4 landed: a refusal must now be `Invalid` — the caller's input
        // was rejected — never a transport/backend class wearing a refusal's
        // clothes. A driver that answers `Unauthorized` or `Backend` here is
        // not refusing the shape, it is failing, and failure must fail the
        // suite instead of reading as a documented refusal.
        match provider
            .store(
                &ns,
                key,
                content,
                MemoryCategory::Core,
                None,
                MemoryTaint::Internal,
            )
            .await
        {
            Ok(()) => {}
            Err(MemoryError::Invalid(_)) => continue,
            Err(other) => panic!(
                "{who}: refusing `{key}` must be Invalid (a validation refusal); got: {other}"
            ),
        }
        accepted.push(key);
        if let Some(got) = provider
            .get(&ns, key)
            .await
            .unwrap_or_else(|e| panic!("{who}: get of `{key}` failed: {e}"))
        {
            assert_eq!(&got.content, content, "{who}: `{key}` content was mangled");
        }
    }
    // "May refuse" needs a floor, or a driver that refused everything would pass
    // having stored nothing. The floor is `unicode` specifically rather than a
    // count: refusing `empty` is documented validation, and refusing `large` is
    // a defensible size limit, but refusing ordinary UTF-8 text is a broken
    // driver — and unicode is the case where mangling actually shows, since
    // truncation and re-encoding are invisible on ASCII.
    assert!(
        accepted.contains(&"unicode"),
        "{who}: refused ordinary UTF-8 content — accepted {accepted:?}. A driver \
         may refuse a shape, but not this one; every assertion about content \
         surviving a round trip rests on it."
    );
    let keys: Vec<&str> = cases.iter().map(|(k, _)| *k).collect();
    cleanup(provider, &ns, &keys).await;
}

/// The key/value family round-trips: put → get → list-by-prefix → delete.
///
/// As wired in [`assert_provider`], also skipped for a driver that fails the
/// `retains_writes` probe (the early return precedes every assert): a
/// non-retaining driver would fail the put→get leg for retention reasons,
/// which is not the asymmetry this case exists to catch.
///
/// Skipped when the driver does not serve the optional `Graph` family — the
/// `as_graph()` accessor is the negotiated surface, and
/// [`assert_capability_audit`] already pins that it agrees with the advertised
/// [`Capability`] set, so probing the accessor *is* the capability check.
///
/// One leg uses a formatted national-ID key on purpose. A driver may
/// canonicalize identifiers on write (PII scrubbing rewrites the stored key),
/// and the contract this asserts is *symmetry*: whatever transform the write
/// path applies, every read path must apply too. A driver whose `kv_get` /
/// `kv_list` compare the raw caller key misses every rewritten key — put→get
/// answers `None` forever — while its canonicalizing `kv_delete` still
/// reports `true`, which is exactly the asymmetry that stays invisible
/// without this case. The read-back key is deliberately *not* asserted to
/// equal the caller's: it is the stored (possibly canonical) form.
///
/// # Panics
///
/// Panics when a just-put key cannot be read, listed under its own prefix, or
/// deleted, or when it is still readable after a `true` delete.
pub async fn assert_kv_round_trip(provider: &dyn MemoryProvider) {
    let who = provider.driver_id();
    let Some(graph) = provider.as_graph() else {
        return;
    };
    let ns = ns(provider, "kv");
    // A plain key, and one shaped like a formatted national ID — the shape
    // identifier-canonicalizing drivers rewrite on write. (Not an email and
    // not a bare digit run: both are deliberately outside the strict PII
    // boundary gates such drivers use, so neither would exercise a rewrite.)
    // The value carries no PII or secret shapes — drivers may scrub *content*
    // more aggressively than identifiers, and this case asserts key symmetry,
    // not content fidelity (that is `assert_awkward_content_round_trips`'s
    // job for the document tier).
    for (marker, key) in [("plain", "plain-key"), ("rewritten", "ssn-123-45-6789")] {
        let value = serde_json::json!({ "leg": marker });
        graph
            .kv_put(Some(&ns), key, value.clone())
            .await
            .unwrap_or_else(|e| panic!("{who}: kv_put of `{key}` failed: {e}"));

        let got = graph
            .kv_get(Some(&ns), key)
            .await
            .unwrap_or_else(|e| panic!("{who}: kv_get of `{key}` failed: {e}"))
            .unwrap_or_else(|| {
                panic!(
                    "{who}: kv_get did not find `{key}` right after kv_put — the read \
                     path does not apply the write path's key transform"
                )
            });
        assert_eq!(
            got.value, value,
            "{who}: kv_get of `{key}` surfaced another record's value"
        );

        // The caller's key must work as a prefix of its own record: prefix
        // matching is over stored keys, so a driver that rewrites the key on
        // write has to rewrite the prefix on read the same way.
        let listed = graph
            .kv_list(Some(&ns), Some(key), 16)
            .await
            .unwrap_or_else(|e| panic!("{who}: kv_list under `{key}` failed: {e}"));
        assert!(
            listed.iter().any(|record| record.value == value),
            "{who}: kv_list under the `{key}` prefix did not surface the record"
        );

        assert!(
            graph
                .kv_delete(Some(&ns), key)
                .await
                .unwrap_or_else(|e| panic!("{who}: kv_delete of `{key}` failed: {e}")),
            "{who}: kv_delete did not find `{key}`"
        );
        let gone = graph
            .kv_get(Some(&ns), key)
            .await
            .unwrap_or_else(|e| panic!("{who}: kv_get after delete failed: {e}"));
        assert!(
            gone.is_none(),
            "{who}: `{key}` is still readable after kv_delete reported true"
        );
    }
}

/// A namespace unique to this driver and assertion.
///
/// Prefixed so the suite can run against a live service holding real data
/// without colliding with it, and without needing a teardown it might not get.
fn ns(provider: &dyn MemoryProvider, what: &str) -> String {
    format!("tinymemory-conformance/{}/{what}", provider.driver_id())
}

/// Best-effort teardown. Failures are ignored: a driver that cannot delete is
/// reported by [`assert_forget_is_idempotent`], and failing here would mask the
/// assertion that actually found the problem.
async fn cleanup(provider: &dyn MemoryProvider, namespace: &str, keys: &[&str]) {
    for key in keys {
        let _ = provider.forget(namespace, key).await;
    }
}
