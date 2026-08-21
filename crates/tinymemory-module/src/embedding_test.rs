//! These drive a real in-memory bus with a stand-in host embedder.
//!
//! The interesting cases are the refusals. A host that returns the wrong number
//! of vectors, or vectors of the wrong width, has silently split the embedding
//! space — every vector written on the wrong side of the split becomes
//! unsearchable without a re-embed, and nothing fails at the time. So the
//! provider checks, and these tests are what prove it checks.

use std::sync::Arc;

use tinybus::broker::Broker;
use tinybus::transport::memory::MemoryBus;
use tinybus::{Connection, Result as BusResult};
use tinymemory_api::host::{EmbeddingHost, EmbeddingProvider};

use super::{BusEmbeddingHost, EMBEDDING_HOST_BUS_NAME, EMBEDDING_HOST_OBJECT_PATH};
use crate::config::ModuleConfig;

/// A stand-in for the host's embedder, returning vectors of a chosen width.
struct FakeHostEmbedder {
    /// Width of each returned vector. Set to something other than the requested
    /// dimensionality to exercise the mismatch refusal.
    width: usize,
    /// Return this many vectors regardless of input count, when `Some`.
    force_count: Option<usize>,
}

#[tinybus::interface(name = "ai.tinyhumans.tinymemory.EmbeddingHost")]
impl FakeHostEmbedder {
    async fn embed(
        &self,
        _model: String,
        _dimensions: usize,
        texts: Vec<String>,
    ) -> BusResult<Vec<Vec<f32>>> {
        std::future::ready(()).await;
        let count = self.force_count.unwrap_or(texts.len());
        Ok((0..count).map(|_| vec![0.5_f32; self.width]).collect())
    }
}

/// Bring up a bus with `embedder` served at the host's well-known name.
///
/// The broker task is leaked deliberately: it lives as long as the test, and
/// joining it would mean shutting the bus down before the assertions run.
async fn bus_with_host(embedder: FakeHostEmbedder) -> Connection {
    let bus = MemoryBus::new();
    let broker = Broker::new();
    let _broker_task = broker.spawn(bus.clone());

    let host_side = Connection::connect(bus.connect().await.expect("host transport"))
        .await
        .expect("host connection");
    host_side
        .serve_at(
            EMBEDDING_HOST_OBJECT_PATH.try_into().expect("valid path"),
            embedder,
        )
        .await
        .expect("serve embedder");
    host_side
        .request_name(EMBEDDING_HOST_BUS_NAME)
        .await
        .expect("claim name");
    // Held for the lifetime of the test: dropping it would release the name.
    std::mem::forget(host_side);

    Connection::connect(bus.connect().await.expect("module transport"))
        .await
        .expect("module connection")
}

fn config_with_dims(dims: usize) -> ModuleConfig {
    ModuleConfig {
        workspace_dir: "/tmp/tinymemory-module-test".into(),
        cloud_embedding_model: "test-model".to_string(),
        cloud_embedding_dimensions: dims,
        models_supporting_dimensions: vec!["test-model".to_string()],
        ..ModuleConfig::default()
    }
}

#[tokio::test]
async fn a_batch_is_embedded_over_the_bus() {
    let connection = bus_with_host(FakeHostEmbedder {
        width: 4,
        force_count: None,
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(4));
    let provider = host.default_embedding_provider();

    let vectors = provider
        .embed(&["alpha", "beta"])
        .await
        .expect("the host embeds");

    assert_eq!(vectors.len(), 2);
    assert!(vectors.iter().all(|vector| vector.len() == 4));
}

#[tokio::test]
async fn a_wrong_width_is_refused_rather_than_written() {
    // The dangerous case. Accepting these would write vectors into a space they
    // do not belong to, and nothing would fail until a later search silently
    // returned nothing.
    let connection = bus_with_host(FakeHostEmbedder {
        width: 8,
        force_count: None,
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(4));
    let provider = host.default_embedding_provider();

    let error = provider
        .embed(&["alpha"])
        .await
        .expect_err("an 8-wide vector must not pass as 4-wide");
    assert!(error.to_string().contains("dimension"), "{error}");
}

#[tokio::test]
async fn a_wrong_vector_count_is_refused() {
    // Callers pair inputs with outputs positionally, so a short reply would
    // attach the wrong vector to the wrong chunk.
    let connection = bus_with_host(FakeHostEmbedder {
        width: 4,
        force_count: Some(1),
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(4));
    let provider = host.default_embedding_provider();

    let error = provider
        .embed(&["alpha", "beta"])
        .await
        .expect_err("one vector for two inputs must be refused");
    assert!(error.to_string().contains("vectors"), "{error}");
}

#[tokio::test]
async fn a_zero_dimension_provider_yields_empty_vectors() {
    // Zero dimensions is the engine's "semantic search off" state, and the
    // vectors it yields are expected to be empty rather than merely unchecked.
    let connection = bus_with_host(FakeHostEmbedder {
        width: 0,
        force_count: None,
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(0));
    let provider = host.default_embedding_provider();

    let vectors = provider
        .embed(&["alpha"])
        .await
        .expect("zero dims is legal");
    assert_eq!(vectors.len(), 1);
    assert!(vectors[0].is_empty());
}

#[tokio::test]
async fn a_zero_dimension_request_answered_with_real_vectors_is_refused() {
    // The case an earlier revision let through: `dimensions == 0` skipped the
    // width check outright, so a host could answer a "semantic search off"
    // request with a real 768-wide space and pass validation. The engine would
    // then believe no vectors existed while the store filled with embeddings
    // from a space nothing tracks — the split-space failure, with the split
    // hidden. Zero means empty, and this is the test that says so.
    let connection = bus_with_host(FakeHostEmbedder {
        width: 768,
        force_count: None,
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(0));
    let provider = host.default_embedding_provider();

    let error = provider
        .embed(&["alpha"])
        .await
        .expect_err("a 768-wide answer to a zero-dimension request must be refused");
    assert!(error.to_string().contains("768"), "{error}");
}

#[tokio::test]
async fn an_empty_batch_never_reaches_the_bus() {
    // No host is served here at all, so this only passes if the call short
    // circuits — which is what makes it a test of the short circuit rather than
    // of the happy path.
    let bus = MemoryBus::new();
    let broker = Broker::new();
    let _task = broker.spawn(bus.clone());
    let connection = Connection::connect(bus.connect().await.expect("transport"))
        .await
        .expect("connection");

    let host = BusEmbeddingHost::new(connection, &config_with_dims(4));
    let provider = host.default_embedding_provider();

    let vectors = provider
        .embed(&[])
        .await
        .expect("an empty batch is trivial");
    assert!(vectors.is_empty());
}

#[tokio::test]
async fn an_absent_host_fails_by_name_rather_than_hanging() {
    // A host that never served its embedder must produce an error the operator
    // can act on. This is also why the embedder is not declared as a module
    // `requires`: that would leave the module permanently unresolved instead.
    let bus = MemoryBus::new();
    let broker = Broker::new();
    let _task = broker.spawn(bus.clone());
    let connection = Connection::connect(bus.connect().await.expect("transport"))
        .await
        .expect("connection");

    let host = BusEmbeddingHost::new(connection, &config_with_dims(4));
    let provider = host.default_embedding_provider();

    let error = provider
        .embed(&["alpha"])
        .await
        .expect_err("no embedder is served");
    assert!(!error.to_string().is_empty());
}

#[tokio::test]
async fn the_module_never_reports_a_credential() {
    // The central claim, asserted on the behaviour rather than the config shape:
    // no provider name yields a key, including ones a host would normally have
    // one for.
    let connection = bus_with_host(FakeHostEmbedder {
        width: 4,
        force_count: None,
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(4));

    for provider in ["openai", "cohere", "voyage", "custom", "ollama", "cloud"] {
        assert!(
            host.resolve_api_key(provider).is_none(),
            "{provider} must not resolve a key inside the module"
        );
    }
}

#[tokio::test]
async fn a_keyed_provider_request_still_builds_and_ignores_the_key() {
    // The engine may ask for a keyed provider with an empty key. Refusing would
    // break recall; forwarding a key would defeat the split. It builds, and the
    // key goes nowhere.
    let connection = bus_with_host(FakeHostEmbedder {
        width: 3,
        force_count: None,
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(3));

    let provider = host
        .create_embedding_provider_with_credentials("openai", "text-embedding-3-small", 3, "", None)
        .expect("a keyed provider still builds");

    assert_eq!(provider.model_id(), "text-embedding-3-small");
    assert_eq!(provider.dimensions(), 3);
    let vectors = provider.embed(&["alpha"]).await.expect("embeds");
    assert_eq!(vectors[0].len(), 3);
}

#[tokio::test]
async fn dimension_support_is_answered_from_configuration() {
    // A synchronous getter cannot make a bus call, so the host passes the list.
    // Absent means "unsupported", which is the safe direction: the engine omits
    // the parameter rather than writing a batch the provider rejects halfway.
    let connection = bus_with_host(FakeHostEmbedder {
        width: 4,
        force_count: None,
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(4));

    assert!(host.model_supports_dimensions("test-model"));
    assert!(!host.model_supports_dimensions("some-other-model"));
}

#[tokio::test]
async fn configured_getters_and_provider_factories_preserve_their_identity() {
    let connection = bus_with_host(FakeHostEmbedder {
        width: 6,
        force_count: None,
    })
    .await;
    let config = ModuleConfig {
        ollama_base_url: "http://embedder.internal:11434".to_string(),
        cloud_embedding_model: "cloud-default".to_string(),
        cloud_embedding_dimensions: 6,
        ..ModuleConfig::default()
    };
    let host = BusEmbeddingHost::new(connection, &config);

    assert_eq!(host.ollama_base_url(), "http://embedder.internal:11434");
    assert_eq!(host.default_cloud_embedding_model(), "cloud-default");
    assert_eq!(host.default_cloud_embedding_dimensions(), 6);

    let default_provider = host.default_embedding_provider();
    assert_eq!(default_provider.name(), "module-bus");
    assert_eq!(default_provider.model_id(), "cloud-default");
    assert_eq!(default_provider.dimensions(), 6);

    let cloud = host
        .cloud_embedding_provider("cloud-explicit", 6)
        .expect("cloud provider builds");
    assert_eq!(cloud.name(), "cloud");
    assert_eq!(cloud.model_id(), "cloud-explicit");
    assert_eq!(cloud.dimensions(), 6);

    let ollama = host
        .ollama_embedding_provider("http://ignored.example", "nomic-embed-text", 6)
        .expect("ollama provider builds");
    assert_eq!(ollama.name(), "ollama");
    assert_eq!(ollama.model_id(), "nomic-embed-text");
    assert_eq!(ollama.dimensions(), 6);

    let rendered = format!("{:?}", host.provider("ollama", "nomic-embed-text", 6));
    assert!(rendered.contains("BusEmbeddingProvider"), "{rendered}");
    assert!(rendered.contains("ollama"), "{rendered}");
    assert!(rendered.contains("nomic-embed-text"), "{rendered}");
    assert!(rendered.contains("dimensions: 6"), "{rendered}");
    assert!(!rendered.contains("Connection"), "{rendered}");
}

#[tokio::test]
async fn the_signature_matches_what_the_contract_formats() {
    // Drift between a live provider's signature and a config-derived one splits
    // one embedding space in two, so both must route through the same formatter.
    let connection = bus_with_host(FakeHostEmbedder {
        width: 4,
        force_count: None,
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(4));
    let provider = host.default_embedding_provider();

    assert_eq!(
        provider.signature(),
        super::bus_provider_signature(provider.name(), provider.model_id(), 4)
    );
}

#[tokio::test]
async fn the_debug_form_carries_no_connection_and_no_key() {
    // `Debug` output reaches logs. It must not become a place a credential or a
    // transport detail leaks.
    //
    // Async despite testing a synchronous formatter: `Broker::spawn` needs a
    // reactor, so building a connection at all requires a runtime.
    let bus = MemoryBus::new();
    let broker = Broker::new();
    let _task = broker.spawn(bus.clone());
    let connection = Connection::connect(bus.connect().await.expect("transport"))
        .await
        .expect("connection");

    let host = BusEmbeddingHost::new(connection, &config_with_dims(4));
    let rendered = format!("{host:?}");
    assert!(rendered.contains("BusEmbeddingHost"), "{rendered}");
    for forbidden in ["api_key", "token", "secret", "Connection"] {
        assert!(!rendered.contains(forbidden), "{rendered}");
    }
}

/// Kept honest: an `Arc<dyn EmbeddingProvider>` is what the engine holds, so the
/// bus provider must be usable as one.
#[tokio::test]
async fn the_provider_is_usable_as_a_trait_object() {
    let connection = bus_with_host(FakeHostEmbedder {
        width: 2,
        force_count: None,
    })
    .await;
    let host = BusEmbeddingHost::new(connection, &config_with_dims(2));

    let provider: Arc<dyn EmbeddingProvider> = host.default_embedding_provider();
    let one = provider.embed_one("alpha").await.expect("embed_one works");
    assert_eq!(one.len(), 2);
}
