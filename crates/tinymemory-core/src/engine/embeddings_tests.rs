//! Tests for the surrounding module.

use super::*;

struct FakeProvider;

#[async_trait]
impl EmbeddingProvider for FakeProvider {
    fn name(&self) -> &str {
        "fake"
    }
    fn model_id(&self) -> &str {
        "fake-model"
    }
    fn dimensions(&self) -> usize {
        3
    }
    async fn embed(&self, texts: &[&str]) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|t| vec![t.len() as f32, 0.0, 0.0])
            .collect())
    }
}

#[tokio::test]
async fn backend_passes_through_metadata_and_signature() {
    let seam = SeamEmbedder::new(Arc::new(FakeProvider));
    assert_eq!(EmbeddingBackend::name(&seam), "fake");
    assert_eq!(seam.model_id(), "fake-model");
    assert_eq!(seam.dimensions(), 3);
    // Byte-identical to format_embedding_signature(name, model_id, dims).
    assert_eq!(
        EmbeddingBackend::signature(&seam),
        "provider=fake;model=fake-model;dims=3"
    );
}

#[tokio::test]
async fn backend_and_embedder_both_delegate_to_provider() {
    let seam = SeamEmbedder::new(Arc::new(FakeProvider));

    let batch = EmbeddingBackend::embed(&seam, &["ab", "cde"])
        .await
        .unwrap();
    assert_eq!(batch, vec![vec![2.0, 0.0, 0.0], vec![3.0, 0.0, 0.0]]);

    let one = Embedder::embed(&seam, "abcd").await.unwrap();
    assert_eq!(one, vec![4.0, 0.0, 0.0]);
    assert_eq!(Embedder::name(&seam), "openhuman-seam");
}
