//! Tests for the surrounding module.

use super::UnifiedMemory;
use serde_json::json;

// ── vec_to_bytes / bytes_to_vec ──────────────────────────────────

#[test]
fn vec_bytes_roundtrip() {
    let original = vec![1.0_f32, 2.5, -3.0, 0.0];
    let bytes = UnifiedMemory::vec_to_bytes(&original);
    assert_eq!(bytes.len(), 16); // 4 floats * 4 bytes
    let back = UnifiedMemory::bytes_to_vec(&bytes);
    assert_eq!(back, original);
}

#[test]
fn vec_to_bytes_empty() {
    let bytes = UnifiedMemory::vec_to_bytes(&[]);
    assert!(bytes.is_empty());
    let back = UnifiedMemory::bytes_to_vec(&bytes);
    assert!(back.is_empty());
}

// ── cosine_similarity ────────────────────────────────────────────

#[test]
fn cosine_similarity_identical_vectors() {
    let v = vec![1.0_f32, 0.0, 0.0];
    let sim = UnifiedMemory::cosine_similarity(&v, &v);
    assert!((sim - 1.0).abs() < 1e-6);
}

#[test]
fn cosine_similarity_orthogonal_vectors() {
    let a = vec![1.0_f32, 0.0];
    let b = vec![0.0_f32, 1.0];
    let sim = UnifiedMemory::cosine_similarity(&a, &b);
    assert!(sim.abs() < 1e-6);
}

#[test]
fn cosine_similarity_different_lengths_returns_zero() {
    let a = vec![1.0_f32, 0.0];
    let b = vec![1.0_f32, 0.0, 0.0];
    assert_eq!(UnifiedMemory::cosine_similarity(&a, &b), 0.0);
}

#[test]
fn cosine_similarity_empty_vectors_returns_zero() {
    assert_eq!(UnifiedMemory::cosine_similarity(&[], &[]), 0.0);
}

#[test]
fn cosine_similarity_zero_vector_returns_zero() {
    let a = vec![0.0_f32, 0.0];
    let b = vec![1.0_f32, 0.0];
    assert_eq!(UnifiedMemory::cosine_similarity(&a, &b), 0.0);
}

// ── collapse_whitespace ──────────────────────────────────────────

#[test]
fn collapse_whitespace_normalizes() {
    assert_eq!(
        UnifiedMemory::collapse_whitespace("  hello   world  "),
        "hello world"
    );
}

#[test]
fn collapse_whitespace_empty() {
    assert_eq!(UnifiedMemory::collapse_whitespace(""), "");
}

// ── normalize_search_text ────────────────────────────────────────

#[test]
fn normalize_search_text_lowercases_and_strips_special() {
    let result = UnifiedMemory::normalize_search_text("Hello, World! @#$ test");
    assert_eq!(result, "hello world test");
}

#[test]
fn normalize_search_text_preserves_separators() {
    let result = UnifiedMemory::normalize_search_text("path/to_file-name.txt");
    assert_eq!(result, "path to file name txt");
}

// ── tokenize_search_terms ────────────────────────────────────────

#[test]
fn tokenize_search_terms_splits_correctly() {
    let terms = UnifiedMemory::tokenize_search_terms("Hello World");
    assert_eq!(terms, vec!["hello", "world"]);
}

#[test]
fn tokenize_search_terms_empty() {
    assert!(UnifiedMemory::tokenize_search_terms("").is_empty());
    assert!(UnifiedMemory::tokenize_search_terms("  @#$  ").is_empty());
}

// ── normalize_graph_entity / predicate ───────────────────────────

#[test]
fn normalize_graph_entity_uppercases() {
    assert_eq!(
        UnifiedMemory::normalize_graph_entity("  rust language  "),
        "RUST LANGUAGE"
    );
}

#[test]
fn normalize_graph_predicate_underscores_separators() {
    assert_eq!(
        UnifiedMemory::normalize_graph_predicate("is written in"),
        "IS_WRITTEN_IN"
    );
}

#[test]
fn normalize_graph_predicate_strips_trailing_underscores() {
    assert_eq!(UnifiedMemory::normalize_graph_predicate("  has -- "), "HAS");
}

// ── json_string_array ────────────────────────────────────────────

#[test]
fn json_string_array_from_array_and_singular() {
    let val = json!({"tags": ["a", "b"], "tag": "c"});
    let result = UnifiedMemory::json_string_array(&val, "tags", "tag");
    assert_eq!(result, vec!["a", "b", "c"]);
}

#[test]
fn json_string_array_deduplicates() {
    let val = json!({"tags": ["a", "a"], "tag": "a"});
    let result = UnifiedMemory::json_string_array(&val, "tags", "tag");
    assert_eq!(result, vec!["a"]);
}

#[test]
fn json_string_array_empty_when_missing() {
    let val = json!({});
    let result = UnifiedMemory::json_string_array(&val, "tags", "tag");
    assert!(result.is_empty());
}

#[test]
fn json_string_array_filters_empty_strings() {
    let val = json!({"tags": ["", "  ", "valid"]});
    let result = UnifiedMemory::json_string_array(&val, "tags", "tag");
    assert_eq!(result, vec!["valid"]);
}

// ── json_i64 ─────────────────────────────────────────────────────

#[test]
fn json_i64_from_integer() {
    assert_eq!(UnifiedMemory::json_i64(&json!({"n": 42}), "n"), Some(42));
}

#[test]
fn json_i64_from_float() {
    assert_eq!(UnifiedMemory::json_i64(&json!({"n": 3.9}), "n"), Some(3));
}

#[test]
fn json_i64_missing_key() {
    assert_eq!(UnifiedMemory::json_i64(&json!({}), "n"), None);
}

#[test]
fn json_i64_from_string_returns_none() {
    assert_eq!(UnifiedMemory::json_i64(&json!({"n": "42"}), "n"), None);
}

// ── recency_score ────────────────────────────────────────────────

#[test]
fn recency_score_current_time_is_one() {
    let now = 1_700_000_000.0;
    let score = UnifiedMemory::recency_score(now, now);
    assert!((score - 1.0).abs() < 1e-6);
}

#[test]
fn recency_score_old_document_is_lower() {
    let now = 1_700_000_000.0;
    let one_day_ago = now - 86400.0;
    let score = UnifiedMemory::recency_score(one_day_ago, now);
    assert!(score < 1.0);
    assert!(score > 0.0);
}

#[test]
fn recency_score_future_clamped_to_one() {
    let now = 1_700_000_000.0;
    let future = now + 86400.0;
    let score = UnifiedMemory::recency_score(future, now);
    assert!((score - 1.0).abs() < 1e-6);
}

// ── chunk_document_content ───────────────────────────────────────

#[test]
fn chunk_document_content_returns_nonempty_for_content() {
    let chunks = UnifiedMemory::chunk_document_content("Hello world. This is a test.", 100);
    assert!(!chunks.is_empty());
}

#[test]
fn chunk_document_content_empty_input_returns_empty() {
    let chunks = UnifiedMemory::chunk_document_content("", 100);
    assert!(chunks.is_empty());
}

#[test]
fn chunk_document_content_whitespace_only_returns_empty() {
    let chunks = UnifiedMemory::chunk_document_content("   \n  \t  ", 100);
    assert!(chunks.is_empty());
}
