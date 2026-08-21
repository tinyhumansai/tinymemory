//! Build an [`Embedder`] from [`Config`] settings.
//!
//! Resolution order:
//! 1. **Explicit override** — `memory_tree.embedding_endpoint` +
//!    `memory_tree.embedding_model` both Some → `OllamaEmbedder` with
//!    those exact values. For power users / E2E test rigs that want to
//!    point at a non-default Ollama endpoint.
//! 2. **Local-AI usage flag** — `config.local_ai().use_local_for_embeddings()`
//!    (i.e. `runtime_enabled && usage.embeddings`) → `OllamaEmbedder`
//!    against `ollama_base_url` with the user's chosen
//!    `config.local_ai().embedding_model_id`. This is the path driven by
//!    the "Memory embeddings" checkbox in Local AI Settings.
//! 3. **Default** — `CloudEmbedder` (OpenHuman backend / Voyage,
//!    1024 dims). Auth failures surface at the first `embed()` call so
//!    ingest's existing retry-with-backoff logic handles them.
//!
//! NOTE on dimensions: the memory tree on-disk format is hard-coded at
//! [`EMBEDDING_DIM`] (1024). If the user picks a
//! local embedding model whose output is a different dimensionality,
//! the trait's post-call validator rejects each embed with a clear
//! `expected N dims, got M` error. Switching the local model picker in
//! Local AI Settings is the fix.
//!
//! The historical `InertEmbedder` (zero vectors) path is retained for
//! tests only — it is no longer the production lax-mode fallback.
//!
//! Env var overrides applied in the host's `config::load`:
//! - `OPENHUMAN_MEMORY_EMBED_ENDPOINT`
//! - `OPENHUMAN_MEMORY_EMBED_MODEL`
//! - `OPENHUMAN_MEMORY_EMBED_TIMEOUT_MS`

use anyhow::{Context, Result};

use std::time::Duration;

#[cfg(test)]
use tinymemory_api::host::test_support::TestHostConfig;

use super::{Embedder, InertEmbedder, ProviderEmbedder, EMBEDDING_DIM};
use crate::embedding_host::require_embedding_host;
use crate::Config;
use tinyagents::harness::embeddings::{OllamaEmbeddingModel, RECOMMENDED_OLLAMA_CONTEXT_TOKENS};

/// Cheap heuristic for "is a backend session reachable?" — the cloud
/// embedder needs one and bails on first embed call without it. We use
/// the *presence* of `auth-profiles.json` next to the config file as a
/// proxy: production after login has it, test harnesses and fresh
/// pre-login installs don't. The CloudEmbedder still re-validates the
/// JWT at every embed call, so a stale file just surfaces at embed
/// time (not factory build), preserving the prior failure behavior.
fn cloud_session_available(config: &Config) -> bool {
    config
        .config_path()
        .parent()
        .map(|dir| dir.join("auth-profiles.json").exists())
        .unwrap_or(false)
}

/// Construct the active embedder for this process, honouring
/// `config.memory_tree().*` and `embedding_strict`.
///
/// Returns a boxed trait object so ingest / seal can call one code path
/// regardless of which provider is active. The returned box is created
/// per call — cheap because `OllamaEmbedder` owns a cloned `reqwest::Client`
/// internally and `InertEmbedder` is a ZST.
pub fn build_embedder_from_config(config: &Config) -> Result<Box<dyn Embedder>> {
    // Read path: walk the shared ladder, then terminate at InertEmbedder (zero
    // vectors) so retrieval / semantic rerank can still run with no provider.
    Ok(match resolve_embedder_choice(config)? {
        EmbedderChoice::Ollama {
            endpoint,
            model,
            timeout_ms,
        } => {
            log::debug!(
                "[memory_tree::embed::factory] read → Ollama endpoint={endpoint} model={model} timeout_ms={timeout_ms}"
            );
            Box::new(build_ollama_embedder(&endpoint, &model, timeout_ms)?)
        }
        EmbedderChoice::OptOut => {
            log::info!(
                "[memory_tree::embed::factory] embeddings_provider=none — \
                 using InertEmbedder (vector search disabled)"
            );
            Box::new(InertEmbedder::new())
        }
        EmbedderChoice::OpenAiCompat(openai) => {
            log::debug!(
                "[memory_tree::embed::factory] read → user OpenAI-compatible embeddings ({})",
                openai.name()
            );
            Box::new(openai)
        }
        EmbedderChoice::Cloud => {
            log::debug!(
                "[memory_tree::embed::factory] read → cloud (Voyage) — flip \
                 'Memory embeddings' in Local AI Settings to switch to local"
            );
            Box::new(build_cloud_embedder(config))
        }
        EmbedderChoice::NoProvider => {
            log::warn!(
                "[memory_tree::embed::factory] no backend session found — \
                 using InertEmbedder (zero vectors). Log in to OpenHuman, or \
                 enable 'Memory embeddings' in Local AI Settings, to fix."
            );
            Box::new(InertEmbedder::new())
        }
    })
}

/// The embedder the resolution ladder selects, independent of whether the
/// caller is a read path (retrieval) or a write path (ingest/seal). Both
/// public factories walk [`resolve_embedder_choice`] and differ ONLY at the
/// terminal + degraded-flag side-effects — so "identical resolution for every
/// real provider" is a structural guarantee, not two hand-maintained copies
/// that could drift (reviewer sanil-23, #3076: a read/write provider mismatch
/// would silently corrupt recall).
enum EmbedderChoice {
    /// Explicit Ollama override, or the unified `ollama:<model>` workload setting.
    Ollama {
        endpoint: String,
        model: String,
        timeout_ms: u64,
    },
    /// `embeddings_provider = "none"` — vector search off by deliberate user
    /// choice (NOT a degradation). Both paths use `InertEmbedder`.
    OptOut,
    /// User-configured OpenAI / custom OpenAI-compatible endpoint (#002 FR-015).
    OpenAiCompat(super::openai_compat::OpenAiCompatEmbedder),
    /// Logged-in managed cloud (Voyage).
    Cloud,
    /// No usable provider. Read path → `InertEmbedder` (zero vectors); write
    /// path → `None` (skip) + mark `semantic_recall` degraded.
    NoProvider,
}

/// Walk the provider-resolution ladder once. The order is the single source of
/// truth for both factories; the only read/write differences are encoded by the
/// callers at the terminal, never here.
fn resolve_embedder_choice(config: &Config) -> Result<EmbedderChoice> {
    let tree_cfg = &config.memory_tree();

    // 1. Explicit Ollama override (power-user / E2E rig).
    if let (Some(endpoint), Some(model)) = (
        tree_cfg.embedding_endpoint.as_deref(),
        tree_cfg.embedding_model.as_deref(),
    ) {
        if !endpoint.trim().is_empty() && !model.trim().is_empty() {
            return Ok(EmbedderChoice::Ollama {
                endpoint: endpoint.to_string(),
                model: model.to_string(),
                timeout_ms: tree_cfg.embedding_timeout_ms.unwrap_or(0),
            });
        }
    }

    // 2. Deliberate opt-out — vector search off by user choice.
    if config
        .embeddings_provider()
        .map(str::trim)
        .is_some_and(|s| s == "none")
    {
        return Ok(EmbedderChoice::OptOut);
    }

    // 3. Local Ollama via the unified workload setting.
    if let Some(model) = config.workload_local_model("embeddings") {
        return Ok(EmbedderChoice::Ollama {
            endpoint: require_embedding_host()
                .map_err(|e| anyhow::anyhow!(e))?
                .ollama_base_url(),
            model,
            timeout_ms: tree_cfg.embedding_timeout_ms.unwrap_or(0),
        });
    }

    // 4. #002 FR-015: user-configured OpenAI / custom OpenAI-compatible.
    if let Some(openai) = super::openai_compat::OpenAiCompatEmbedder::try_from_config(config)? {
        return Ok(EmbedderChoice::OpenAiCompat(openai));
    }

    // 5. Logged-in managed cloud (Voyage).
    if cloud_session_available(config) {
        return Ok(EmbedderChoice::Cloud);
    }

    // 6. Nothing usable.
    Ok(EmbedderChoice::NoProvider)
}

/// Build the embedder used by **write** paths (ingest extract + seal), with an
/// explicit "no usable embedder" signal (#002 FR-002).
///
/// Identical resolution to [`build_embedder_from_config`] for every real
/// provider (explicit Ollama override, local Ollama, cloud session). The one
/// difference is the terminal fallback: where the read-path factory returns an
/// [`InertEmbedder`] (zero vectors) so retrieval can still run, the write path
/// returns **`Ok(None)`** so callers **skip** embedding instead of persisting a
/// fake all-zero vector that would silently poison semantic recall and present
/// a degraded result as success. The chunk/summary is written embedding-less
/// (re-embeddable later once a provider is configured), and the process-global
/// `semantic_recall` degraded flag is set with a typed cause so the status /
/// doctor surface can name the fix.
///
/// `embeddings_provider = "none"` is treated as a deliberate opt-out, not a
/// degradation: it returns the [`InertEmbedder`] (vector search intentionally
/// off) without setting the degraded flag — same as the read path.
pub fn build_write_embedder(config: &Config) -> Result<Option<Box<dyn Embedder>>> {
    use crate::tree::health::{
        clear_semantic_recall_degraded, mark_semantic_recall_degraded, FailureCode,
    };

    // Write path: same ladder as the read factory, terminating at `None` (skip,
    // don't persist zero vectors) + a typed degraded flag when no provider is
    // usable. Every real-provider branch clears the flag; the deliberate
    // "none" opt-out leaves it untouched (off by choice, not degradation).
    Ok(match resolve_embedder_choice(config)? {
        EmbedderChoice::Ollama {
            endpoint,
            model,
            timeout_ms,
        } => {
            clear_semantic_recall_degraded();
            Some(Box::new(build_ollama_embedder(
                &endpoint, &model, timeout_ms,
            )?))
        }
        EmbedderChoice::OptOut => {
            clear_semantic_recall_degraded();
            log::info!(
                "[memory_tree::embed::factory] embeddings_provider=none — write path \
                 uses InertEmbedder (vector search disabled by choice)"
            );
            Some(Box::new(InertEmbedder::new()))
        }
        EmbedderChoice::OpenAiCompat(openai) => {
            clear_semantic_recall_degraded();
            Some(Box::new(openai))
        }
        EmbedderChoice::Cloud => {
            clear_semantic_recall_degraded();
            Some(Box::new(build_cloud_embedder(config)))
        }
        EmbedderChoice::NoProvider => {
            log::warn!(
                "[memory_tree::embed::factory] no usable embeddings provider — skipping \
                 embedding (chunk persists embedding-less, re-embeddable later). Set up \
                 local Ollama embeddings or log in to OpenHuman to enable semantic recall."
            );
            mark_semantic_recall_degraded(FailureCode::EmbeddingsUnconfigured);
            None
        }
    })
}

/// Render a ladder-resolution error safely for a log line.
///
/// The only user-controlled values these errors interpolate are the configured
/// provider string and model (see `openai_compat::try_from_config`), and one of
/// them is an endpoint: in its `custom:<url>` form `memory.embedding_provider`
/// *is* a URL, which may carry `user:pass@` userinfo. Configured
/// `cloud_providers` endpoints can reach the message the same way through the
/// underlying constructor's own context.
///
/// So rather than dropping the reason — which would cost the diagnostic that
/// makes this log worth having ("dimension mismatch", "build failed") — replace
/// each known endpoint substring with its [`redact_endpoint`] form. Scrubbing
/// the exact strings we already hold is precise, where a generic URL-matching
/// pass over free text would be guesswork (CodeRabbit, #5402 / CWE-532).
fn redact_ladder_error(config: &Config, err: &anyhow::Error) -> String {
    use crate::util::redact::redact_endpoint;

    // Candidates: the inline `custom:<url>` endpoint (when that is the
    // configured form) plus every configured OpenAI-compatible endpoint
    // (LM Studio, vLLM, …), any of which the ladder may have been resolving.
    let mut endpoints: Vec<&str> = config
        .memory()
        .embedding_provider
        .trim()
        .strip_prefix("custom:")
        .into_iter()
        .chain(config.cloud_providers().iter().map(|e| e.endpoint.as_str()))
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .collect();

    // Longest first. Substring replacement is order-sensitive: if a short
    // endpoint is a strict prefix of a longer one (`https://host` vs
    // `https://host/v1?key=…`), scrubbing the short one first rewrites the
    // longer one's prefix, so its own replacement no longer matches and the
    // credential-bearing suffix survives in the log (CodeRabbit, #5402).
    endpoints.sort_by_key(|e| std::cmp::Reverse(e.len()));
    endpoints.dedup();

    let mut msg = format!("{err:#}");
    for endpoint in endpoints {
        msg = msg.replace(endpoint, &redact_endpoint(endpoint));
    }
    msg
}

/// Slug naming the embedder ingestion will **actually** use, walking the same
/// `resolve_embedder_choice` ladder the read and write factories walk.
///
/// This exists because `config.memory().embedding_provider` is *not* authoritative
/// for how embeddings are funded, and reading it as if it were produces a false
/// alarm. The ladder resolves local Ollama from `memory_tree.embedding_endpoint`
/// or from the unified `workload_local_model("embeddings")` setting (the "Memory
/// embeddings" toggle in Local AI Settings), and **neither path rewrites
/// `memory.embedding_provider`** — so a user running fully local still reads as
/// `"cloud"` there. Any surface that asks "do these embeddings bill against the
/// managed budget?" must ask this function, not that field (reviewer M3gA-Mind,
/// #5402: otherwise a local-embeddings user whose *chat* budget crosses 90% is
/// told memory has stopped growing while it is growing fine).
///
/// Slugs are stable wire values consumed by the frontend:
/// - `"ollama"` — local daemon; user-funded.
/// - `"custom"` — user's own OpenAI-compatible endpoint / key; user-funded.
/// - `"cloud"` — managed OpenHuman backend; **bills the managed cycle budget**.
/// - `"none"` — deliberate opt-out (`embeddings_provider = "none"`).
/// - `"unconfigured"` — no usable provider (signed out); nothing is billed.
/// - `"unknown"` — the ladder itself failed to resolve. Deliberately not
///   `"cloud"`: an unresolvable config must never manufacture a budget warning.
pub fn effective_embedder_slug(config: &Config) -> &'static str {
    let slug = match resolve_embedder_choice(config) {
        Ok(EmbedderChoice::Ollama { .. }) => "ollama",
        Ok(EmbedderChoice::OptOut) => "none",
        Ok(EmbedderChoice::OpenAiCompat(_)) => "custom",
        Ok(EmbedderChoice::Cloud) => "cloud",
        Ok(EmbedderChoice::NoProvider) => "unconfigured",
        Err(err) => {
            log::warn!(
                "[memory_tree::embed::factory] effective_embedder_slug: ladder failed to \
                 resolve ({}) — reporting 'unknown' (treated as NOT managed)",
                redact_ladder_error(config, &err)
            );
            "unknown"
        }
    };
    log::debug!("[memory_tree::embed::factory] effective_embedder_slug → {slug}");
    slug
}

fn build_ollama_embedder(endpoint: &str, model: &str, timeout_ms: u64) -> Result<ProviderEmbedder> {
    let timeout = Duration::from_millis(if timeout_ms == 0 { 10_000 } else { timeout_ms });
    let client = reqwest::Client::builder()
        .connect_timeout(timeout)
        .build()
        .context("build Ollama embeddings HTTP client")?;
    let model = OllamaEmbeddingModel::try_new(endpoint, model, EMBEDDING_DIM)?
        .with_context_options(
            RECOMMENDED_OLLAMA_CONTEXT_TOKENS,
            RECOMMENDED_OLLAMA_CONTEXT_TOKENS,
        )
        .with_client(client);
    Ok(ProviderEmbedder::new(
        crate::embedding_adapter::TinyAgentsEmbeddingProvider::boxed(model),
        "ollama",
    ))
}

fn build_cloud_embedder(config: &Config) -> ProviderEmbedder {
    let openhuman_dir = config.config_path().parent().map(std::path::PathBuf::from);
    // The managed cloud embedder resolves a session JWT and the backend API
    // URL, both host concerns — it is built through the seam rather than here.
    // `openhuman_dir` stays part of the caller's contract: the host reads the
    // encrypted-secrets material relative to it.
    let _ = openhuman_dir;
    let host = require_embedding_host().expect("embedding host installed");
    let provider = host
        .cloud_embedding_provider(
            host.default_cloud_embedding_model(),
            host.default_cloud_embedding_dimensions(),
        )
        .expect("cloud embedding provider");
    ProviderEmbedder::new(provider, "cloud")
}

#[cfg(test)]
#[path = "factory_tests.rs"]
mod tests;
