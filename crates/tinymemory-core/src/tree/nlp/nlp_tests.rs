//! Tests for the surrounding module.

use super::*;

fn cfg_spacy_off() -> TestHostConfig {
    crate::test_seams::init();
    let mut c = TestHostConfig::default();
    c.memory_tree.spacy_enabled = false;
    c
}

#[test]
fn label_mapping_covers_common_kinds() {
    assert_eq!(map_spacy_label("PERSON"), EntityKind::Person);
    assert_eq!(map_spacy_label("ORG"), EntityKind::Organization);
    assert_eq!(map_spacy_label("GPE"), EntityKind::Location);
    assert_eq!(map_spacy_label("WHATEVER"), EntityKind::Misc);
}

#[tokio::test]
async fn fallback_used_when_spacy_disabled_extracts_mechanical_entities() {
    let cfg = cfg_spacy_off();
    let ents = extract_query_entities(&cfg, "ping alice@example.com about #launch").await;
    assert!(
        ents.iter()
            .any(|e| e.canonical_id == "email:alice@example.com"),
        "regex fallback should find the email; got {ents:?}"
    );
    assert!(
        ents.iter().any(|e| e.kind == EntityKind::Hashtag),
        "regex fallback should find the hashtag; got {ents:?}"
    );
}

#[tokio::test]
async fn empty_query_yields_no_entities() {
    let cfg = cfg_spacy_off();
    assert!(extract_query_entities(&cfg, "   ").await.is_empty());
}

#[test]
fn spacy_response_maps_nouns_to_topics() {
    let resp = SpacyResponse {
        entities: vec![crate::nlp_host::SpacyEntity {
            text: "Alice".into(),
            label: "PERSON".into(),
            start: 0,
            end: 5,
        }],
        nouns: vec!["migration".into()],
    };
    let extracted = spacy_to_extracted(&resp);
    let canon = canonicalise(&extracted);
    assert!(canon.iter().any(|c| c.canonical_id == "person:alice"));
    assert!(canon.iter().any(|c| c.canonical_id == "topic:migration"));
}
