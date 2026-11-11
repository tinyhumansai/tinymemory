//! The CortexDB engines against a real server, on each wire configured:
//!
//! - `cortexdb` (Direct): when `TINYMEMORY_LIVE_CORTEXDB_URL` names a CortexDB
//!   server. The harness in `integration/cortexdb/` boots a pinned one, and
//!   `scripts/cortexdb-live.sh` runs this whole file against it. The key
//!   defaults to the harness's (`TINYMEMORY_TEST_CORTEX_KEY`).
//! - `tinyhumans` (hosted, behind the TinyHumans backend): when
//!   `TINYMEMORY_LIVE_TINYHUMANS_URL` names the backend and
//!   `TINYMEMORY_TEST_TINYHUMANS_TOKEN` holds a session JWT or `tiny_live_` key
//!   for a test account. This writes to that account's hosted memory, which
//!   is billed, and forgets what it wrote.
//!
//! With neither set, every test skips.
//!
//! Two passes: the shared conformance suite, then the three stores the host
//! uses (a document, a conversation with a tool call, a learning) read back
//! through `list`, `fetch` and `recall`, compiled into `context.md`, and
//! forgotten.

// The helpers outside `#[test]` fns fail the test by panicking, like the tests.
#![allow(clippy::expect_used)]

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use std::sync::Arc;
use tinymemory_api::{
    EraseRequest, FetchMode, FetchRequest, ForgetTarget, ItemKind, LearningKind, ListRequest,
    MemoryEngine, MemoryMeta, MetaFilter, RecallRequest, Role, SourceKind, SourceRef, StoreItem,
    ToolCallRef, Turn,
};

use tinymemory_integrations::cortex::{CortexCredential, CortexEngine, StaticBearer};
use tinymemory_tools::context::{ContextSpec, compile};

const DEFAULT_KEY: &str = "tinymemory-cortex-test";

/// How long a stored item may take to become readable. CortexDB indexes
/// asynchronously, so a write is not visible to the very next read.
const VISIBILITY: Duration = Duration::from_secs(60);

/// Every live engine the environment configures, labelled by wire.
fn live_engines() -> Vec<(&'static str, CortexEngine)> {
    let mut engines = Vec::new();
    if let Ok(url) = std::env::var("TINYMEMORY_LIVE_CORTEXDB_URL") {
        let key =
            std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into());
        engines.push((
            "cortexdb",
            CortexEngine::direct(&url, CortexCredential::api_key(key))
                .expect("a valid live CortexDB endpoint"),
        ));
    }
    if let Ok(url) = std::env::var("TINYMEMORY_LIVE_TINYHUMANS_URL") {
        let token = std::env::var("TINYMEMORY_TEST_TINYHUMANS_TOKEN")
            .expect("TINYMEMORY_LIVE_TINYHUMANS_URL needs TINYMEMORY_TEST_TINYHUMANS_TOKEN");
        engines.push((
            "tinyhumans",
            CortexEngine::tinyhumans(&url, Arc::new(StaticBearer::new(token)))
                .expect("a valid live TinyHumans endpoint"),
        ));
    }
    if engines.is_empty() {
        eprintln!(
            "neither TINYMEMORY_LIVE_CORTEXDB_URL nor TINYMEMORY_LIVE_TINYHUMANS_URL set; skipping"
        );
    }
    engines
}

fn run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    format!("live-{nanos}")
}

/// The lookup-label digest prefix used to inspect a broad live pack without
/// sending the workspace path itself.
fn label_digest(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn meta(workspace: &str, source: SourceKind) -> MemoryMeta {
    MemoryMeta {
        workspace: Some(workspace.to_string()),
        source: SourceRef {
            kind: source,
            id: Some(format!("{workspace}-{}", source.as_str())),
        },
        ..MemoryMeta::default()
    }
}

/// Lists `filter` until it holds `want` items or [`VISIBILITY`] runs out.
async fn list_until(engine: &CortexEngine, filter: &MetaFilter, want: usize) -> Vec<String> {
    let deadline = Instant::now() + VISIBILITY;
    loop {
        let page = engine
            .list(ListRequest::new(filter.clone(), 50))
            .await
            .expect("list");
        let texts: Vec<String> = page.items.into_iter().map(|hit| hit.text).collect();
        if texts.len() >= want || Instant::now() >= deadline {
            return texts;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Lists `filter` until it holds exactly `want` (in any order) or
/// [`VISIBILITY`] runs out, and returns the last listing, sorted.
async fn list_exactly(engine: &CortexEngine, filter: &MetaFilter, want: &[String]) -> Vec<String> {
    let mut want = want.to_vec();
    want.sort();
    let deadline = Instant::now() + VISIBILITY;
    loop {
        let mut texts: Vec<String> = engine
            .list(ListRequest::new(filter.clone(), 50))
            .await
            .expect("list")
            .items
            .into_iter()
            .map(|hit| hit.text)
            .collect();
        texts.sort();
        if texts == want || Instant::now() >= deadline {
            return texts;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Held by each live test for its whole run, so they reach the server one
/// at a time. Run together, one test's forgets drop the packs another is
/// about to answer from, and load the server enough that forgetting a long
/// document (seconds per ~240 KiB event on 0.10.4) outlasts the request
/// timeout.
static ONE_AT_A_TIME: futures::lock::Mutex<()> = futures::lock::Mutex::new(());

#[tokio::test]
async fn the_live_server_upholds_the_contract() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (wire, engine) in live_engines() {
        eprintln!("conformance on {wire}");
        tinymemory_api::conformance::run(&engine)
            .await
            .unwrap_or_else(|error| panic!("the live {wire} engine conforms: {error}"));
    }
}

/// Layout v3 on the real server: the whole contract below a fresh person's
/// root, which a direct engine registers with that person as owner.
#[tokio::test]
async fn the_live_server_upholds_the_contract_in_layout_v3() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let mut roots = Vec::new();
    for (wire, engine) in live_engines() {
        // A fresh root per wire, so the registration checked below is this
        // run's own on this wire.
        let root = format!("user:{}", run_id());
        roots.push((wire, root.clone()));
        eprintln!("layout v3 conformance on {wire} below {root}");
        let engine = engine
            .with_scope_root(&root, Some(&root))
            .expect("a valid scope root");
        tinymemory_api::conformance::run(&engine)
            .await
            .unwrap_or_else(|error| panic!("the live {wire} engine conforms in v3: {error}"));
    }
    let Ok(url) = std::env::var("TINYMEMORY_LIVE_CORTEXDB_URL") else {
        return;
    };
    let Some((_, root)) = roots.into_iter().find(|(wire, _)| *wire == "cortexdb") else {
        return;
    };
    let key = std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into());
    let record: serde_json::Value = reqwest::Client::new()
        .get(format!("{url}/v1/scopes"))
        .query(&[("path", root.as_str())])
        .bearer_auth(key)
        .send()
        .await
        .expect("look the root up")
        .json()
        .await
        .expect("a scope record");
    let owners: Vec<&str> = record["members"]
        .as_array()
        .expect("members")
        .iter()
        .filter(|member| member["role"] == "owner")
        .filter_map(|member| member["actor"].as_str())
        .collect();
    assert!(owners.contains(&root.as_str()), "{record}");
    assert_eq!(
        record["auto_provisioned"], false,
        "registered, not auto-made: {record}"
    );
}

/// A real channel filter must narrow CortexDB before its top-k budget. Seed
/// five distinct same-scope records, inspect the actual broad top two, and
/// choose the wanted record from those the server omitted.
#[tokio::test]
async fn a_channel_filter_recalls_beyond_the_broad_pack_budget() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let Ok(url) = std::env::var("TINYMEMORY_LIVE_CORTEXDB_URL") else {
        eprintln!("TINYMEMORY_LIVE_CORTEXDB_URL unset; skipping");
        return;
    };
    let key = std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into());
    let engine = CortexEngine::direct(&url, CortexCredential::api_key(key))
        .expect("a valid live CortexDB endpoint");
    let run = run_id();
    let namespace: tinymemory_api::Namespace = format!("project:channel-{run}")
        .parse()
        .expect("a valid namespace");
    let workspace = format!("/tmp/tinymemory-channel-{run}");
    let marker = "Cobalt memory";
    let candidates = [
        ("Cobalt memory alpha", "channel-alpha", "alpha"),
        ("Cobalt memory bravo", "channel-bravo", "bravo"),
        ("Cobalt memory charlie", "channel-charlie", "charlie"),
        ("Cobalt memory delta", "channel-delta", "delta"),
        ("Cobalt memory echo", "channel-echo", "echo"),
    ];
    let mut ids = Vec::with_capacity(candidates.len());
    for (text, channel, source_id) in candidates {
        let item = StoreItem::document(
            text,
            MemoryMeta {
                namespace: namespace.clone(),
                workspace: Some(workspace.clone()),
                channel: Some(channel.to_string()),
                source: SourceRef {
                    kind: SourceKind::Agent,
                    id: Some(format!("{run}-{source_id}")),
                },
                ..MemoryMeta::default()
            },
        );
        ids.push(item.fingerprint());
        engine.store(item).await.expect("store a channel candidate");
    }

    let reach = Some(tinymemory_api::Reach::subtree(namespace.clone()));
    let broad_filter = MetaFilter {
        workspace: Some(workspace.clone()),
        reach: reach.clone(),
        ..MetaFilter::kinds([ItemKind::Document])
    };
    let mut expected_texts: Vec<String> = candidates
        .iter()
        .map(|(text, _, _)| (*text).to_string())
        .collect();
    expected_texts.sort();
    let listed = list_exactly(&engine, &broad_filter, &expected_texts).await;
    assert_eq!(
        listed, expected_texts,
        "all five candidates reached the server"
    );

    // Inspect the server's actual broad pack, not the smaller citation page.
    // Its two results establish the cutoff; choosing one of the other three
    // candidates avoids any assumption about vector ranking or tie-breaks.
    let scope = format!("app:tinymemory/{namespace}/app:documents");
    let broad_label = format!("tm:w:{}", label_digest(&workspace));
    let broad_deadline = Instant::now() + VISIBILITY;
    let (broad_pack, broad_texts) = loop {
        let pack: serde_json::Value = reqwest::Client::new()
            .post(format!("{url}/v1/recall"))
            .bearer_auth(
                std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into()),
            )
            .json(&serde_json::json!({
                "scope": scope,
                "query": marker,
                "view": "granular",
                "include": ["events"],
                "filters": { "metadata": { "labels": [broad_label] } },
                "budgets": {
                    "max_tokens": 1_572_864,
                    "per_layer_limits": { "events": 2 }
                }
            }))
            .send()
            .await
            .expect("request the broad two-event control pack")
            .error_for_status()
            .expect("the broad control pack succeeds")
            .json()
            .await
            .expect("the broad control pack is JSON");
        let events = pack
            .pointer("/layers/events")
            .and_then(serde_json::Value::as_array)
            .expect("the broad control pack has events");
        let texts: Vec<String> = events
            .iter()
            .filter_map(|event| {
                event
                    .pointer("/content/text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        if events.len() == 2 && texts.len() == 2 {
            break (pack, texts);
        }
        assert!(
            Instant::now() < broad_deadline,
            "broad events are still indexing: {texts:?}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    assert!(
        broad_texts.iter().all(|text| expected_texts.contains(text)),
        "the broad pack returned only seeded candidates: {broad_texts:?}"
    );
    assert!(
        !broad_pack.to_string().contains(&workspace),
        "the local workspace path is never returned in event labels"
    );
    assert!(
        broad_pack
            .pointer("/layers/events")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|events| events.iter().all(|event| {
                event
                    .pointer("/context/labels")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|labels| {
                        labels.contains(&serde_json::Value::String(broad_label.clone()))
                    })
            })),
        "the raw control only narrows by the hashed workspace label"
    );
    let wanted_index = candidates
        .iter()
        .position(|(text, _, _)| !broad_texts.iter().any(|seen| seen == text))
        .expect("three candidates are outside the two-event broad pack");
    let (wanted_text, wanted_channel, _) = candidates[wanted_index];
    let wanted_id = ids[wanted_index].as_str();
    let distractor_ids: Vec<&str> = ids
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != wanted_index)
        .map(|(_, id)| id.as_str())
        .collect();
    let channel_filter = MetaFilter {
        channel: Some(wanted_channel.to_string()),
        ..broad_filter
    };

    let deadline = Instant::now() + VISIBILITY;
    let channel_answer = loop {
        let mut recall = RecallRequest::new(marker, 1);
        recall.filter = channel_filter.clone();
        let answer = engine.recall(recall).await.expect("channel recall");
        if answer
            .citations
            .iter()
            .any(|citation| citation.id.as_str() == wanted_id)
        {
            break answer;
        }
        assert!(
            Instant::now() < deadline,
            "channel recall did not return {wanted_text}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    let recall_ids: Vec<&str> = channel_answer
        .citations
        .iter()
        .map(|citation| citation.id.as_str())
        .collect();
    assert_eq!(recall_ids, [wanted_id], "recall result: {recall_ids:?}");
    assert!(
        recall_ids
            .iter()
            .all(|id| distractor_ids.iter().all(|other| *other != *id)),
        "recall excludes every competing channel"
    );

    let mut fetch = FetchRequest::new(marker, FetchMode::Hybrid, 1);
    fetch.filter = channel_filter.clone();
    let fetched = engine.fetch(fetch).await.expect("channel fetch");
    let fetched_ids: Vec<&str> = fetched.hits.iter().map(|hit| hit.id.as_str()).collect();
    assert_eq!(fetched_ids, [wanted_id], "fetch result: {fetched_ids:?}");
    assert!(
        fetched_ids
            .iter()
            .all(|id| distractor_ids.iter().all(|other| *other != *id)),
        "fetch excludes every competing channel"
    );

    let mut evidence = RecallRequest::new(marker, 1);
    evidence.mode = tinymemory_api::RecallMode::Evidence;
    evidence.filter = channel_filter;
    let evidence = engine
        .recall(evidence)
        .await
        .expect("channel Evidence recall");
    let evidence_ids: Vec<&str> = evidence
        .citations
        .iter()
        .map(|citation| citation.id.as_str())
        .collect();
    assert_eq!(
        evidence_ids,
        [wanted_id],
        "Evidence result: {evidence_ids:?}"
    );
    assert!(
        evidence_ids
            .iter()
            .all(|id| distractor_ids.iter().all(|other| *other != *id)),
        "Evidence excludes every competing channel"
    );

    engine
        .forget(ForgetTarget::Filter(MetaFilter {
            reach: Some(tinymemory_api::Reach::subtree(namespace)),
            ..MetaFilter::default()
        }))
        .await
        .expect("forget the isolated fixture scope");
}

/// No write claims a v3 root: CortexDB registers only the scope a write
/// lands in, and v3 never writes to the root itself. So a root written to
/// before its owner registration (one that failed, say) is still free, and
/// the next write with an owner registers it to that owner. Direct only.
#[tokio::test]
async fn a_write_never_claims_a_v3_root_before_its_owner() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let Ok(url) = std::env::var("TINYMEMORY_LIVE_CORTEXDB_URL") else {
        eprintln!("TINYMEMORY_LIVE_CORTEXDB_URL unset; skipping");
        return;
    };
    let key = std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into());
    let root = format!("user:{}", run_id());
    let record = || async {
        reqwest::Client::new()
            .get(format!("{url}/v1/scopes"))
            .query(&[("path", root.as_str())])
            .bearer_auth(&key)
            .send()
            .await
            .expect("look the root up")
    };
    let engine = |owner: Option<&str>| {
        CortexEngine::direct(&url, CortexCredential::api_key(key.clone()))
            .expect("a valid live CortexDB endpoint")
            .with_scope_root(&root, owner)
            .expect("a valid scope root")
    };
    let item =
        |text: &str| StoreItem::learning(text, LearningKind::Fact, 0.8, MemoryMeta::default());

    let first = engine(None)
        .store(item("written before any owner"))
        .await
        .expect("store without an owner");
    // The write reached the v3 root's learnings: it reads back there.
    let back = engine(None)
        .get(tinymemory_api::GetRequest {
            ids: vec![first.id.clone()],
            reach: Some(tinymemory_api::Reach::exact(
                tinymemory_api::Namespace::ROOT,
            )),
        })
        .await
        .expect("get it back below the root");
    assert_eq!(back.len(), 1, "the write landed below {root}: {back:?}");
    assert_eq!(
        record().await.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a write below the root left the root unregistered"
    );

    engine(Some(&root))
        .store(item("written with an owner"))
        .await
        .expect("store with an owner");
    let record: serde_json::Value = record().await.json().await.expect("a scope record");
    let owners: Vec<&str> = record["members"]
        .as_array()
        .expect("members")
        .iter()
        .filter(|member| member["role"] == "owner")
        .filter_map(|member| member["actor"].as_str())
        .collect();
    assert!(owners.contains(&root.as_str()), "{record}");
}

/// Attribution on against the real server: whether it accepts
/// `observed_actor` and `subject`, ignores them, or refuses them for want of
/// a capability, every write lands, and the sender stays on the item.
/// Direct only: the hosted wire never attributes.
#[tokio::test]
async fn attributed_writes_land_whatever_the_server_allows() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let Ok(url) = std::env::var("TINYMEMORY_LIVE_CORTEXDB_URL") else {
        eprintln!("TINYMEMORY_LIVE_CORTEXDB_URL unset; skipping");
        return;
    };
    let key = std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into());
    let root = format!("user:{}", run_id());
    let engine = CortexEngine::direct(&url, CortexCredential::api_key(key))
        .expect("a valid live CortexDB endpoint")
        .with_scope_root(&root, Some(&root))
        .expect("a valid scope root")
        .with_observed_actor(true);
    let chat = StoreItem::Conversation {
        turns: vec![
            Turn::new(Role::User, "lunch tomorrow?"),
            Turn::new(Role::Assistant, "Booked a table at noon."),
        ],
        meta: MemoryMeta {
            agent_id: Some("orchestrator".into()),
            ..MemoryMeta::default()
        },
    };
    let email = StoreItem::document(
        "Lunch is on me.",
        MemoryMeta {
            observed_actor: Some(tinymemory_api::ObservedActor {
                id: "user:priya@acme.com".into(),
                name: Some("Priya".into()),
            }),
            ..MemoryMeta::default()
        },
    );
    let receipts = engine
        .store_many(vec![chat, email])
        .await
        .expect("attributed writes land");
    let back = engine
        .get(tinymemory_api::GetRequest {
            ids: receipts.iter().map(|receipt| receipt.id.clone()).collect(),
            reach: Some(tinymemory_api::Reach::exact(
                tinymemory_api::Namespace::ROOT,
            )),
        })
        .await
        .expect("get them back");
    assert_eq!(back.len(), 2, "both items read back: {back:?}");
    let sender = back
        .iter()
        .find_map(|hit| hit.meta.observed_actor.clone())
        .expect("the email keeps its sender");
    assert_eq!(sender.id, "user:priya@acme.com");
}

#[tokio::test]
async fn every_kind_is_exported_whole_across_pages() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (wire, engine) in live_engines() {
        eprintln!("export on {wire}");
        let workspace = run_id();
        let at = tinymemory_api::chrono::TimeZone::with_ymd_and_hms(
            &tinymemory_api::chrono::Utc,
            2026,
            5,
            6,
            7,
            8,
            9,
        )
        .single()
        .expect("a valid time");
        let mut stored = vec![
            StoreItem::Conversation {
                turns: vec![
                    Turn {
                        role: Role::User,
                        text: "when is the review".into(),
                        at: Some(at),
                        tool_calls: Vec::new(),
                    },
                    Turn {
                        role: Role::Assistant,
                        text: "Thursday at ten".into(),
                        at: Some(at),
                        tool_calls: vec![ToolCallRef {
                            name: "calendar".into(),
                            id: Some("call-1".into()),
                        }],
                    },
                ],
                meta: meta(&workspace, SourceKind::Conversation),
            },
            StoreItem::Document {
                title: Some("Plan".into()),
                body: tinymemory_api::DocumentBody::Text("The plan has three steps.".into()),
                mime: Some("text/markdown".into()),
                meta: meta(&workspace, SourceKind::File),
            },
        ];
        let chapters: Vec<String> = (1..=12)
            .map(|page| {
                format!(
                    "# Chapter {page}\n\n{}",
                    "Export policy text. ".repeat(1500)
                )
            })
            .collect();
        stored.push(StoreItem::Document {
            title: Some("Handbook".into()),
            body: tinymemory_api::DocumentBody::Text(chapters.join("\u{c}")),
            mime: Some("application/pdf".into()),
            meta: meta(&workspace, SourceKind::File),
        });
        for n in 0..3 {
            stored.push(StoreItem::Learning {
                text: format!("export fact {n}"),
                kind: LearningKind::Preference,
                confidence: 0.75,
                evidence: Some("said so".into()),
                meta: meta(&workspace, SourceKind::Agent),
            });
        }
        for item in &stored {
            engine.store(item.clone()).await.expect("store");
        }
        let filter = MetaFilter {
            workspace: Some(workspace.clone()),
            ..MetaFilter::default()
        };
        // Each store returned once listable; this only guards a server that
        // lags behind its own listing.
        let visible = list_until(&engine, &filter, stored.len()).await;
        assert_eq!(visible.len(), stored.len(), "every stored item lists");
        let (mut exported, mut cursor) = (Vec::new(), None);
        for _ in 0..20 {
            let mut request = ListRequest::new(filter.clone(), 2);
            request.cursor = cursor;
            let page = engine.export(request).await.expect("export");
            assert!(page.incomplete.is_empty(), "{:?}", page.incomplete);
            exported.extend(page.items);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        assert!(cursor.is_none(), "the export ended");
        let mut want: Vec<String> = stored.iter().map(StoreItem::fingerprint).collect();
        let mut got: Vec<String> = exported.iter().map(|e| e.id.as_str().to_string()).collect();
        want.sort();
        got.sort();
        assert_eq!(got, want, "exactly the stored items, each once");
        for item in &stored {
            let found = exported
                .iter()
                .find(|e| e.id.as_str() == item.fingerprint())
                .expect("exported");
            assert_eq!(&found.item, item, "whole, exactly as stored");
            let again = engine.store(found.item.clone()).await.expect("store");
            assert!(again.replayed, "storing it where it was is a replay");
        }
        engine
            .forget(ForgetTarget::Filter(filter))
            .await
            .expect("cleanup");
    }
}

/// Direct only for now: the hosted `memory/v1/erasures` passthrough is
/// covered by the wire double until it is live on the backend.
#[tokio::test]
async fn an_erased_node_is_gone_and_its_items_store_anew() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (wire, engine) in live_engines() {
        if wire != "cortexdb" {
            continue;
        }
        let run = run_id();
        let node: tinymemory_api::Namespace =
            format!("agent:{run}-erased").parse().expect("a namespace");
        let child: tinymemory_api::Namespace = format!("agent:{run}-erased/agent:step")
            .parse()
            .expect("a namespace");
        let kept: tinymemory_api::Namespace =
            format!("agent:{run}-kept").parse().expect("a namespace");
        let fact = |text: &str, namespace: &tinymemory_api::Namespace| {
            StoreItem::learning(
                format!("{run} {text}"),
                LearningKind::Fact,
                0.9,
                MemoryMeta {
                    namespace: namespace.clone(),
                    workspace: Some(run.clone()),
                    ..MemoryMeta::default()
                },
            )
        };
        let items = [
            fact("node", &node),
            fact("step", &child),
            fact("kept", &kept),
        ];
        for item in &items {
            engine.store(item.clone()).await.expect("store");
        }
        let filter = MetaFilter {
            workspace: Some(run.clone()),
            ..MetaFilter::default()
        };
        list_until(&engine, &filter, 3).await;
        let report = engine
            .erase(EraseRequest::new(tinymemory_api::Reach::subtree(
                node.clone(),
            )))
            .await
            .expect("erase");
        assert_eq!(report.erased_scopes, 2, "{report:?}");
        assert!(
            report.receipts.iter().all(|r| r.starts_with("erasure_")),
            "{report:?}"
        );
        let left = list_exactly(&engine, &filter, &[format!("{run} kept")]).await;
        assert_eq!(
            left,
            vec![format!("{run} kept")],
            "only the sibling is left"
        );
        for erased in &items[..2] {
            let again = engine.store(erased.clone()).await.expect("store");
            assert!(!again.replayed, "an erased item stores anew");
        }
        assert_eq!(list_until(&engine, &filter, 3).await.len(), 3);
        engine
            .forget(ForgetTarget::Filter(filter))
            .await
            .expect("cleanup");
    }
}

#[tokio::test]
async fn documents_conversations_and_learnings_round_trip_into_context() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (wire, engine) in live_engines() {
        eprintln!("round trip on {wire}");
        round_trip(&engine).await;
    }
}

async fn round_trip(engine: &CortexEngine) {
    assert!(engine.health().await.is_serving(), "the server is serving");
    let workspace = run_id();

    // Document: a file read from a folder source.
    let mut doc_meta = meta(&workspace, SourceKind::Folder);
    doc_meta.folder = Some("/notes".into());
    doc_meta.file_path = Some("/notes/aurora.md".into());
    doc_meta.language = Some("en".into());
    let document = engine
        .store(StoreItem::Document {
            title: Some("Aurora plan".into()),
            body: tinymemory_api::DocumentBody::Text(
                "Project Aurora launches on Thursday from the Lisbon office.".into(),
            ),
            mime: Some("text/markdown".into()),
            meta: doc_meta,
        })
        .await
        .expect("store document");

    // Conversation: two turns, one of which called a tool.
    let mut conv_meta = meta(&workspace, SourceKind::Conversation);
    conv_meta.thread_id = Some(format!("{workspace}-thread"));
    conv_meta.agent_id = Some("orchestrator".into());
    let mut answer = Turn::new(Role::Assistant, "Booked the Lisbon venue for Thursday.");
    answer.tool_calls.push(ToolCallRef {
        name: "calendar_create".into(),
        id: Some("call-1".into()),
    });
    let conversation = engine
        .store(StoreItem::Conversation {
            turns: vec![
                Turn::new(Role::User, "Book a venue for the Aurora launch."),
                answer,
            ],
            meta: conv_meta,
        })
        .await
        .expect("store conversation");

    // Learning: explicit, from the agent, attributed to its tool call.
    let mut learn_meta = meta(&workspace, SourceKind::Agent);
    learn_meta.tool_call = Some(ToolCallRef {
        name: "memory".into(),
        id: Some("call-2".into()),
    });
    let learning = engine
        .store(StoreItem::learning(
            "The user prefers launch events in Lisbon.",
            LearningKind::Preference,
            0.9,
            learn_meta,
        ))
        .await
        .expect("store learning");

    let by_kind = |kind: ItemKind| MetaFilter {
        workspace: Some(workspace.clone()),
        kinds: vec![kind],
        ..MetaFilter::default()
    };
    let docs = list_until(engine, &by_kind(ItemKind::Document), 1).await;
    assert_eq!(docs.len(), 1, "the document lists back: {docs:?}");
    assert!(docs[0].contains("Project Aurora"));
    let convs = list_until(engine, &by_kind(ItemKind::Conversation), 1).await;
    assert_eq!(convs.len(), 1, "the conversation lists back: {convs:?}");
    assert!(
        convs[0].contains("calendar_create (call-1)"),
        "the tool call stays visible: {}",
        convs[0]
    );
    let learns = list_until(engine, &by_kind(ItemKind::Learning), 1).await;
    assert_eq!(learns.len(), 1, "the learning lists back: {learns:?}");

    // Metadata filters narrow server-side and client-side alike.
    let by_file = MetaFilter {
        workspace: Some(workspace.clone()),
        file_path: Some("/notes/aurora.md".into()),
        ..MetaFilter::default()
    };
    assert_eq!(list_until(engine, &by_file, 1).await.len(), 1);
    let by_tool = MetaFilter {
        workspace: Some(workspace.clone()),
        tool_call: Some("memory".into()),
        ..MetaFilter::default()
    };
    assert_eq!(list_until(engine, &by_tool, 1).await.len(), 1);

    // Fetch: hybrid search over the run's items finds the document.
    let mut fetch = FetchRequest::new("When does Project Aurora launch?", FetchMode::Hybrid, 10);
    fetch.filter = MetaFilter {
        workspace: Some(workspace.clone()),
        ..MetaFilter::default()
    };
    let page = engine.fetch(fetch).await.expect("fetch");
    assert!(
        page.hits.iter().any(|hit| hit.id == document.id),
        "fetch finds the document: {:?}",
        page.hits.iter().map(|hit| &hit.text).collect::<Vec<_>>()
    );

    // Recall: CortexDB's ask route answers and cites what it used.
    let mut recall = RecallRequest::new("When does Project Aurora launch?", 10);
    recall.filter = MetaFilter {
        workspace: Some(workspace.clone()),
        ..MetaFilter::default()
    };
    let answer = engine.recall(recall).await.expect("recall");
    assert!(!answer.answer.trim().is_empty(), "recall answers");
    assert!(!answer.citations.is_empty(), "recall cites its evidence");

    // context.md: compiled from the same engine, it lists the learning.
    let context = compile(engine, &ContextSpec::default())
        .await
        .expect("compile context");
    assert_eq!(context.engine, engine.descriptor().id);
    assert!(
        context
            .markdown
            .contains("The user prefers launch events in Lisbon."),
        "context.md carries the learning:\n{}",
        context.markdown
    );
    assert!(context.tokens <= ContextSpec::default().budget_tokens);

    // Forget the run.
    let report = engine
        .forget(ForgetTarget::Filter(MetaFilter {
            workspace: Some(workspace.clone()),
            ..MetaFilter::default()
        }))
        .await
        .expect("forget");
    assert_eq!(report.forgotten, 3, "all three items are forgotten");
    let _ = (conversation, learning);
}

/// A long, paged, sectioned document goes to the real server as several
/// events, each under its 1 MiB limit, and comes back whole: `get` and
/// `list` reassemble it, a `fetch` hit is one piece tagged with its page and
/// section, and `forget` removes every piece.
#[tokio::test]
async fn a_long_document_round_trips_in_pieces() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (wire, engine) in live_engines() {
        eprintln!("long document on {wire}");
        let workspace = format!("ws-long-{}", run_id());
        let pages: Vec<String> = (1..=24)
            .map(|page| {
                let filler =
                    format!("Clause {page} covers refunds and delivery terms. ").repeat(600);
                format!("# Section {page}\n\n{filler}\n")
            })
            .collect();
        let body = pages.join("\u{c}");
        // A document splits once its envelope passes the 256 KiB chunk
        // target (not the 1 MiB event limit): this one is three pieces, so
        // two boundaries are crossed. It stays under 1 MiB because CortexDB
        // 0.10.4 takes seconds to forget a ~240 KiB event (8 to 13 s
        // measured), and a longer document's forget can outlast the 60 s
        // request timeout.
        assert!(
            body.len() > 2 * 256 * 1024,
            "over twice the chunk target: {} bytes",
            body.len()
        );
        let document = StoreItem::Document {
            title: Some("Long contract".into()),
            body: tinymemory_api::DocumentBody::Text(body.clone()),
            mime: Some("application/pdf".into()),
            meta: MemoryMeta {
                workspace: Some(workspace.clone()),
                file_path: Some("/contracts/long.pdf".into()),
                ..MemoryMeta::from_source(SourceKind::File, Some("contracts".into()))
            },
        };
        let receipt = engine
            .store(document.clone())
            .await
            .expect("store in pieces");

        let filter = MetaFilter {
            workspace: Some(workspace.clone()),
            ..MetaFilter::default()
        };
        // `list` returns a chunked document only once every piece is
        // listed, so this waits for all of them, not just the first.
        let listed = list_until(&engine, &filter, 1).await;
        assert_eq!(listed.len(), 1, "one item, not one per piece");
        assert_eq!(
            listed[0],
            document.render_text(),
            "list reassembles the body"
        );
        let got = engine
            .get(tinymemory_api::GetRequest {
                ids: vec![receipt.id.clone()],
                reach: None,
            })
            .await
            .expect("get");
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].text,
            document.render_text(),
            "get reassembles the body"
        );

        // Ranking may lag the write; poll as `list_until` polls the listing.
        let mut fetch = FetchRequest::new("Clause 7 refunds and delivery", FetchMode::Hybrid, 5);
        fetch.filter = filter.clone();
        let deadline = Instant::now() + VISIBILITY;
        let hit = loop {
            let page = engine.fetch(fetch.clone()).await.expect("fetch");
            if let Some(hit) = page.hits.into_iter().find(|hit| hit.id == receipt.id) {
                break hit;
            }
            if Instant::now() >= deadline {
                panic!("fetch never found the document");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        assert!(hit.text.len() < body.len(), "a piece, not the whole");
        assert!(
            hit.meta.tags.iter().any(|tag| tag.starts_with("page:"))
                && hit
                    .meta
                    .tags
                    .iter()
                    .any(|tag| tag.starts_with("section:Section ")),
            "{:?}",
            hit.meta.tags
        );

        let report = engine
            .forget(ForgetTarget::Ids(vec![receipt.id.clone()]))
            .await
            .expect("forget");
        assert_eq!(report.forgotten, 1);
        assert!(
            engine
                .list(ListRequest::new(filter, 10))
                .await
                .expect("list after forget")
                .items
                .is_empty(),
            "the document is gone"
        );
        // `list` hides a document missing a piece, so ask ranked recall,
        // which hits single pieces: none may be left behind.
        let deadline = Instant::now() + VISIBILITY;
        loop {
            let page = engine.fetch(fetch.clone()).await.expect("fetch");
            if !page.hits.iter().any(|hit| hit.id == receipt.id) {
                break;
            }
            assert!(Instant::now() < deadline, "a piece outlived forget");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// A `service:` namespace node on the real server: CortexDB admits the
/// `service` scope type (no `422 UNREGISTERED_SCOPE_TYPE`), and the item
/// reads back at its node and is forgotten there.
#[tokio::test]
async fn a_service_node_round_trips() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (wire, engine) in live_engines() {
        let node: tinymemory_api::Namespace = format!("ws:{}/service:newsletter", run_id())
            .parse()
            .expect("a valid service node");
        let receipt = engine
            .store(StoreItem::learning(
                "Newsletter item 5531 was already sent",
                LearningKind::Fact,
                0.8,
                MemoryMeta {
                    namespace: node.clone(),
                    ..MemoryMeta::default()
                },
            ))
            .await
            .unwrap_or_else(|error| panic!("{wire} stores at a service node: {error}"));
        let filter = MetaFilter {
            reach: Some(tinymemory_api::Reach::exact(node)),
            ..MetaFilter::default()
        };
        let listed = list_until(&engine, &filter, 1).await;
        assert_eq!(listed, ["Newsletter item 5531 was already sent"], "{wire}");
        let report = engine
            .forget(ForgetTarget::Ids(vec![receipt.id]))
            .await
            .expect("forget");
        assert_eq!(report.forgotten, 1, "{wire}");
        let left = engine
            .list(ListRequest::new(filter, 50))
            .await
            .expect("list the service node again")
            .items;
        assert!(left.is_empty(), "{wire} still lists {left:?}");
    }
}

/// A capped fetch reads the scopes its query names, then the most recently
/// written: four sources, each newer than the last, read two at a time.
#[tokio::test]
async fn a_capped_fetch_reads_the_named_then_the_newest_scopes() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (wire, engine) in live_engines() {
        let root = run_id();
        let mut ids = Vec::new();
        // Stored oldest first: each write makes its scope the newest.
        for source in ["gmail", "notion", "slack", "files"] {
            let node: tinymemory_api::Namespace = format!("ws:{root}/source:{source}")
                .parse()
                .expect("a valid source node");
            let receipt = engine
                .store(StoreItem::document(
                    format!("The launch plan is kept in {source}"),
                    MemoryMeta {
                        namespace: node,
                        ..MemoryMeta::default()
                    },
                ))
                .await
                .unwrap_or_else(|error| panic!("{wire} stores in {source}: {error}"));
            ids.push(receipt.id);
        }
        let reach: tinymemory_api::Namespace = format!("ws:{root}").parse().expect("a valid root");
        let mut request =
            FetchRequest::new("where is the launch plan in gmail", FetchMode::Hybrid, 10);
        request.filter = MetaFilter {
            reach: Some(tinymemory_api::Reach::subtree(reach)),
            ..MetaFilter::kinds([ItemKind::Document])
        };
        request.max_scopes = Some(2);
        // Ranked recall may lag the writes; read until both scopes answer.
        let deadline = Instant::now() + VISIBILITY;
        let sources = loop {
            let hits = engine.fetch(request.clone()).await.expect("fetch").hits;
            let mut sources: Vec<String> = hits
                .iter()
                .filter_map(|hit| {
                    let last = hit.meta.namespace.segments().last()?;
                    Some(last.id().to_string())
                })
                .collect();
            sources.sort();
            sources.dedup();
            if sources.len() >= 2 || Instant::now() >= deadline {
                break sources;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        // `gmail` by name, `files` as the newest; never `notion` or `slack`.
        assert_eq!(sources, ["files", "gmail"], "{wire}");
        engine
            .forget(ForgetTarget::Ids(ids))
            .await
            .expect("forget the run's documents");
    }
}

/// A turn logged on the hot path (one single-turn conversation, accepted
/// only) skips the lookup, so a retry is caught by CortexDB itself: the
/// same body is the same idempotency key, answered as a replay of the
/// first event. The hosted wire always looks up, so this is Direct only.
#[tokio::test]
async fn a_logged_turn_sent_twice_is_written_once() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (wire, engine) in live_engines() {
        if wire != "cortexdb" {
            continue;
        }
        let thread = run_id();
        let turn = StoreItem::Conversation {
            turns: vec![Turn::new(
                Role::User,
                format!("Ship the Aurora build on Friday ({thread})."),
            )],
            meta: MemoryMeta {
                thread_id: Some(thread.clone()),
                ..MemoryMeta::default()
            },
        };
        let first = engine
            .store_with(turn.clone(), tinymemory_api::WriteOptions::accepted())
            .await
            .expect("log the turn");
        assert!(!first.replayed);
        let retry = engine
            .store_with(turn, tinymemory_api::WriteOptions::accepted())
            .await
            .expect("log it again");
        assert!(retry.replayed, "CortexDB answered the retry as a replay");
        assert_eq!(retry.id, first.id);

        let filter = MetaFilter {
            thread_id: Some(thread),
            ..MetaFilter::default()
        };
        let listed = list_until(&engine, &filter, 1).await;
        assert_eq!(listed.len(), 1, "one conversation");
        let report = engine
            .forget(ForgetTarget::Ids(vec![first.id]))
            .await
            .expect("forget");
        assert_eq!(report.forgotten, 1);
    }
}

/// A fetch hit on a conversation carries the whole conversation on the real
/// server: a one-turn conversation (as a host logs each turn) taken from its
/// pack event alone, and a longer one assembled from its turns. Each hit's
/// text matches a whole-item `get` and the item as stored.
#[tokio::test]
async fn a_fetch_hit_is_the_whole_conversation_one_turn_or_longer() {
    let _alone = ONE_AT_A_TIME.lock().await;
    for (wire, engine) in live_engines() {
        let workspace = run_id();
        let said = |turn: usize| format!("Turn {turn}: the Kestrel launch moved ({workspace}).");
        let conversation = |turns: usize, thread: &str| StoreItem::Conversation {
            turns: (0..turns)
                .map(|turn| {
                    let role = if turn % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    };
                    Turn::new(role, said(turn))
                })
                .collect(),
            meta: MemoryMeta {
                thread_id: Some(format!("{workspace}-{thread}")),
                ..meta(&workspace, SourceKind::Conversation)
            },
        };
        let items = vec![conversation(1, "one"), conversation(3, "three")];
        let receipts = engine
            .store_many(items.clone())
            .await
            .expect("store the conversations");
        let filter = MetaFilter {
            workspace: Some(workspace.clone()),
            ..MetaFilter::kinds([ItemKind::Conversation])
        };
        assert_eq!(list_until(&engine, &filter, 2).await.len(), 2, "{wire}");

        let deadline = Instant::now() + VISIBILITY;
        let hits = loop {
            let mut request =
                FetchRequest::new(format!("Kestrel launch {workspace}"), FetchMode::Hybrid, 10);
            request.filter = filter.clone();
            let hits = engine.fetch(request).await.expect("fetch").hits;
            if hits.len() >= 2 || Instant::now() >= deadline {
                break hits;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        let mut found: Vec<&str> = hits.iter().map(|hit| hit.id.as_str()).collect();
        let mut stored: Vec<&str> = receipts.iter().map(|receipt| receipt.id.as_str()).collect();
        found.sort_unstable();
        stored.sort_unstable();
        assert_eq!(found, stored, "{wire}: one hit for each conversation");
        let whole = engine
            .get(tinymemory_api::GetRequest {
                ids: hits.iter().map(|hit| hit.id.clone()).collect(),
                reach: None,
            })
            .await
            .expect("get");
        for hit in &hits {
            let item = items
                .iter()
                .find(|item| item.fingerprint() == hit.id.as_str())
                .expect("a stored item");
            assert_eq!(
                hit.text,
                item.render_text(),
                "{wire}: the whole conversation"
            );
            let read = whole.iter().find(|read| read.id == hit.id).expect("read");
            assert_eq!(hit.text, read.text, "{wire}");
            assert_eq!(hit.meta, read.meta, "{wire}");
        }
        engine
            .forget(ForgetTarget::Ids(
                receipts.into_iter().map(|receipt| receipt.id).collect(),
            ))
            .await
            .expect("forget");
    }
}

/// What recall's dropped-pack retry (`engine/recall.rs`) is built on, pinned
/// against the real server: a forget anywhere, even in an unrelated scope,
/// drops every pack CortexDB holds, so the next `use_pack_id` answer is a
/// 404, and a pack built after the forget answers. Direct only: the hosted
/// wire's answer route sits behind the TinyHumans backend.
#[tokio::test]
async fn a_forget_anywhere_drops_every_pack_the_server_holds() {
    let _alone = ONE_AT_A_TIME.lock().await;
    let Ok(url) = std::env::var("TINYMEMORY_LIVE_CORTEXDB_URL") else {
        return;
    };
    let url = url.trim_end_matches('/').to_string();
    let key = std::env::var("TINYMEMORY_TEST_CORTEX_KEY").unwrap_or_else(|_| DEFAULT_KEY.into());
    let http = reqwest::Client::new();
    let post = |path: &str, body: serde_json::Value| {
        http.post(format!("{url}/{path}"))
            .bearer_auth(&key)
            .json(&body)
            .send()
    };
    let run = run_id();
    let (kept, other) = (
        format!("app:tinymemory-probe/agent:{run}-kept/app:learnings"),
        format!("app:tinymemory-probe/agent:{run}-other/app:learnings"),
    );
    let write = |scope: &str, text: &str| {
        serde_json::json!({
            "scope": scope, "modality": "observation", "idempotency_key": format!("{run}-{text}"),
            "content": { "kind": "message", "role": "user", "text": text },
        })
    };
    let written: serde_json::Value = post(
        "v1/experience?wait=indexed",
        write(&other, "to be forgotten"),
    )
    .await
    .expect("write")
    .json()
    .await
    .expect("receipt");
    post(
        "v1/experience?wait=indexed",
        write(&kept, "The office closes at six on Fridays."),
    )
    .await
    .expect("write");
    let pack = || async {
        let pack: serde_json::Value = post(
            "v1/recall",
            serde_json::json!({ "scope": kept, "query": "office", "view": "granular" }),
        )
        .await
        .expect("recall")
        .json()
        .await
        .expect("pack");
        pack["pack_id"].as_str().expect("a pack id").to_string()
    };
    let answer = |pack_id: String| {
        post(
            "v1/answer",
            serde_json::json!({ "scope": kept, "question": "When does the office close?",
                                "use_pack_id": pack_id, "answer_instructions": null }),
        )
    };
    let before = pack().await;
    let forgot = post(
        "v1/forget",
        serde_json::json!({ "scope": other, "selector": { "memory_ids": [written["event_id"]] } }),
    )
    .await
    .expect("forget");
    assert!(forgot.status().is_success(), "{}", forgot.status());
    let dropped = answer(before).await.expect("answer");
    assert_eq!(
        dropped.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a forget elsewhere drops the pack"
    );
    let fresh = answer(pack().await).await.expect("answer");
    assert!(
        fresh.status().is_success(),
        "a pack built after the forget answers: {}",
        fresh.status()
    );
}

/// A model's `refers_to` on `memory_fetch` reaches the live server as
/// `refers_during` and ranks that local day first: the tool JSON, the host's
/// zone, the engine's capability gate and the server's boost end to end.
#[tokio::test]
async fn a_models_refers_to_ranks_that_day_first_on_the_live_server() {
    let _alone = ONE_AT_A_TIME.lock().await;
    use chrono::{TimeZone, Utc};
    use tinymemory_tools::MemoryTools;

    for (name, engine) in live_engines() {
        let workspace = run_id();
        let engine = Arc::new(engine);
        // 20:00 UTC is already the next day in India.
        for (text, day) in [("Toit", 1), ("Truffles", 2), ("home", 5)] {
            let mut item_meta = meta(&workspace, SourceKind::Folder);
            item_meta.observed_at = Utc.with_ymd_and_hms(2026, 10, day, 20, 0, 0).single();
            engine
                .store(StoreItem::document(
                    format!("{workspace} dinner at {text}"),
                    item_meta,
                ))
                .await
                .expect("store");
        }
        let filter = MetaFilter {
            workspace: Some(workspace.clone()),
            ..MetaFilter::default()
        };
        assert_eq!(
            list_until(&engine, &filter, 3).await.len(),
            3,
            "{name}: fixture visible"
        );

        let tools = MemoryTools::new(engine.clone()).in_zone("Asia/Kolkata");
        let page = tools
            .call(
                "memory_fetch",
                serde_json::json!({
                    "query": format!("{workspace} dinner"),
                    "filter": { "workspace": workspace },
                    "refers_to": { "from": "2026-10-03", "to": "2026-10-03" },
                }),
            )
            .await
            .expect("memory_fetch");
        let first = page["hits"][0]["text"].as_str().unwrap_or_default();
        assert!(
            first.contains("Truffles"),
            "{name}: the hinted day leads: {page}"
        );
    }
}
