//! Tests for the surrounding module.

use super::*;

#[test]
fn cosine_identical_vectors_is_one() {
    let a = vec![0.1_f32, 0.2, 0.3, 0.4];
    assert!((cosine_similarity(&a, &a) - 1.0).abs() < 1e-6);
}

#[test]
fn cosine_orthogonal_vectors_is_zero() {
    let a = vec![1.0_f32, 0.0, 0.0];
    let b = vec![0.0_f32, 1.0, 0.0];
    assert!(cosine_similarity(&a, &b).abs() < 1e-6);
}

#[test]
fn cosine_opposite_vectors_is_minus_one() {
    let a = vec![1.0_f32, 2.0, 3.0];
    let b = vec![-1.0_f32, -2.0, -3.0];
    assert!((cosine_similarity(&a, &b) + 1.0).abs() < 1e-6);
}

#[test]
fn cosine_zero_vector_returns_zero_not_nan() {
    let a = vec![0.0_f32; 4];
    let b = vec![1.0_f32, 2.0, 3.0, 4.0];
    let s = cosine_similarity(&a, &b);
    assert_eq!(s, 0.0, "expected 0.0, got {s}");
    assert!(!s.is_nan());
}

#[test]
fn cosine_empty_returns_zero() {
    assert_eq!(cosine_similarity(&[], &[]), 0.0);
}

#[test]
fn cosine_length_mismatch_returns_zero() {
    let a = vec![1.0_f32, 2.0];
    let b = vec![1.0_f32, 2.0, 3.0];
    assert_eq!(cosine_similarity(&a, &b), 0.0);
}

#[test]
fn pack_unpack_round_trip() {
    let v: Vec<f32> = (0..EMBEDDING_DIM).map(|i| (i as f32) / 100.0).collect();
    let packed = pack_embedding(&v);
    assert_eq!(packed.len(), EMBEDDING_DIM * 4);
    let back = unpack_embedding(&packed).unwrap();
    assert_eq!(back, v);
}

#[test]
fn unpack_wrong_byte_count_errors() {
    let bad = vec![0u8, 0, 0]; // not multiple of 4
    assert!(unpack_embedding(&bad).is_err());
}

#[test]
fn unpack_wrong_dim_errors() {
    // Correct byte multiple, but wrong float count.
    let bad = vec![0u8; 16]; // 4 floats, expected EMBEDDING_DIM (1024)
    let err = unpack_embedding(&bad).unwrap_err().to_string();
    assert!(
        err.contains(&format!("expected {EMBEDDING_DIM}")),
        "got {err}"
    );
}

#[test]
fn pack_checked_rejects_wrong_dim() {
    let too_short = vec![0.0_f32; 5];
    assert!(pack_checked(&too_short).is_err());
    let correct = vec![0.0_f32; EMBEDDING_DIM];
    assert!(pack_checked(&correct).is_ok());
}

// --- batch-embedding (variant B) scaffolding + tests ---

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tinymemory_api::host::EmbeddingProvider;

fn ok_vec() -> Vec<f32> {
    vec![0.5_f32; EMBEDDING_DIM]
}

#[derive(Clone)]
enum ProviderMode {
    /// One correct-dim vector per text (single batch call succeeds).
    Ok,
    /// Batch (`len > 1`) call errors, per-text (`len == 1`) succeeds —
    /// exercises the whole-batch-error fallback path.
    BatchFailsPerTextOk,
    /// Batch (`len > 1`) returns one extra vector, per-text is fine —
    /// exercises the length-mismatch fallback path.
    WrongCount,
    /// Returns `len` vectors but the one at `idx` has the wrong dim —
    /// length matches so no fallback; that position must map to `Err`.
    OneWrongDim(usize),
}

struct FakeProvider {
    calls: Arc<AtomicUsize>,
    mode: ProviderMode,
}

#[async_trait::async_trait]
impl EmbeddingProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }
    fn model_id(&self) -> &str {
        "fake-model"
    }
    fn dimensions(&self) -> usize {
        EMBEDDING_DIM
    }
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            ProviderMode::Ok => Ok(texts.iter().map(|_| ok_vec()).collect()),
            ProviderMode::BatchFailsPerTextOk => {
                if texts.len() > 1 {
                    anyhow::bail!("simulated batch endpoint failure")
                } else {
                    Ok(texts.iter().map(|_| ok_vec()).collect())
                }
            }
            ProviderMode::WrongCount => {
                if texts.len() > 1 {
                    Ok((0..texts.len() + 1).map(|_| ok_vec()).collect())
                } else {
                    Ok(texts.iter().map(|_| ok_vec()).collect())
                }
            }
            ProviderMode::OneWrongDim(idx) => Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| if i == idx { vec![0.0_f32; 3] } else { ok_vec() })
                .collect()),
        }
    }
}

#[tokio::test]
async fn embed_batch_via_provider_happy_is_single_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let p = FakeProvider {
        calls: calls.clone(),
        mode: ProviderMode::Ok,
    };
    let out = embed_batch_via_provider(&p, "test", &["a", "b", "c"]).await;
    assert_eq!(out.len(), 3);
    assert!(out.iter().all(|r| r.is_ok()));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "happy path must collapse to exactly one batch call"
    );
}

#[tokio::test]
async fn embed_batch_via_provider_empty_makes_no_call() {
    let calls = Arc::new(AtomicUsize::new(0));
    let p = FakeProvider {
        calls: calls.clone(),
        mode: ProviderMode::Ok,
    };
    let texts: [&str; 0] = [];
    let out = embed_batch_via_provider(&p, "test", &texts).await;
    assert!(out.is_empty());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn embed_batch_via_provider_falls_back_on_batch_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let p = FakeProvider {
        calls: calls.clone(),
        mode: ProviderMode::BatchFailsPerTextOk,
    };
    let out = embed_batch_via_provider(&p, "test", &["a", "b", "c"]).await;
    assert_eq!(out.len(), 3);
    assert!(
        out.iter().all(|r| r.is_ok()),
        "per-text fallback should still produce all vectors"
    );
    // 1 failed batch call + 3 per-text calls.
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn embed_batch_via_provider_falls_back_on_length_mismatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let p = FakeProvider {
        calls: calls.clone(),
        mode: ProviderMode::WrongCount,
    };
    let out = embed_batch_via_provider(&p, "test", &["a", "b"]).await;
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|r| r.is_ok()));
    // 1 mismatched batch call + 2 per-text calls.
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn embed_batch_via_provider_maps_wrong_dim_per_position() {
    let calls = Arc::new(AtomicUsize::new(0));
    let p = FakeProvider {
        calls: calls.clone(),
        mode: ProviderMode::OneWrongDim(1),
    };
    let out = embed_batch_via_provider(&p, "test", &["a", "b", "c"]).await;
    assert_eq!(out.len(), 3);
    assert!(out[0].is_ok());
    assert!(out[1].is_err(), "wrong-dim vector maps to Err at its slot");
    assert!(out[2].is_ok());
    // Length matched, so no fallback — a single batch call.
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

struct SeqEmbedder {
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Embedder for SeqEmbedder {
    fn name(&self) -> &'static str {
        "seq"
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if text == "bad" {
            anyhow::bail!("simulated per-text failure")
        }
        Ok(ok_vec())
    }
    // Uses the default `embed_batch`.
}

#[tokio::test]
async fn default_embed_batch_calls_embed_per_text() {
    let calls = Arc::new(AtomicUsize::new(0));
    let e = SeqEmbedder {
        calls: calls.clone(),
    };
    let out = e.embed_batch(&["a", "b", "c"]).await;
    assert_eq!(out.len(), 3);
    assert!(out.iter().all(|r| r.is_ok()));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn default_embed_batch_preserves_per_position_errors() {
    let calls = Arc::new(AtomicUsize::new(0));
    let e = SeqEmbedder {
        calls: calls.clone(),
    };
    let out = e.embed_batch(&["ok", "bad", "ok"]).await;
    assert_eq!(out.len(), 3);
    assert!(out[0].is_ok());
    assert!(out[1].is_err());
    assert!(out[2].is_ok());
}
