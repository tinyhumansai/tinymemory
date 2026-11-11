//! Holistic recall against the reference engine and a half-broken one.

use async_trait::async_trait;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{
    EngineDescriptor, EngineHealth, Error, FetchPage, FetchRequest, ForgetReport, ForgetTarget,
    ItemKind, LearningKind, ListPage, ListRequest, MemoryMeta, MetaFilter, Namespace, Reach,
    RecallAnswer, RecallRequest, Role, StoreItem, StoreReceipt, Turn, TurnRange,
};

use super::*;

fn turn(thread: &str, index: u32, text: &str) -> StoreItem {
    StoreItem::Conversation {
        turns: vec![Turn::new(Role::User, text)],
        meta: MemoryMeta {
            thread_id: Some(thread.to_string()),
            turns: Some(TurnRange {
                first: index,
                last: index,
            }),
            ..MemoryMeta::default()
        },
    }
}

async fn seeded() -> ReferenceEngine {
    let engine = ReferenceEngine::new();
    for item in [
        StoreItem::document("Refunds take five business days.", MemoryMeta::default()),
        StoreItem::document("Deploys happen on Fridays.", MemoryMeta::default()),
        StoreItem::learning(
            "Customers prefer refunds by email",
            LearningKind::Fact,
            0.9,
            MemoryMeta::default(),
        ),
        turn("t1", 0, "asked about refunds yesterday"),
        turn("t2", 4, "refunds question in this very thread"),
    ] {
        engine.store(item).await.unwrap();
    }
    engine
}

fn docs() -> MetaFilter {
    MetaFilter::kinds([ItemKind::Document])
}

#[tokio::test]
async fn fetch_sections_rank_for_the_pack_query() {
    let engine = seeded().await;
    let request = HolisticRecall::new(
        Some("refunds".into()),
        vec![ScopeSection::fetch("Docs", docs(), 1)],
    );
    let pack = holistic_recall(&engine, &request).await.unwrap();
    assert_eq!(
        pack.markdown,
        "# Memory\n\n## Docs\n\n- Refunds take five business days.\n"
    );
    assert_eq!(pack.sections.len(), 1);
    assert_eq!(pack.sections[0].hits.len(), 1);
    assert_eq!(pack.refs.len(), 1);
    assert_eq!(pack.engine, "reference");
    assert!(pack.skipped.is_empty());
}

#[tokio::test]
async fn a_fetch_section_without_any_query_reads_the_latest() {
    let engine = seeded().await;
    let request = HolisticRecall::new(None, vec![ScopeSection::fetch("Docs", docs(), 5)]);
    let pack = holistic_recall(&engine, &request).await.unwrap();
    assert_eq!(pack.sections[0].hits.len(), 2);
}

#[tokio::test]
async fn a_section_s_own_query_overrides_the_pack_s() {
    let engine = seeded().await;
    let mut section = ScopeSection::fetch("Docs", docs(), 1);
    section.query = SectionQuery::Fetch {
        query: Some("deploys fridays".into()),
    };
    let pack = holistic_recall(
        &engine,
        &HolisticRecall::new(Some("refunds".into()), vec![section]),
    )
    .await
    .unwrap();
    assert!(pack.markdown.contains("Deploys happen on Fridays."));
}

#[tokio::test]
async fn answered_sections_are_prose_with_citations() {
    let engine = seeded().await;
    let request = HolisticRecall::new(
        None,
        vec![ScopeSection::answer(
            "Refunds",
            "how long do refunds take",
            docs(),
            3,
        )],
    );
    let pack = holistic_recall(&engine, &request).await.unwrap();
    assert!(
        pack.markdown.contains("## Refunds\n\nFrom "),
        "{}",
        pack.markdown
    );
    assert!(pack.sections[0].answer.is_some());
    assert!(!pack.refs.is_empty());
}

#[tokio::test]
async fn excluded_ids_and_the_live_thread_window_are_left_out() {
    let engine = seeded().await;
    let conversations = MetaFilter::kinds([ItemKind::Conversation]);
    let mut request = HolisticRecall::new(
        Some("refunds".into()),
        vec![ScopeSection::fetch("History", conversations, 5)],
    );
    request.exclude_thread = Some(ThreadWindow {
        thread_id: "t2".into(),
        from_turn: 3,
    });
    let pack = holistic_recall(&engine, &request).await.unwrap();
    assert!(pack.markdown.contains("asked about refunds yesterday"));
    assert!(!pack.markdown.contains("this very thread"));

    request.exclude_thread = Some(ThreadWindow {
        thread_id: "t2".into(),
        from_turn: 5,
    });
    let older = holistic_recall(&engine, &request).await.unwrap();
    assert!(
        older.markdown.contains("this very thread"),
        "a turn before the window is no longer in the prompt"
    );

    request.exclude_thread = None;
    request.exclude_ids = older.refs.clone();
    let none = holistic_recall(&engine, &request).await.unwrap();
    assert!(none.is_empty());
    assert_eq!(none.skipped[0].reason, SkipReason::Empty);
}

/// The reference engine with recall (and optionally fetch) broken.
struct Broken {
    inner: ReferenceEngine,
    fetch_too: bool,
    recall_error: Error,
}

#[async_trait]
impl tinymemory_api::MemoryEngine for Broken {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }
    async fn health(&self) -> EngineHealth {
        EngineHealth::Ok
    }
    async fn recall(&self, _req: RecallRequest) -> tinymemory_api::Result<RecallAnswer> {
        Err(self.recall_error.clone())
    }
    async fn fetch(&self, req: FetchRequest) -> tinymemory_api::Result<FetchPage> {
        if self.fetch_too {
            return Err(Error::Unavailable("fetch is down".into()));
        }
        self.inner.fetch(req).await
    }
    async fn store(&self, item: StoreItem) -> tinymemory_api::Result<StoreReceipt> {
        self.inner.store(item).await
    }
    async fn forget(&self, target: ForgetTarget) -> tinymemory_api::Result<ForgetReport> {
        self.inner.forget(target).await
    }
    async fn list(&self, req: ListRequest) -> tinymemory_api::Result<ListPage> {
        self.inner.list(req).await
    }
}

#[tokio::test]
async fn a_failed_answer_falls_back_to_fetch_only_when_asked() {
    let engine = Broken {
        inner: seeded().await,
        fetch_too: false,
        recall_error: Error::Unavailable("recall is down".into()),
    };
    let mut section = ScopeSection::answer("Refunds", "refunds", docs(), 3);
    let plain = holistic_recall(&engine, &HolisticRecall::new(None, vec![section.clone()]))
        .await
        .unwrap();
    assert!(plain.is_empty());
    assert!(matches!(
        &plain.skipped[0].reason,
        SkipReason::Engine(Error::Unavailable(message)) if message == "recall is down"
    ));

    if let SectionQuery::Answer {
        fallback_to_fetch, ..
    } = &mut section.query
    {
        *fallback_to_fetch = true;
    }
    let fallen = holistic_recall(&engine, &HolisticRecall::new(None, vec![section]))
        .await
        .unwrap();
    assert!(
        fallen
            .markdown
            .contains("- Refunds take five business days.")
    );
}

#[tokio::test]
async fn a_failing_section_never_fails_the_pack() {
    let engine = Broken {
        inner: seeded().await,
        fetch_too: true,
        recall_error: Error::Unauthorized("credential expired".into()),
    };
    let request = HolisticRecall::new(
        Some("refunds".into()),
        vec![
            ScopeSection::fetch("Docs", docs(), 3),
            ScopeSection::latest("Learnings", MetaFilter::kinds([ItemKind::Learning]), 3),
        ],
    );
    let pack = holistic_recall(&engine, &request).await.unwrap();
    assert_eq!(pack.skipped.len(), 1);
    assert_eq!(pack.skipped[0].heading, "Docs");
    assert!(matches!(
        &pack.skipped[0].reason,
        SkipReason::Engine(error @ Error::Unavailable(message))
            if message == "fetch is down" && !error.is_account_wide()
    ));
    assert!(pack.markdown.contains("## Learnings"));
}

#[tokio::test]
async fn account_wide_errors_keep_their_typed_cause_when_a_recall_section_is_skipped() {
    let errors = [
        Error::Off("opaque off detail".into()),
        Error::InsufficientCredits("opaque credit detail".into()),
        Error::Unauthorized("opaque auth detail".into()),
    ];

    for error in errors {
        let engine = Broken {
            inner: seeded().await,
            fetch_too: false,
            recall_error: error.clone(),
        };
        let request = HolisticRecall::new(
            None,
            vec![ScopeSection::answer("Account", "question", docs(), 1)],
        );
        let pack = holistic_recall(&engine, &request).await.unwrap();

        assert_eq!(pack.skipped[0].reason, SkipReason::Engine(error.clone()));
        assert!(error.is_account_wide(), "{error:?}");
    }
}

#[tokio::test]
async fn sections_read_only_their_scope() {
    let engine = ReferenceEngine::new();
    for (text, namespace) in [
        ("pdf fact about refunds", Namespace::source("pdf")),
        ("agent note about refunds", Namespace::agent("a")),
    ] {
        engine
            .store(StoreItem::document(
                text,
                MemoryMeta {
                    namespace,
                    ..MemoryMeta::default()
                },
            ))
            .await
            .unwrap();
    }
    let pdf_only = MetaFilter {
        reach: Some(Reach::subtree(Namespace::source("pdf"))),
        ..docs()
    };
    let pack = holistic_recall(
        &engine,
        &HolisticRecall::new(
            Some("refunds".into()),
            vec![ScopeSection::fetch("Pdf", pdf_only, 5)],
        ),
    )
    .await
    .unwrap();
    assert!(pack.markdown.contains("pdf fact"));
    assert!(!pack.markdown.contains("agent note"));
}

#[tokio::test]
async fn an_invalid_request_is_refused() {
    let engine = ReferenceEngine::new();
    let cases = [
        HolisticRecall {
            budget_tokens: 0,
            ..HolisticRecall::new(None, Vec::new())
        },
        HolisticRecall {
            title: " ".into(),
            ..HolisticRecall::new(None, Vec::new())
        },
        HolisticRecall::new(None, vec![ScopeSection::fetch(" ", docs(), 1)]),
        HolisticRecall::new(None, vec![ScopeSection::fetch("Docs", docs(), 0)]),
        HolisticRecall::new(None, vec![ScopeSection::answer("Docs", " ", docs(), 1)]),
    ];
    for request in cases {
        let error = holistic_recall(&engine, &request).await.unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(_)), "{error:?}");
    }
}

#[tokio::test]
async fn an_item_is_listed_once_in_its_first_section() {
    let engine = seeded().await;
    let request = HolisticRecall::new(
        Some("refunds".into()),
        vec![
            ScopeSection::fetch("Docs", docs(), 1),
            ScopeSection::fetch("Everything", MetaFilter::default(), 10),
        ],
    );
    let pack = holistic_recall(&engine, &request).await.unwrap();
    assert_eq!(
        pack.markdown
            .matches("Refunds take five business days.")
            .count(),
        1,
        "{}",
        pack.markdown
    );
    assert!(
        pack.sections[1]
            .hits
            .iter()
            .all(|hit| hit.id != pack.sections[0].hits[0].id)
    );
}

#[tokio::test]
async fn a_titled_document_is_one_readable_bullet() {
    let engine = ReferenceEngine::new();
    for (title, body) in [
        ("Onboarding", "# Onboarding\n\nReply within four hours."),
        ("Refunds", "Refunds take five days."),
        ("Billing", "# Billing disputes\n\nGo to finance."),
    ] {
        engine
            .store(StoreItem::Document {
                title: Some(title.into()),
                body: tinymemory_api::DocumentBody::Text(body.into()),
                mime: None,
                meta: MemoryMeta::default(),
            })
            .await
            .unwrap();
    }
    let pack = holistic_recall(
        &engine,
        &HolisticRecall::new(None, vec![ScopeSection::latest("Docs", docs(), 5)]),
    )
    .await
    .unwrap();
    let md = &pack.markdown;
    assert!(
        md.contains("- Onboarding: Reply within four hours.\n"),
        "{md}"
    );
    assert!(md.contains("- Refunds: Refunds take five days.\n"), "{md}");
    assert!(
        md.contains("- Billing: # Billing disputes Go to finance.\n"),
        "{md}"
    );
}

#[tokio::test]
async fn a_timed_turn_is_led_by_when_it_was_said() {
    let engine = ReferenceEngine::new();
    let at = "2026-09-15T09:01:30Z".parse().unwrap();
    for item in [
        StoreItem::Conversation {
            turns: vec![Turn::new(Role::User, "The budget is now 6500 dollars.")],
            meta: MemoryMeta {
                observed_at: Some(at),
                ..MemoryMeta::default()
            },
        },
        turn("t1", 0, "The budget is 5000 dollars."),
    ] {
        engine.store(item).await.unwrap();
    }
    let pack = holistic_recall(
        &engine,
        &HolisticRecall::new(
            None,
            vec![ScopeSection::latest(
                "History",
                MetaFilter::kinds([ItemKind::Conversation]),
                5,
            )],
        ),
    )
    .await
    .unwrap();
    let md = &pack.markdown;
    assert!(
        md.contains("- [2026-09-15 09:01] user: The budget is now 6500 dollars.\n"),
        "{md}"
    );
    assert!(
        md.contains("- user: The budget is 5000 dollars.\n"),
        "an untimed turn has no date: {md}"
    );
}

/// The reference engine plus beliefs kept apart from its items, or a belief
/// read that fails. Fetch returns them when asked; it records every belief
/// listing and every fetch's belief budget.
struct Believer {
    inner: ReferenceEngine,
    beliefs: Option<Vec<&'static str>>,
    asked: std::sync::Mutex<Vec<tinymemory_api::BeliefsRequest>>,
    budgets: std::sync::Mutex<Vec<usize>>,
}

impl Believer {
    async fn new(beliefs: Option<Vec<&'static str>>) -> Self {
        Self {
            inner: seeded().await,
            beliefs,
            asked: std::sync::Mutex::new(Vec::new()),
            budgets: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn held(&self, limit: usize) -> tinymemory_api::Result<Vec<tinymemory_api::Hit>> {
        let Some(texts) = &self.beliefs else {
            return Err(Error::Unavailable("beliefs are down".into()));
        };
        Ok(texts
            .iter()
            .map(|text| tinymemory_api::Hit {
                id: tinymemory_api::ItemId::new(format!("belief:{text}")),
                kind: ItemKind::Learning,
                text: (*text).to_string(),
                meta: MemoryMeta {
                    tags: vec![tinymemory_api::BELIEF_TAG.to_string()],
                    ..MemoryMeta::default()
                },
                score: 0.0,
                confidence: Some(0.9),
            })
            .take(limit)
            .collect())
    }
}

#[async_trait]
impl tinymemory_api::MemoryEngine for Believer {
    fn descriptor(&self) -> &EngineDescriptor {
        self.inner.descriptor()
    }
    async fn health(&self) -> EngineHealth {
        EngineHealth::Ok
    }
    async fn recall(&self, req: RecallRequest) -> tinymemory_api::Result<RecallAnswer> {
        self.inner.recall(req).await
    }
    async fn fetch(&self, req: FetchRequest) -> tinymemory_api::Result<FetchPage> {
        self.budgets.lock().unwrap().push(req.beliefs);
        let wanted = req.beliefs;
        let mut page = self.inner.fetch(req).await?;
        if wanted > 0 {
            page.beliefs = self.held(wanted).unwrap_or_default();
        }
        Ok(page)
    }
    async fn store(&self, item: StoreItem) -> tinymemory_api::Result<StoreReceipt> {
        self.inner.store(item).await
    }
    async fn forget(&self, target: ForgetTarget) -> tinymemory_api::Result<ForgetReport> {
        self.inner.forget(target).await
    }
    async fn list(&self, req: ListRequest) -> tinymemory_api::Result<ListPage> {
        self.inner.list(req).await
    }
    async fn beliefs(
        &self,
        req: tinymemory_api::BeliefsRequest,
    ) -> tinymemory_api::Result<Vec<tinymemory_api::Hit>> {
        self.asked.lock().unwrap().push(req.clone());
        self.held(req.limit)
    }
}

fn learnings_section(heading: &str) -> ScopeSection {
    ScopeSection::fetch(heading, MetaFilter::kinds([ItemKind::Learning]), 5)
}

#[tokio::test]
async fn a_learnings_section_merges_the_engine_s_beliefs() {
    let engine = Believer::new(Some(vec!["user prefers pnpm over npm"])).await;
    let pack = holistic_recall(
        &engine,
        &HolisticRecall::new(
            Some("refunds".into()),
            vec![
                learnings_section("Learnings"),
                ScopeSection::fetch("Docs", docs(), 5),
            ],
        ),
    )
    .await
    .unwrap();
    let md = &pack.markdown;
    assert!(md.contains("- Customers prefer refunds by email\n"), "{md}");
    assert!(md.contains("- user prefers pnpm over npm\n"), "{md}");
    assert!(
        md.find("Customers prefer").unwrap() < md.find("user prefers pnpm").unwrap(),
        "stored learnings lead at each rank: {md}"
    );
    assert!(
        engine.asked.lock().unwrap().is_empty(),
        "with a query, beliefs come from the fetches, not a separate read"
    );
    assert_eq!(
        *engine.budgets.lock().unwrap(),
        [5, 5],
        "every fetch asks for what the learnings section wants"
    );
}

#[tokio::test]
async fn beliefs_another_section_read_land_in_the_learnings() {
    let engine = Believer::new(Some(vec!["user prefers pnpm over npm"])).await;
    let pack = holistic_recall(
        &engine,
        &HolisticRecall::new(
            Some("refunds".into()),
            vec![
                ScopeSection::fetch("Docs", docs(), 5),
                learnings_section("Learnings"),
            ],
        ),
    )
    .await
    .unwrap();
    let learnings = pack
        .sections
        .iter()
        .find(|section| section.heading == "Learnings")
        .unwrap();
    assert_eq!(
        learnings
            .hits
            .iter()
            .filter(|hit| hit.text == "user prefers pnpm over npm")
            .count(),
        1,
        "each belief once, in the learnings: {}",
        pack.markdown
    );
    assert!(
        !pack.sections[0]
            .hits
            .iter()
            .any(|hit| hit.text.contains("pnpm")),
        "beliefs never land in a section that does not read learnings"
    );
}

#[tokio::test]
async fn a_pack_without_learnings_asks_for_no_beliefs() {
    let engine = Believer::new(Some(vec!["user prefers pnpm over npm"])).await;
    holistic_recall(
        &engine,
        &HolisticRecall::new(
            Some("refunds".into()),
            vec![ScopeSection::fetch("Docs", docs(), 5)],
        ),
    )
    .await
    .unwrap();
    assert_eq!(*engine.budgets.lock().unwrap(), [0]);
    assert!(engine.asked.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_latest_learnings_section_reads_beliefs_without_a_query() {
    let engine = Believer::new(Some(vec!["user prefers pnpm over npm"])).await;
    let pack = holistic_recall(
        &engine,
        &HolisticRecall::new(
            None,
            vec![ScopeSection::latest(
                "Learnings",
                MetaFilter::kinds([ItemKind::Learning]),
                5,
            )],
        ),
    )
    .await
    .unwrap();
    assert!(pack.markdown.contains("- user prefers pnpm over npm\n"));
    assert_eq!(engine.asked.lock().unwrap()[0].query, None);
}

#[tokio::test]
async fn a_failed_belief_listing_leaves_the_stored_learnings() {
    let engine = Believer::new(None).await;
    let pack = holistic_recall(
        &engine,
        &HolisticRecall::new(
            None,
            vec![ScopeSection::latest(
                "Learnings",
                MetaFilter::kinds([ItemKind::Learning]),
                5,
            )],
        ),
    )
    .await
    .unwrap();
    assert!(
        pack.markdown
            .contains("- Customers prefer refunds by email\n")
    );
    assert!(pack.skipped.is_empty(), "{:?}", pack.skipped);
}

#[tokio::test]
async fn a_belief_the_filter_rules_out_is_left_out() {
    let engine = Believer::new(Some(vec!["user prefers pnpm over npm"])).await;
    let mut section = learnings_section("Learnings");
    section.filter.thread_id = Some("t1".into());
    let pack = holistic_recall(
        &engine,
        &HolisticRecall::new(Some("refunds".into()), vec![section]),
    )
    .await
    .unwrap();
    assert!(!pack.markdown.contains("pnpm"), "{}", pack.markdown);
}

/// Hosts run recall on multi-threaded runtimes (a spawned pre-turn, an
/// `async_trait` method), which need its future to be `Send`. Every
/// reference it holds across an `.await` must therefore be `Sync`,
/// including the `keep` predicate the gathering passes down.
#[test]
fn a_recall_future_can_cross_threads() {
    fn send<T: Send>(_: &T) {}
    let engine = ReferenceEngine::new();
    let request = HolisticRecall::new(
        Some("refunds".into()),
        vec![ScopeSection::fetch("Docs", docs(), 5)],
    );
    let future = holistic_recall(&engine, &request);
    send(&future);
}

#[tokio::test]
async fn a_ranked_section_reads_past_a_page_the_thread_window_empties() {
    // Eight turns of one thread all match the query, and the prompt still
    // holds turns 2..7, which rank first. The section wants 1 hit, so its
    // first page (limit plus window allowance = 2) is entirely in-window, as
    // are the next two. The section must read on (within its page cap) and
    // find turn 0 or 1, not come back empty.
    let engine = ReferenceEngine::new();
    for turn in 0..8u32 {
        let meta = MemoryMeta {
            thread_id: Some("tw".into()),
            turns: Some(TurnRange {
                first: turn,
                last: turn,
            }),
            ..MemoryMeta::default()
        };
        engine
            .store(StoreItem::Conversation {
                turns: vec![Turn::new(
                    Role::User,
                    if turn >= 2 {
                        // In-window turns rank first: the query terms, repeated.
                        format!("turn {turn}: Porto refund, Porto refund, Porto refund")
                    } else {
                        format!("turn {turn} asks about the Porto refund")
                    },
                )],
                meta,
            })
            .await
            .unwrap();
    }
    let mut request = HolisticRecall::new(
        Some("Porto refund".into()),
        vec![ScopeSection::fetch(
            "History",
            MetaFilter::kinds([ItemKind::Conversation]),
            1,
        )],
    );
    request.exclude_thread = Some(ThreadWindow {
        thread_id: "tw".into(),
        from_turn: 2,
    });
    let pack = holistic_recall(&engine, &request).await.unwrap();
    assert!(!pack.is_empty(), "skipped: {:?}", pack.skipped);
    assert!(
        pack.markdown.contains("turn 0 ") || pack.markdown.contains("turn 1 "),
        "{}",
        pack.markdown
    );
}

#[test]
fn a_section_reading_zero_scopes_is_refused() {
    let section = ScopeSection::fetch("Docs", docs(), 5);
    assert_eq!(section.max_scopes, None);
    assert_eq!(section.clone().with_max_scopes(4).max_scopes, Some(4));
    let request = HolisticRecall::new(Some("q".into()), vec![section.with_max_scopes(0)]);
    assert!(request.validate().is_err());
}
