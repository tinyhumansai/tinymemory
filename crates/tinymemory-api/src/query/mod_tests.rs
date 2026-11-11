//! Request validation and wire shapes.

use super::*;

#[test]
fn malformed_requests_are_refused() {
    assert!(RecallRequest::new(" ", 3).validate().is_err());
    assert!(RecallRequest::new("q", 0).validate().is_err());
    assert!(RecallRequest::new("q", 1).validate().is_ok());
    assert!(
        FetchRequest::new("", FetchMode::Hybrid, 3)
            .validate()
            .is_err()
    );
    assert!(
        FetchRequest::new("q", FetchMode::Hybrid, 0)
            .validate()
            .is_err()
    );
    assert!(
        FetchRequest::new("q", FetchMode::Keyword, 2)
            .validate()
            .is_ok()
    );
    assert!(
        ListRequest::new(MetaFilter::default(), 0)
            .validate()
            .is_err()
    );
    assert!(
        ListRequest::new(MetaFilter::default(), 1)
            .validate()
            .is_ok()
    );
}

#[test]
fn a_forget_can_never_mean_everything() {
    assert!(matches!(
        ForgetTarget::Ids(Vec::new()).validate(),
        Err(Error::InvalidRequest(_))
    ));
    assert!(matches!(
        ForgetTarget::Filter(MetaFilter::default()).validate(),
        Err(Error::InvalidRequest(_))
    ));
    assert!(
        ForgetTarget::Ids(vec![ItemId::from("a")])
            .validate()
            .is_ok()
    );
    assert!(
        ForgetTarget::Filter(MetaFilter::kinds([ItemKind::Learning]))
            .validate()
            .is_ok()
    );
}

#[test]
fn fetch_mode_wire_strings_match_serde() {
    for mode in FetchMode::ALL {
        assert_eq!(serde_json::to_value(mode).expect("json"), mode.as_str());
    }
}

#[test]
fn requests_deserialise_with_defaults() {
    let request: FetchRequest =
        serde_json::from_str(r#"{"query":"q","mode":"hybrid","limit":2}"#).expect("parse");
    assert!(request.filter.is_empty());
    assert_eq!(request.cursor, None);
}

#[test]
fn a_fetch_reads_every_scope_unless_capped_and_never_zero() {
    let mut request = FetchRequest::new("q", FetchMode::Hybrid, 3);
    assert_eq!(request.max_scopes, None);
    request.max_scopes = Some(4);
    assert!(request.validate().is_ok());
    request.max_scopes = Some(0);
    assert!(request.validate().is_err());
    // An uncapped request keeps its old wire shape.
    let json = serde_json::to_value(FetchRequest::new("q", FetchMode::Hybrid, 3)).unwrap();
    assert!(json.get("max_scopes").is_none());
}

#[test]
fn a_thread_window_covers_its_thread_from_its_first_turn_on() {
    let turn = |thread: Option<&str>, turns: Option<(u32, u32)>| MemoryMeta {
        thread_id: thread.map(str::to_string),
        turns: turns.map(|(first, last)| crate::meta::TurnRange { first, last }),
        ..MemoryMeta::default()
    };
    let whole = ThreadWindow::whole("t1");
    assert!(whole.covers_meta(&turn(Some("t1"), Some((0, 0)))));
    assert!(
        whole.covers_meta(&turn(Some("t1"), None)),
        "an item without turns is the thread's"
    );
    assert!(!whole.covers_meta(&turn(Some("t2"), Some((0, 0)))));
    assert!(!whole.covers_meta(&turn(None, None)));
    let in_prompt = ThreadWindow {
        thread_id: "t1".into(),
        from_turn: 4,
    };
    assert!(
        !in_prompt.covers_meta(&turn(Some("t1"), Some((0, 3)))),
        "compacted turns stay recallable"
    );
    assert!(in_prompt.covers_meta(&turn(Some("t1"), Some((2, 4)))));
}

#[test]
fn reads_refuse_a_blank_excluded_thread() {
    let mut recall = RecallRequest::new("q", 5);
    recall.exclude_thread = Some(ThreadWindow::whole(""));
    assert!(matches!(recall.validate(), Err(Error::InvalidRequest(_))));
    recall.exclude_thread = Some(ThreadWindow::whole("t1"));
    assert!(recall.validate().is_ok());
    let mut fetch = FetchRequest::new("q", FetchMode::Hybrid, 5);
    fetch.exclude_thread = Some(ThreadWindow::whole("  "));
    assert!(matches!(fetch.validate(), Err(Error::InvalidRequest(_))));
}

#[test]
fn a_read_without_an_excluded_thread_serializes_as_before() {
    let json = serde_json::to_value(RecallRequest::new("q", 5)).unwrap();
    assert!(json.get("exclude_thread").is_none());
    let parsed: FetchRequest =
        serde_json::from_value(serde_json::json!({"query": "q", "mode": "hybrid", "limit": 5}))
            .unwrap();
    assert_eq!(parsed.exclude_thread, None);
}

#[test]
fn a_thread_window_excludes_conversation_turns_only() {
    let meta = MemoryMeta {
        thread_id: Some("t1".into()),
        ..MemoryMeta::default()
    };
    let window = ThreadWindow::whole("t1");
    assert!(window.excludes(ItemKind::Conversation, &meta));
    assert!(
        !window.excludes(ItemKind::Learning, &meta),
        "a learning stamped with the thread stays readable"
    );
    assert!(!window.excludes(ItemKind::Document, &meta));
}
