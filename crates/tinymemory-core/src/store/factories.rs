//! # Memory Store Factories
//!
//! Factory functions for creating and initializing various memory store
//! implementations.
//!
//! This module provides a centralized way to instantiate memory stores based on
//! configuration, ensuring that the correct embedding providers and storage
//! backends are used. Currently, it primarily focuses on creating
//! `UnifiedMemory` instances.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;
use rusqlite::Connection;

use crate::embedding_host::require_embedding_host;
use crate::store::namespace_store::UnifiedMemory;
use crate::traits::Memory;
use tinyagents::harness::embeddings::{DEFAULT_OLLAMA_DIMENSIONS, DEFAULT_OLLAMA_MODEL};
use tinymemory_api::host::MemoryConfig;
use tinymemory_api::host::{format_embedding_signature, EmbeddingProvider};
use tinymemory_api::host::{EmbeddingRouteConfig, StorageProviderConfig};

/// One-shot guard so the Ollama health-gate fallback only reports to Sentry
/// once per process lifetime. Memory is constructed many times per session
/// (once per agent in the harness), so an unguarded `report_error` would
/// re-create the per-embed flood the gate exists to suppress — just with a
/// different message. The first failed probe trips this flag; subsequent
/// probes log at debug level and skip the Sentry report.
static OLLAMA_HEALTH_REPORTED: AtomicBool = AtomicBool::new(false);

/// Reports the Ollama-unreachable fallback to Sentry at most once per
/// process and publishes an [`EmbeddingModelUnhealthy`] domain event.
///
/// The "once" applies to the Sentry report and the domain event only. The
/// client-facing `user_error` broadcast fires on **every** call, deliberately
/// — see the comment on the first statement.
///
/// Returns `true` on the firing call, `false` afterwards — callers use the
/// return value only for logging context.
///
/// [`EmbeddingModelUnhealthy`]: crate::events::MemoryEvent::EmbeddingModelUnhealthy
fn report_ollama_health_gate_once(base_url: &str, model: &str) -> bool {
    // Deliberately ABOVE the Sentry latch (#5354). `publish_web_channel_event`
    // is a `broadcast::send`: with no socket client attached yet it returns Err
    // and the event is dropped, with no buffering and no redelivery. Memory is
    // constructed early (once per agent in the harness), so the very first
    // failed probe usually lands before the renderer's socket is up — under the
    // latch that one dropped send would be the only attempt ever made, and the
    // UserErrorCenter would stay empty for the whole outage.
    //
    // Re-broadcasting per failed probe is safe and intended: the panel store
    // dedupes on the descriptor's `kind:scope:provider` identity and bumps
    // `count`, so repeats collapse into one entry rather than stacking. Only
    // the Sentry report below stays once-per-process, which is what the latch
    // was introduced for.
    surface_local_model_unavailable_to_clients();

    if OLLAMA_HEALTH_REPORTED
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        log::debug!(
            "[memory::factory] ollama health-gate fallback already reported this process; suppressing duplicate at {base_url} model={model}"
        );
        return false;
    }
    // Tags are indexed and grouped on; keep them low-cardinality and free of
    // credentials. Full URL stays in the message body for diagnostics.
    let host_tag = redact_ollama_host(base_url);
    let sentry_message = format!(
        "ollama embeddings opted-in but daemon unreachable at {base_url}; falling back to cloud embeddings for this session"
    );
    // Route through `report_error_or_expected` so the GX arm of
    // `is_ollama_user_config_rejection` in `expected_error_kind` demotes
    // the message to an info breadcrumb (user-state: ollama daemon not
    // running). Direct `report_error_message` here bypassed the classifier
    // and produced TAURI-RUST-B (~409 events). The `&str` input avoids
    // the `format!("{:#}")` round-trip that `report_error` would do on an
    // anyhow chain — the wire shape stays bit-identical.
    crate::observability::report_error_or_expected(
        sentry_message.as_str(),
        "memory",
        "ollama_health_gate",
        &[("ollama_host", host_tag), ("fallback", "cloud")],
    );

    // Publish a user-visible domain event so the UI can surface a notification
    // with an actionable fix hint. The event bus is best-effort (no runtime
    // present in unit-test contexts without `init_global`), so we fire-and-
    // forget and ignore any lagged-receiver errors.
    let user_message = format!(
        "Local embedding model unreachable — falling back to cloud embeddings. \
         Run `ollama pull {model}` to fix."
    );
    log::debug!(
        "[memory::factory] publishing EmbeddingModelUnhealthy event: provider=ollama model={model} fallback=cloud"
    );
    let event = crate::events::MemoryEvent::EmbeddingModelUnhealthy(
        tinymemory_api::host::EmbeddingHealthReason {
            provider: "ollama".to_string(),
            model: model.to_string(),
            fallback_provider: "cloud".to_string(),
            message: user_message,
        },
    );
    // publish_global is infallible (drops the event when no receivers are
    // registered, which is fine for the health-gate use case).
    crate::events::publish(event);

    true
}

/// Surface the Ollama-unreachable fallback in every connected client's
/// UserErrorCenter (#5354).
///
/// `DomainEvent::EmbeddingModelUnhealthy` is published above, but nothing
/// bridges the domain bus to the product UI — `/events/domain` is consumed only
/// by the developer Event Log panel — so that event alone reaches no user. The
/// payload and publisher live in `memory::tree::health::user_error` so this
/// producer and the embed-failure classifier emit one identical, tested shape.
fn surface_local_model_unavailable_to_clients() {
    crate::tree::health::publish_local_model_unavailable_user_error("health_gate");
}

// The once-per-process Sentry latch reset used by fallback tests moved out of
// this production file so an earlier test cannot affect later fallback cases.
// Its implementation now lives beside those cases in `factories_tests.rs`.
// The helper now lives in `factories_tests.rs`. Keep this non-executable range
// so LLVM can merge production regions linked into multiple workspace tests.
// Moving the following functions would otherwise duplicate their line regions.
//
// No test behavior is compiled from this production source file.
/// Effective Ollama base URL.
///
/// Delegates to the host's [`EmbeddingHost::ollama_base_url`] so the probe
/// always agrees with the rest of the Ollama machinery on the daemon address.
/// If a future change adds another env-var override or shifts precedence, the
/// memory health-gate picks it up automatically.
fn ollama_base_url_for_probe() -> String {
    require_embedding_host()
        .map(|host| host.ollama_base_url())
        .unwrap_or_default()
}

/// Canonical `(provider, model, dimensions)` tuple used everywhere the
/// health-gate falls back from Ollama → cloud. Centralised so both the async
/// and sync gate sites agree if the cloud defaults ever change.
fn cloud_embedding_fallback() -> (String, String, usize) {
    // The cloud defaults are the host's to state — it owns the managed
    // endpoint. Falling back to the ollama defaults when unwired keeps this
    // total; a caller with no embedding host has bigger problems than the
    // fallback tuple being wrong.
    match require_embedding_host() {
        Ok(host) => (
            "cloud".to_string(),
            host.default_cloud_embedding_model().to_string(),
            host.default_cloud_embedding_dimensions(),
        ),
        Err(_) => (
            "cloud".to_string(),
            DEFAULT_OLLAMA_MODEL.to_string(),
            DEFAULT_OLLAMA_DIMENSIONS,
        ),
    }
}

/// Extracts a low-cardinality `host[:port]` tag from `base_url` for Sentry.
///
/// Sentry tags are indexed and should not carry secrets or per-instance noise:
/// `http://user:token@host:11434/api/tags?key=v` collapses to `host:11434`.
/// Falls back to `"unknown"` if parsing yields an empty string so we never
/// emit an empty tag value.
fn redact_ollama_host(base_url: &str) -> &str {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let after_userinfo = after_scheme
        .rsplit_once('@')
        .map_or(after_scheme, |(_, h)| h);
    let host = after_userinfo
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        "unknown"
    } else {
        host
    }
}

/// Probe whether an Ollama daemon is reachable at `base_url`.
///
/// Issues a short-timeout `GET <base_url>/api/tags` (the standard Ollama
/// "list models" endpoint) and returns `true` only when it responds with a
/// 2xx status. Transport failures, timeouts, and non-2xx responses all
/// return `false`.
///
/// Kept deliberately small and side-effect-free so it can be called from
/// the memory factory's startup path without pulling in the full
/// `local_ai::service::ollama_admin` machinery.
///
/// Scoped `pub(crate)` to match `local_ai::ollama_base_url`; the only callers
/// are the factory itself and its sibling tests. Stable external API for the
/// health-gate is [`effective_embedding_settings_probed`].
pub(crate) async fn probe_ollama_reachable(base_url: &str) -> bool {
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            log::debug!(
                "[memory::factory] probe_ollama_reachable: failed to build http client: {e}"
            );
            return false;
        }
    };
    match client.get(&url).send().await {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            log::debug!("[memory::factory] probe_ollama_reachable: {url} unreachable: {e}");
            false
        }
    }
}

/// Returns the effective `(provider, model, dimensions)` triple for the
/// embedding backend.
///
/// The user-facing default is `"cloud"` (OpenHuman backend, Voyage-backed) so
/// fresh installs work without a local Ollama daemon. When the user has
/// explicitly opted into local AI for embeddings —
/// `LocalAiConfig::use_local_for_embeddings` — we route through the local
/// Ollama embedder regardless of what `memory.embedding_provider` says, since
/// that toggle is a stronger statement of intent than the per-section default.
///
/// Note: this is the *intended* setting. It does not check whether the Ollama
/// daemon is actually running. For the live, health-checked variant that
/// falls back to cloud when Ollama is configured but unreachable, see
/// [`effective_embedding_settings_probed`].
pub fn effective_embedding_settings(
    memory: &MemoryConfig,
    local_embedding_model: Option<&str>,
) -> (String, String, usize) {
    if let Some(raw) = local_embedding_model {
        // Trim once and reuse — the emptiness check and the final model
        // string must agree, otherwise a value like "  bge-m3  " would pass
        // through to Ollama with surrounding whitespace and 404.
        let trimmed = raw.trim();
        let model = if trimmed.is_empty() {
            DEFAULT_OLLAMA_MODEL.to_string()
        } else {
            trimmed.to_string()
        };
        return ("ollama".to_string(), model, DEFAULT_OLLAMA_DIMENSIONS);
    }
    (
        memory.embedding_provider.clone(),
        memory.embedding_model.clone(),
        memory.embedding_dimensions,
    )
}

/// The **active embedding signature** — the canonical key every per-model
/// sidecar read/write is scoped by (#1574).
///
/// Derived from [`effective_embedding_settings`] (the *intended*, non-probed
/// selection) — deliberately **not** [`effective_embedding_settings_probed`].
/// A transient Ollama-down fallback to cloud must never silently redefine the
/// signature: that would re-key every read at a different space and trigger a
/// spurious full re-embed on the next cold-Ollama launch (spec §3 oscillation
/// guard). The string is produced by [`format_embedding_signature`], the same
/// formatter [`EmbeddingProvider::signature`] uses, so a config-derived
/// signature is byte-identical to a live provider's.
pub fn active_embedding_signature(
    memory: &MemoryConfig,
    local_embedding_model: Option<&str>,
) -> String {
    let (provider, model, dims) = effective_embedding_settings(memory, local_embedding_model);
    format_embedding_signature(&provider, &model, dims)
}

/// Async, health-checked variant of [`effective_embedding_settings`].
///
/// If the intended provider is `"ollama"` but the daemon doesn't respond at
/// `<base_url>/api/tags` within a short timeout, this falls back to the cloud
/// embedder and logs a single warning. This avoids the failure mode behind
/// OPENHUMAN-TAURI-B7: a user who's flipped `local_ai.usage.embeddings = true`
/// in Settings but doesn't actually have Ollama running ends up firing one
/// `ollama_embed` Sentry event per embed call (226+ events in a day with zero
/// impacted users — pure noise that drowns out real signals). With this
/// gate, embed calls never even reach `OllamaEmbedding` in that state; the
/// cloud embedder serves the session and the user gets a working app.
///
/// The probe deliberately uses a 2s timeout — long enough to tolerate a
/// briefly-busy daemon, short enough to not block startup if Ollama is
/// genuinely down.
pub async fn effective_embedding_settings_probed(
    memory: &MemoryConfig,
    local_embedding_model: Option<&str>,
) -> (String, String, usize) {
    let intended = effective_embedding_settings(memory, local_embedding_model);
    if intended.0 != "ollama" {
        return intended;
    }
    let base_url = ollama_base_url_for_probe();
    if probe_ollama_reachable(&base_url).await {
        log::debug!(
            "[memory::factory] ollama healthy at {base_url}; using local embeddings (model={}, dims={})",
            intended.1,
            intended.2,
        );
        return intended;
    }
    // Ollama is configured but not reachable. Report once per process at this
    // gate so a genuine misconfiguration still surfaces in Sentry — but no
    // more than once, so re-instantiating memory across agents/sessions
    // doesn't recreate the per-embed flood we're fixing. Then fall back to
    // cloud so the user has a working app.
    log::warn!(
        "[memory::factory] ollama unreachable at {base_url} (model={}); falling back to cloud embedder for this session",
        intended.1
    );
    report_ollama_health_gate_once(&base_url, &intended.1);
    cloud_embedding_fallback()
}

/// Returns the effective name of the memory backend being used.
///
/// Currently, this always returns "namespace" as the unified memory system
/// is the standard.
pub fn effective_memory_backend_name(
    _memory_backend: &str,
    _storage_provider: Option<&StorageProviderConfig>,
) -> String {
    "namespace".to_string()
}

/// Create a standard memory instance based on the provided configuration.
pub fn create_memory(
    config: &MemoryConfig,
    workspace_dir: &Path,
) -> anyhow::Result<Box<dyn Memory>> {
    // No `Config` in scope here (tests + migration), so no credential store to
    // read — pass an empty key. Callers that select a keyed BYO provider must
    // use `create_memory_with_local_ai`, which resolves the stored credential.
    create_memory_full(config, &[], None, None, "", workspace_dir)
}

/// Bind this crate's own store as a [`MemoryProvider`], the way every other
/// engine in the workspace is bound.
///
/// Issue #18 §A3/§A5. Until this existed, `tinymemory-core`'s
/// [`UnifiedMemory`] was the one storage
/// implementation in the workspace with no route through the contract: the
/// TinyCortex adapter binds the bundled engine, `tinymemory-remote` binds the
/// three hosted services, and core's own SQLite store was reachable only by
/// naming its concrete type. A host could therefore not treat "the store
/// tinymemory ships with" as a driver, which is the whole premise of the
/// registry — and `create_memory` returning `Box<dyn Memory>` is exactly the
/// bypass §A3 names.
///
/// Nothing structural was missing, which is worth recording because it was
/// mis-diagnosed at one point as needing the crate split: `UnifiedMemory`
/// already implements [`Memory`], and
/// [`MemoryTraitProvider`] already wraps any `Memory` into a provider. This is
/// the one-line composition the adapters have been doing all along.
///
/// # Capabilities
///
/// The returned provider advertises the mandatory three — Core, Recall,
/// Portability — and nothing else, because that is what [`Memory`] can express.
/// `UnifiedMemory` implements more than that internally (trees, chunks,
/// entities), but those reach the caller through their own concrete APIs rather
/// than through an optional family accessor, so advertising them here would be
/// a claim `audit_provider` correctly rejects. Widening that is §C3's shape of
/// work, not this function's.
///
/// # Errors
///
/// Propagates whatever [`create_memory`] fails with — a store that cannot open
/// cannot be bound.
///
/// [`MemoryProvider`]: tinymemory_api::provider::MemoryProvider
/// [`Memory`]: tinymemory_api::traits::Memory
/// [`MemoryTraitProvider`]: tinymemory_api::mandatory::MemoryTraitProvider
pub fn create_memory_provider(
    config: &MemoryConfig,
    workspace_dir: &Path,
) -> anyhow::Result<Arc<dyn tinymemory_api::provider::MemoryProvider>> {
    let memory = create_memory(config, workspace_dir)?;
    Ok(bind_as_provider(memory))
}

/// Wraps an already-built store as a driver under [`NAMESPACE_DRIVER_ID`].
///
/// Split out from [`create_memory_provider`] so a caller that already holds a
/// store — the migration path, tests, a host that built one through
/// [`create_memory_with_local_ai`] — can bind it without constructing a second
/// one. Constructing twice against one directory is the hazard the host-side
/// bypass allowlists exist to refuse, so the seam that avoids it belongs here
/// rather than at each call site.
///
/// [`NAMESPACE_DRIVER_ID`]: tinymemory_api::drivers::NAMESPACE_DRIVER_ID
#[must_use]
pub fn bind_as_provider(
    memory: Box<dyn Memory>,
) -> Arc<dyn tinymemory_api::provider::MemoryProvider> {
    Arc::new(tinymemory_api::mandatory::MemoryTraitProvider::new(
        Arc::from(memory),
        tinymemory_api::drivers::NAMESPACE_DRIVER_ID,
    ))
}

/// Create a memory instance honouring the unified per-workload embedding
/// provider.
///
/// `local_embedding_model` is the parsed Ollama model id when
/// `Config::workload_local_model("embeddings")` returned `Some`, otherwise
/// `None`. Used by top-level entry points (agent harness, channels runtime)
/// that have the full `Config` in scope. The local-AI opt-in flips the
/// embedder to Ollama when `Some`.
///
/// `embedding_api_key` is the user's stored credential for the selected BYO
/// embedding provider, resolved by the caller via
/// the host's `EmbeddingHost::resolve_api_key` (empty string when none is
/// configured). It is threaded into the keyed providers (cohere/openai/voyage/
/// custom) so they authenticate instead of sending an empty bearer; cloud /
/// managed / ollama / none ignore it.
pub fn create_memory_with_local_ai(
    memory: &MemoryConfig,
    local_embedding_model: Option<&str>,
    embedding_api_key: &str,
    embedding_routes: &[EmbeddingRouteConfig],
    storage_provider: Option<&StorageProviderConfig>,
    workspace_dir: &Path,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory_full(
        memory,
        embedding_routes,
        storage_provider,
        local_embedding_model,
        embedding_api_key,
        workspace_dir,
    )
}

/// Memory resources needed by an agent session.
///
/// The storage abstraction remains backend-neutral while SQLite-specific
/// consumers receive the concrete shared connection explicitly.
pub struct SessionMemory {
    pub memory: Box<dyn Memory>,
    pub sqlite_connection: Arc<Mutex<Connection>>,
}

pub fn create_session_memory_with_local_ai(
    memory: &MemoryConfig,
    local_embedding_model: Option<&str>,
    embedding_api_key: &str,
    embedding_routes: &[EmbeddingRouteConfig],
    storage_provider: Option<&StorageProviderConfig>,
    workspace_dir: &Path,
    // Memory subdirectory under `workspace_dir` — `"memory"` for the shared
    // tree, `"memory-<id>"` for a profile that opted into dedicated memory
    // (derived by the session builder from `effective_memory_suffix`). Routes
    // the session's captures + recall (the `UnifiedMemory` SQLite store) into the
    // profile's own subtree so `dedicatedMemory` isolation actually takes effect.
    memory_subdir: &str,
) -> anyhow::Result<SessionMemory> {
    let memory = create_unified_memory_full(
        memory,
        embedding_routes,
        storage_provider,
        local_embedding_model,
        embedding_api_key,
        workspace_dir,
        memory_subdir,
    )?;
    let sqlite_connection = Arc::clone(&memory.conn);
    Ok(SessionMemory {
        memory: Box::new(memory),
        sqlite_connection,
    })
}

/// Synchronous health-check shim around [`probe_ollama_reachable`].
///
/// Production call sites (`create_memory_with_local_ai` and friends) live in
/// sync code that doesn't want to plumb `async` through the whole agent
/// harness builder chain. They always run inside a multi-thread tokio
/// runtime (the core's main runtime), so we can park the worker via
/// [`tokio::task::block_in_place`] and drive the probe future to completion.
///
/// When no tokio runtime is available OR the runtime is single-threaded
/// (current-thread flavour), we skip the probe entirely and assume the
/// daemon is reachable. `block_in_place` panics on a current-thread runtime
/// — see <https://docs.rs/tokio/latest/tokio/task/fn.block_in_place.html> —
/// so probing in that context would crash the caller. Skipping preserves
/// the pre-health-gate behaviour (which is what tests rely on) and is safe
/// because the existing `OllamaEmbedding` error path still surfaces a
/// transport failure if the daemon truly is down.
fn probe_ollama_reachable_blocking(base_url: &str) -> bool {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        log::debug!(
            "[memory::factory] probe_ollama_reachable_blocking: no tokio runtime in context; skipping probe"
        );
        return true;
    };
    if !matches!(
        handle.runtime_flavor(),
        tokio::runtime::RuntimeFlavor::MultiThread
    ) {
        log::debug!(
            "[memory::factory] probe_ollama_reachable_blocking: runtime is current-thread (block_in_place would panic); skipping probe"
        );
        return true;
    }
    tokio::task::block_in_place(move || handle.block_on(probe_ollama_reachable(base_url)))
}

/// The most comprehensive factory function for creating a memory instance.
///
/// This function resolves the embedding provider — applying the Ollama
/// health-gate when the user has opted into local embeddings — then
/// initializes the provider and creates a `UnifiedMemory` instance.
fn create_memory_full(
    config: &MemoryConfig,
    _embedding_routes: &[EmbeddingRouteConfig],
    _storage_provider: Option<&StorageProviderConfig>,
    local_embedding_model: Option<&str>,
    embedding_api_key: &str,
    workspace_dir: &Path,
) -> anyhow::Result<Box<dyn Memory>> {
    Ok(Box::new(create_unified_memory_full(
        config,
        _embedding_routes,
        _storage_provider,
        local_embedding_model,
        embedding_api_key,
        workspace_dir,
        // Non-session callers (migration, standalone memory) always use the
        // shared default subtree.
        "memory",
    )?))
}

fn create_unified_memory_full(
    config: &MemoryConfig,
    _embedding_routes: &[EmbeddingRouteConfig],
    _storage_provider: Option<&StorageProviderConfig>,
    local_embedding_model: Option<&str>,
    embedding_api_key: &str,
    workspace_dir: &Path,
    memory_subdir: &str,
) -> anyhow::Result<UnifiedMemory> {
    // 1. Resolve the intended provider from config.
    let intended = effective_embedding_settings(config, local_embedding_model);
    let local_ai_opt_in = local_embedding_model
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);

    // 2. Health-gate: if the user has opted into Ollama embeddings but the
    //    daemon isn't reachable, fall back to cloud for this session.
    //    Prevents OPENHUMAN-TAURI-B7's 226-event Sentry flood: instead of
    //    one Sentry event per embed attempt, we report once at the gate
    //    (low cardinality, high signal) and serve the session from cloud.
    let gate_triggered;
    let (provider, model, dims) = if intended.0 == "ollama" {
        let base_url = ollama_base_url_for_probe();
        if probe_ollama_reachable_blocking(&base_url) {
            log::debug!(
                "[memory::factory] ollama healthy at {base_url}; using local embeddings (model={}, dims={})",
                intended.1,
                intended.2,
            );
            gate_triggered = false;
            intended
        } else {
            log::warn!(
                "[memory::factory] ollama unreachable at {base_url} (model={}); falling back to cloud embedder for this session",
                intended.1
            );
            report_ollama_health_gate_once(&base_url, &intended.1);
            gate_triggered = true;
            cloud_embedding_fallback()
        }
    } else {
        gate_triggered = false;
        intended
    };

    log::debug!(
        "[memory::factory] effective embedding settings: provider={provider} model={model} dims={dims} \
         (local_ai_opt_in={local_ai_opt_in} gate_triggered={gate_triggered})",
    );

    // 3. Create the embedding provider, threading the user's stored BYO
    //    credential. The keyless `create_embedding_provider` left the API key
    //    empty for *every* provider, so a user who selected Cohere — even with
    //    a valid key configured — sent an empty `Bearer ` and got a guaranteed
    //    401 "no api key supplied" on every embed (TAURI-RUST-52S), and the
    //    same gap silently broke BYO OpenAI / Voyage memory embeddings.
    //    cloud/managed/ollama/none ignore the key; the keyed providers now
    //    actually receive it. `embedding_api_key` is "" when no credential is
    //    stored, which the per-provider guards reject fast. A `custom:<url>`
    //    provider keeps its inline endpoint (the factory's `custom:` arm strips
    //    the prefix), so `custom_endpoint` stays `None` here. The key is never
    //    logged — the warning carries only provider/model/dims.
    let embedder: Arc<dyn EmbeddingProvider> = Arc::from(
        require_embedding_host()
        .map_err(anyhow::Error::msg)?
        .create_embedding_provider_with_credentials(
            &provider,
            &model,
            dims,
            embedding_api_key,
            None,
        )
        .inspect_err(|err| {
            log::warn!(
                "[memory::factory] create_embedding_provider_with_credentials failed provider={provider} model={model} dims={dims}: {err}",
            );
        })
        .map_err(anyhow::Error::msg)?,
    );

    // 4. Instantiate UnifiedMemory which handles SQLite and vector storage,
    //    rooted at the caller-selected subtree (`memory` shared, `memory-<id>`
    //    for a dedicated-memory profile).
    UnifiedMemory::new_with_memory_dir(
        workspace_dir,
        memory_subdir,
        embedder,
        config.sqlite_open_timeout_secs,
    )
}

/// Create the high-level memory client used by the compiled TinyMemory module.
///
/// Unlike [`crate::store::MemoryClient::from_workspace_dir`], this preserves
/// the caller's resolved embedding and storage configuration. The returned
/// client and its raw [`Memory`] handle share one `UnifiedMemory` instance and
/// one ingestion worker.
pub fn create_memory_client_with_local_ai(
    memory: &MemoryConfig,
    local_embedding_model: Option<&str>,
    embedding_api_key: &str,
    embedding_routes: &[EmbeddingRouteConfig],
    storage_provider: Option<&StorageProviderConfig>,
    workspace_dir: &Path,
) -> anyhow::Result<crate::store::MemoryClient> {
    let store = create_unified_memory_full(
        memory,
        embedding_routes,
        storage_provider,
        local_embedding_model,
        embedding_api_key,
        workspace_dir,
        "memory",
    )?;
    Ok(crate::store::MemoryClient::from_unified_memory(store))
}

/// Like [`create_memory_client_with_local_ai`], but rooted at an explicit
/// memory subdirectory instead of the shared `"memory"` tree.
///
/// Exists for the module's `OpenStore`: a host with per-profile memory needs
/// more than one store in a process, and each one is an ordinary client rooted
/// at `<workspace>/<memory_subdir>`. Kept as a separate entry point rather than
/// adding a parameter to the function above, because every existing caller
/// wants the shared tree and a defaulted subdir argument is the kind of thing
/// that silently routes a store somewhere nobody intended.
///
/// # Errors
///
/// Propagates whatever opening the store under `memory_subdir` failed with.
pub fn create_memory_client_in_subdir(
    memory: &MemoryConfig,
    local_embedding_model: Option<&str>,
    embedding_api_key: &str,
    embedding_routes: &[EmbeddingRouteConfig],
    storage_provider: Option<&StorageProviderConfig>,
    workspace_dir: &Path,
    memory_subdir: &str,
) -> anyhow::Result<crate::store::MemoryClient> {
    let store = create_unified_memory_full(
        memory,
        embedding_routes,
        storage_provider,
        local_embedding_model,
        embedding_api_key,
        workspace_dir,
        memory_subdir,
    )?;
    Ok(crate::store::MemoryClient::from_unified_memory(store))
}

/// Create a memory instance specifically for migration purposes.
///
/// The unified namespace memory core has a single workspace-scoped
/// store, so migration writes into the same `UnifiedMemory` instance the
/// rest of the app reads from — there is no separate "migration
/// backend". This helper delegates to [`create_memory`] so the
/// migration importer (`migrate_openclaw_memory`) gets a real, writable
/// memory handle and the Apply path can actually run end-to-end.
///
/// Prior to #1440 this function unconditionally bailed with "memory
/// migration is disabled for the unified namespace memory core", which
/// left the OpenClaw importer broken even though the rest of the
/// pipeline (source discovery, dry-run report, backup) worked.
pub fn create_memory_for_migration(
    config: &MemoryConfig,
    workspace_dir: &Path,
) -> anyhow::Result<Box<dyn Memory>> {
    create_memory(config, workspace_dir)
}

#[cfg(test)]
#[path = "factories_tests.rs"]
mod tests;
