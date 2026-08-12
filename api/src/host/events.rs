//! [`MemoryEventSink`] — the events the memory subsystem announces.
//!
//! # Why the host's event enum does not move
//!
//! The host's `DomainEvent` is a single flat enum covering agents, channels,
//! cron, tools, webhooks and the system domain as well as memory. It is the
//! host's own vocabulary; a *memory* crate must not own it, and importing it
//! would make every other subsystem's events a transitive dependency of memory.
//!
//! So the seam runs the other way. This module defines the ~15 memory-domain
//! events the extracted code emits, as a small enum of plain data. The host
//! implements [`MemoryEventSink`] by mapping each variant onto the matching
//! `DomainEvent` and publishing it on its own bus. The core publishes into the
//! sink and never learns that a bus exists.
//!
//! # Subscribing is not part of this seam
//!
//! Several extracted modules used to *subscribe* as well as publish
//! (`sync_events.rs`, `sync/composio/bus.rs`, `conversations/bus.rs`). Those are
//! host wiring by the repository README's split — event-bus subscribers belong
//! in the host, next to the registration site that installs them. They move back
//! rather than growing a subscribe method here.

/// Why an embedding model was reported unhealthy, and what took over.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmbeddingHealthReason {
    /// The provider that failed.
    pub provider: String,
    /// The model that failed.
    pub model: String,
    /// The provider bound in its place.
    pub fallback_provider: String,
    /// Operator-facing explanation. Never carries credentials.
    pub message: String,
}

/// What kicked off a sync run — a schedule, a user action, a webhook.
pub type SyncTrigger = String;

/// A memory-domain event, as announced by `tinymemory-core`.
///
/// Field names and types mirror the host's own event payloads exactly, so the
/// host's [`MemoryEventSink`] impl is a straight structural mapping with no
/// judgement calls in it.
///
/// Deliberately **not** `#[non_exhaustive]`: the host's mapping impl matches
/// exhaustively on purpose, so adding a variant here is a compile error at the
/// mapping site rather than an event that silently never reaches the bus.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum MemoryEvent {
    /// A sync run moved to a new stage.
    SyncStageChanged {
        /// What started this run.
        trigger: SyncTrigger,
        /// The stage just entered.
        stage: String,
        /// Provider slug, when the stage is provider-scoped.
        provider: Option<String>,
        /// Connection id, when the stage is connection-scoped.
        connection_id: Option<String>,
        /// Free-form operator detail.
        detail: Option<String>,
        /// Memory-source id, when the stage is source-scoped.
        source_id: Option<String>,
    },
    /// A document entered the ingestion pipeline.
    IngestionStarted {
        /// Document being ingested.
        document_id: String,
        /// Human-readable document title.
        title: String,
        /// Target namespace.
        namespace: String,
        /// Items still queued behind this one.
        queue_depth: usize,
    },
    /// A document left the ingestion pipeline.
    IngestionCompleted {
        /// Document that was ingested.
        document_id: String,
        /// Target namespace.
        namespace: String,
        /// Whether ingestion succeeded.
        success: bool,
        /// Wall-clock duration.
        elapsed_ms: u64,
        /// Items still queued afterwards.
        queue_depth: usize,
    },
    /// A source document was canonicalized into chunks.
    DocumentCanonicalized {
        /// Source the document came from.
        source_id: String,
        /// Source kind (`gmail`, `slack`, `file`, …).
        source_kind: String,
        /// How many chunks were written.
        chunks_written: usize,
        /// Ids of the written chunks.
        chunk_ids: Vec<String>,
        /// Unix timestamp, seconds with fraction.
        canonicalized_at: f64,
        /// Truncated body preview for operator UIs.
        body_preview: Option<String>,
    },
    /// An hour bucket was sealed and summarized.
    TreeSummarizerHourCompleted {
        /// Tree namespace.
        namespace: String,
        /// Node that was sealed.
        node_id: String,
        /// Tokens in the produced summary.
        token_count: u32,
    },
    /// A summary was propagated up a level.
    TreeSummarizerPropagated {
        /// Tree namespace.
        namespace: String,
        /// Node that received the propagated summary.
        node_id: String,
        /// Level name.
        level: String,
        /// Tokens in the produced summary.
        token_count: u32,
    },
    /// A full tree rebuild finished.
    TreeSummarizerRebuildCompleted {
        /// Tree namespace.
        namespace: String,
        /// Nodes in the rebuilt tree.
        total_nodes: u64,
    },
    /// Progress ticks during a tree build, for the operator UI.
    TreeBuildProgress {
        /// Coarse phase name.
        phase: String,
        /// Fine step name.
        step: String,
        /// Which tree, when scoped.
        tree_scope: Option<String>,
        /// Tree level, when levelled.
        level: Option<u32>,
        /// Items processed in this step.
        item_count: Option<u32>,
        /// Free-form operator detail.
        detail: Option<String>,
    },
    /// An embedding model failed health checks and a fallback was bound.
    EmbeddingModelUnhealthy(EmbeddingHealthReason),
    /// The configured memory driver could not be bound, and another was used.
    DriverBindFailed {
        /// Driver named in config.
        configured_driver: String,
        /// Driver actually bound.
        bound_driver: String,
        /// Why the configured driver was rejected.
        reason: String,
    },
    /// A diff snapshot was captured for a source.
    DiffSnapshotTaken {
        /// The new snapshot.
        snapshot_id: String,
        /// Source the snapshot covers.
        source_id: String,
        /// Source kind.
        source_kind: String,
        /// Items in the snapshot.
        item_count: usize,
        /// What triggered the snapshot.
        trigger: String,
    },
    /// Diffs were acknowledged by the user.
    DiffMarkedRead {
        /// Sources marked read.
        source_ids: Vec<String>,
        /// Snapshots marked read.
        snapshot_ids: Vec<String>,
    },
    /// The set of connected Composio toolkits changed.
    ComposioIntegrationsChanged {
        /// Toolkit slugs now connected.
        toolkits: Vec<String>,
    },
    /// The memory subsystem is asking for a sync run.
    SyncRequested {
        /// Channel to report progress back on, when the request came from one.
        channel_id: Option<String>,
    },
    /// The local embedding runtime is unusable and the user must act outside
    /// the app (start Ollama, pull the model).
    ///
    /// The host surfaces this in its durable user-error centre. Carries no
    /// provider text, model id or endpoint — see [`LOCAL_MODEL_UNAVAILABLE_KIND`].
    LocalModelUnavailable {
        /// Short, non-sensitive tag naming which producer fired
        /// (`health_gate` / `embed_classify`), so the two paths stay
        /// distinguishable in the log without a correlation id.
        origin: String,
    },
}

/// Stable `error_type` token for the local-embedding-runtime user error.
///
/// Mirrors the frontend `UserErrorKind` discriminator of the same name. It is
/// defined in the contract crate because both sides name it: the host builds
/// the wire payload from it, and the core's tests assert on it. A drift on
/// either side drops the signal silently.
pub const LOCAL_MODEL_UNAVAILABLE_KIND: &str = "local_model_unavailable";

/// `error_source` for the memory subsystem's user errors. Drives the panel's
/// scope grouping (`socketService` maps it to the `memory` `UserErrorScope`).
pub const MEMORY_USER_ERROR_SOURCE: &str = "memory";

/// Receives [`MemoryEvent`]s and does something host-shaped with them.
pub trait MemoryEventSink: Send + Sync + std::fmt::Debug {
    /// Announce an event. Implementations must not block and must not fail —
    /// an event bus that can reject a publish turns every emit site into an
    /// error path, which is not what any of the call sites want.
    fn publish(&self, event: MemoryEvent);
}

/// The sink bound when no host has installed one — in unit tests, in the
/// standalone engine build, and before startup wiring runs. Drops everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopEventSink;

impl MemoryEventSink for NoopEventSink {
    fn publish(&self, _event: MemoryEvent) {}
}
