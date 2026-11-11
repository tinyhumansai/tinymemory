//! The read tools against the reference engine, and id-list reading.

use super::*;
use serde_json::json;
use tinymemory_api::conformance::ReferenceEngine;
use tinymemory_api::{LearningKind, MemoryMeta, Namespace, StoreItem};

async fn engine_with(items: &[(&str, Namespace)]) -> (ReferenceEngine, Vec<ItemId>) {
    let engine = ReferenceEngine::new();
    let mut ids = Vec::new();
    for (text, namespace) in items {
        let meta = MemoryMeta {
            namespace: namespace.clone(),
            ..MemoryMeta::default()
        };
        let receipt = engine
            .store(StoreItem::learning(*text, LearningKind::Fact, 0.9, meta))
            .await
            .unwrap();
        ids.push(receipt.id);
    }
    (engine, ids)
}

#[test]
fn item_ids_are_deduplicated_in_order() {
    let value = json!({ "ids": ["b", "a", "b"] });
    let args = Args::parse("t", &value, &["ids"]).unwrap();
    assert_eq!(
        item_ids(&args, "ids").unwrap(),
        [ItemId::new("b"), ItemId::new("a")]
    );
}

#[test]
fn item_ids_refuse_empty_oversized_and_blank_lists() {
    let too_many: Vec<String> = (0..=MAX_IDS).map(|n| n.to_string()).collect();
    for value in [
        json!({}),
        json!({ "ids": [] }),
        json!({ "ids": too_many }),
        json!({ "ids": ["a", " "] }),
    ] {
        let args = Args::parse("t", &value, &["ids"]).unwrap();
        assert!(
            matches!(item_ids(&args, "ids"), Err(Error::InvalidRequest(_))),
            "{value}"
        );
    }
}

#[tokio::test]
async fn get_reports_ids_outside_the_reach_as_missing() {
    let own = Namespace::agent("a");
    let (engine, ids) =
        engine_with(&[("mine", own.clone()), ("theirs", Namespace::agent("b"))]).await;
    let scope = ToolScope::at(own);
    let result = get(
        &engine,
        &scope,
        &json!({ "ids": [ids[0].as_str(), ids[1].as_str()] }),
    )
    .await
    .unwrap();
    assert_eq!(result["items"][0]["text"], json!("mine"));
    assert_eq!(result["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(result["missing"], json!([ids[1].as_str()]));
}

#[tokio::test]
async fn list_overwrites_the_reach_whatever_the_filter_says() {
    let own = Namespace::agent("a");
    let (engine, _) =
        engine_with(&[("mine", own.clone()), ("theirs", Namespace::agent("b"))]).await;
    let scope = ToolScope::at(own);
    let result = list(
        &engine,
        &scope,
        &json!({ "filter": { "kinds": ["learning"] } }),
    )
    .await
    .unwrap();
    let texts: Vec<&Value> = result["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| &item["text"])
        .collect();
    assert_eq!(texts, [&json!("mine")]);
}

#[tokio::test]
async fn read_tool_accepts_the_shared_maximum_limit() {
    let (engine, _) = engine_with(&[]).await;
    let result = list(
        &engine,
        &ToolScope::default(),
        &json!({ "limit": crate::ops::MAX_LIMIT }),
    )
    .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn explorer_accepts_its_separate_maximum_bucket_limit() {
    let (engine, _) = engine_with(&[]).await;
    let result = explore(
        &engine,
        &ToolScope::default(),
        &json!({ "facet": "kind", "limit": crate::ops::MAX_BUCKETS }),
    )
    .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn fetch_refuses_a_mode_the_engine_does_not_list_by_name() {
    let (engine, _) = engine_with(&[]).await;
    let error = fetch(
        &engine,
        &ToolScope::default(),
        &json!({ "query": "x", "mode": "semantic" }),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, Error::InvalidRequest(message) if message.contains("`mode`")));
}

#[tokio::test]
async fn recall_and_explore_need_their_required_arguments() {
    let (engine, _) = engine_with(&[]).await;
    let scope = ToolScope::default();
    assert!(matches!(
        recall(&engine, &scope, &json!({})).await,
        Err(Error::InvalidRequest(message)) if message.contains("`question`")
    ));
    assert!(matches!(
        explore(&engine, &scope, &json!({})).await,
        Err(Error::InvalidRequest(message)) if message.contains("`facet`")
    ));
}
