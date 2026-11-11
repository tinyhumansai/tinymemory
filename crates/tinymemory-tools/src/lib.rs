//! The agent-facing side of TinyMemory, over any
//! [`tinymemory_api::MemoryEngine`].
//!
//! - [`tools`] offers a model seven memory tools (recall, fetch, list, get,
//!   explore, store, forget) as runtime-neutral [`ToolSpec`]s, and runs them
//!   through [`MemoryTools::call`]. The namespace a model writes to and the
//!   reach it reads with are fixed by the host in a [`ToolScope`]; no tool
//!   argument can name either.
//! - tools::single offers a configurable one-name facade for recall, fetch,
//!   learn and forget, plus the host-facing pending-write adapter.
//! - tools::budget provides per-run call and latency limits using a
//!   host-supplied clock.
//! - [`context`] compiles `context.md`, a token-budgeted brief a host injects
//!   at the start of a session.
//! - [`recall`] is the one read every lifecycle step is built from: a
//!   holistic recall across several scopes, rendered as one budgeted
//!   [`ContextPack`]. `context.md` is one preset of it.
//! - [`layout`] is the standard memory tree — a global **brain** of
//!   documents by source type, each agent's **conversations**, and shared
//!   **learnings** — as namespaces and filters ([`MemoryLayout`]).
//! - [`lifecycle`] runs one agent's memory through the agent loop
//!   ([`AgentMemory`]): session start, pre-turn context, post-turn logging,
//!   compaction recall. [`brain`] ingests and searches documents
//!   ([`Brain`]), and [`background`] runs the slow work both hand back
//!   ([`BackgroundJob`]).
//!
//! - [`layout_migration`] moves a legacy tree into the per-user layout,
//!   page by page, resumably, over two engines and two host ports.
//!
//! Everything is written against [`tinymemory_api::MemoryEngine`] alone, so a
//! host moves between engines by changing the one it constructs.
//!
//! # Example
//!
//! ```
//! use std::sync::Arc;
//! use serde_json::json;
//! use tinymemory_api::conformance::ReferenceEngine;
//! use tinymemory_api::Namespace;
//! use tinymemory_tools::{MEMORY_RECALL, MEMORY_STORE, MemoryTools};
//!
//! # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
//! # runtime.block_on(async {
//! let engine = Arc::new(ReferenceEngine::new());
//! let tools = MemoryTools::new(engine).placed_at(Namespace::agent("researcher"));
//!
//! // Hand these to the tool runtime: name, description, JSON Schema.
//! let names: Vec<&str> = tools.specs().iter().map(|spec| spec.name).collect();
//! assert!(names.contains(&MEMORY_STORE));
//!
//! // Run what the model asked for.
//! tools
//!     .call(MEMORY_STORE, json!({
//!         "learning": { "text": "The user prefers short answers", "learning_kind": "preference" },
//!         "tags": ["style"]
//!     }))
//!     .await?;
//! let answer = tools
//!     .call(MEMORY_RECALL, json!({ "question": "How long should answers be?" }))
//!     .await?;
//! assert_eq!(answer["citations"][0]["meta"]["tags"], json!(["style"]));
//!
//! // Read-only tools neither list nor run the write tools.
//! let reader = tools.clone().read_only();
//! assert!(reader.specs().iter().all(|spec| spec.name != MEMORY_STORE));
//! assert!(reader.call(MEMORY_STORE, json!({})).await.is_err());
//! # Ok::<(), tinymemory_api::Error>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod backfill;
pub mod background;
pub mod brain;
pub mod confine;
pub mod context;
pub mod deletion;
pub mod layout;
pub mod layout_migration;
pub mod lifecycle;
pub mod list_store;
pub mod ops;
pub mod pending;
pub mod prefetch;
pub mod queue;
pub mod recall;
pub mod run_slots;
pub mod tools;

pub use background::{BackgroundJob, BackgroundRunner, JobOutcome, JobReport};
pub use brain::{Brain, BrainBatch, BrainDocument, BrainSourceCount, BrainSourcesView, Ingested};
pub use confine::{Confinement, Policy};
pub use layout::identity::{
    AgentPin, Identity, IdentityPins, ResolvedIdentity, account_root, actor_of_root, chat_node,
    org_of_recorded, validate_root,
};
pub use layout::{BrainSource, CoreScope, DEFAULT_CORE_LIMIT, MemoryLayout};
pub use lifecycle::date_hint::DateHintModel;
pub use lifecycle::turn::{
    CitationSink, Clock, CompactionOptions, EngineResolver, JobsSink, LoggedToolCall,
    PreTurnOptions, TurnHooks, TurnPack, logged_reply, user_turn_index,
};
pub use lifecycle::window::InPromptFrom;
pub use lifecycle::{
    AgentCount, AgentMemory, Compaction, PostTurn, PostTurnReport, PreTurn, Preview, PreviewMode,
    RecallPolicy, RecallSettings, SessionStart, TurnContext,
};
pub use recall::{
    ContextPack, HolisticRecall, ScopeSection, SectionQuery, holistic_recall, holistic_recall_dated,
};
pub use tools::budget::{Clock as BudgetClock, DenialReason, Refusal, RunBudget};
pub use tools::single::{
    CallContext, ConcurrencyClass, PendingWriteSink, PermissionClass, RecallDetails, SingleTool,
    SingleToolOutcome, SingleToolSpec, WriteSink, concurrency_for, is_write_action, permission_for,
};
pub use tools::{
    MEMORY_EXPLORE, MEMORY_FETCH, MEMORY_FORGET, MEMORY_GET, MEMORY_LIST, MEMORY_RECALL,
    MEMORY_STORE, MemoryTools, TOOL_NAMES, ToolScope, ToolSpec, WRITE_TOOL_NAMES,
};
