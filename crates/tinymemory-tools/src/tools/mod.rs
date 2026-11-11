//! Agent-facing memory tools over any [`MemoryEngine`], with no tool-runtime
//! dependency.
//!
//! [`MemoryTools`] offers seven tools (see [`TOOL_NAMES`]): `memory_recall`,
//! `memory_fetch`, `memory_list`, `memory_get` and `memory_explore` read;
//! `memory_store` and `memory_forget` write. [`MemoryTools::specs`] describes
//! them as [`ToolSpec`]s (a name, a description, a JSON Schema) for a host to
//! hand its tool runtime, and [`MemoryTools::call`] runs one by name with the
//! JSON arguments the model produced, returning compact JSON.
//!
//! # Scoping
//!
//! Which memory node a model writes to and how far it reads are fixed by the
//! host in a [`ToolScope`], never chosen by the model:
//!
//! - Every stored item's namespace is the scope's `place`.
//! - Every read's reach is the scope's `reach`, overwriting anything else;
//!   `memory_get` passes it as [`tinymemory_api::GetRequest::reach`].
//! - `memory_forget` by ids reads the ids back under the reach first and
//!   forgets only those found, reporting the rest as `skipped`; by filter, the
//!   filter is confined to the reach.
//! - Arguments naming `namespace` or `reach`, at any depth, are refused with
//!   [`Error::InvalidRequest`] rather than ignored, and every schema sets
//!   `additionalProperties: false`.
//!
//! # Errors
//!
//! An unknown tool name and malformed arguments are
//! [`Error::InvalidRequest`], with a lowercase message naming the tool and
//! the field. A write tool called on read-only tools is
//! [`Error::Unsupported`]: the call is well formed, but these tools do not
//! offer the operation, which is what that variant means across the contract.

mod args;
pub mod budget;
mod read;
mod render;
pub mod single;
mod spec;
mod write;

use std::sync::Arc;

use serde_json::Value;
use tinymemory_api::{Error, MemoryEngine, MetaFilter, Namespace, Reach, Result};

pub use spec::{
    MEMORY_EXPLORE, MEMORY_FETCH, MEMORY_FORGET, MEMORY_GET, MEMORY_LIST, MEMORY_RECALL,
    MEMORY_STORE, TOOL_NAMES, ToolSpec, WRITE_TOOL_NAMES,
};

/// Where a model's memory tools write and how far they read, fixed by the
/// host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolScope {
    /// The node every stored item is written to.
    pub place: Namespace,
    /// The reach every read is confined to; `None` reads every namespace,
    /// which suits only a host whose engine serves a single tenant.
    pub reach: Option<Reach>,
    /// Whether `memory_store` and `memory_forget` are offered.
    pub writes: bool,
    /// The user's IANA time zone (`Asia/Kolkata`): the zone a model's
    /// `refers_to` dates are read in. `None` reads them in UTC.
    pub zone: Option<String>,
    /// The trusted workspace that produced a learning.
    pub workspace: Option<String>,
    /// The current thread, when these tools run inside one.
    pub thread_id: Option<String>,
    /// The agent that made the call.
    pub agent_id: Option<String>,
    /// The provider-assigned call name and id. The single facade preserves
    /// them; its configured tool name is used only when no call reference is
    /// set.
    pub tool_call: Option<tinymemory_api::ToolCallRef>,
}

impl Default for ToolScope {
    /// The root, reading every namespace, with writes enabled: a
    /// single-tenant host's scope.
    fn default() -> Self {
        Self {
            place: Namespace::ROOT,
            reach: None,
            writes: true,
            zone: None,
            workspace: None,
            thread_id: None,
            agent_id: None,
            tool_call: None,
        }
    }
}

impl ToolScope {
    /// The scope of an agent at `place`: writes land at `place`, and reads
    /// see `place` and its ancestors ([`Reach::of`]), never a sibling.
    #[must_use]
    pub fn at(place: Namespace) -> Self {
        Self {
            reach: Some(Reach::of(place.clone())),
            place,
            writes: true,
            zone: None,
            workspace: None,
            thread_id: None,
            agent_id: None,
            tool_call: None,
        }
    }

    /// Overwrites `filter.reach` with the scope's reach.
    pub(crate) fn confine(&self, filter: &mut MetaFilter) {
        filter.reach = self.reach.clone();
    }
}

/// The memory tools a host offers a model, over one engine and one
/// [`ToolScope`].
///
/// # Example
///
/// ```
/// use std::sync::Arc;
/// use serde_json::json;
/// use tinymemory_api::conformance::ReferenceEngine;
/// use tinymemory_api::Namespace;
/// use tinymemory_tools::MemoryTools;
///
/// # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
/// # runtime.block_on(async {
/// let tools = MemoryTools::new(Arc::new(ReferenceEngine::new()))
///     .placed_at(Namespace::agent("writer"));
/// let stored = tools
///     .call("memory_store", json!({ "learning": { "text": "prefers tabs" } }))
///     .await?;
/// assert_eq!(stored["replayed"], json!(false));
///
/// // A model cannot pick a namespace.
/// let escape = tools
///     .call("memory_list", json!({ "filter": { "namespace": "agent:other" } }))
///     .await;
/// assert!(escape.is_err());
/// # Ok::<(), tinymemory_api::Error>(())
/// # })?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[derive(Clone)]
pub struct MemoryTools {
    engine: Arc<dyn MemoryEngine>,
    scope: ToolScope,
}

impl std::fmt::Debug for MemoryTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryTools")
            .field("engine", &self.engine.descriptor().id)
            .field("scope", &self.scope)
            .finish()
    }
}

impl MemoryTools {
    /// Tools over `engine` with [`ToolScope::default`]: written to the root,
    /// reading every namespace, writes enabled. A multi-tenant host narrows
    /// this with [`MemoryTools::placed_at`] or [`MemoryTools::with_scope`].
    #[must_use]
    pub fn new(engine: Arc<dyn MemoryEngine>) -> Self {
        Self::with_scope(engine, ToolScope::default())
    }

    /// Tools over `engine` with an explicit scope.
    #[must_use]
    pub fn with_scope(engine: Arc<dyn MemoryEngine>, scope: ToolScope) -> Self {
        Self { engine, scope }
    }

    /// Writes land at `place`, and the reach becomes [`Reach::of`]`(place)`:
    /// `place` and its ancestors, never a sibling. Call
    /// [`MemoryTools::reach`] afterwards to read differently; placing resets
    /// any reach set before, so a placed scope is never left reading more
    /// than its own branch by accident.
    #[must_use]
    pub fn placed_at(mut self, place: Namespace) -> Self {
        let writes = self.scope.writes;
        let zone = self.scope.zone.take();
        let workspace = self.scope.workspace.take();
        let thread_id = self.scope.thread_id.take();
        let agent_id = self.scope.agent_id.take();
        let tool_call = self.scope.tool_call.take();
        self.scope = ToolScope {
            writes,
            zone,
            workspace,
            thread_id,
            agent_id,
            tool_call,
            ..ToolScope::at(place)
        };
        self
    }

    /// Reads the dates a model passes as `refers_to` in `zone`, the user's
    /// IANA time zone.
    #[must_use]
    pub fn in_zone(mut self, zone: impl Into<String>) -> Self {
        self.scope.zone = Some(zone.into());
        self
    }

    /// Confines every read (and every forget) to `reach`.
    #[must_use]
    pub fn reach(mut self, reach: Reach) -> Self {
        self.scope.reach = Some(reach);
        self
    }

    /// Leaves out `memory_store` and `memory_forget`: [`MemoryTools::specs`]
    /// omits them and [`MemoryTools::call`] refuses them.
    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.scope.writes = false;
        self
    }

    /// The scope the tools run in.
    #[must_use]
    pub fn scope(&self) -> &ToolScope {
        &self.scope
    }

    /// The tools to offer the model, reads first. The write tools appear only
    /// when writes are enabled; `memory_fetch`'s `mode` enum lists exactly the
    /// engine's [`tinymemory_api::EngineDescriptor::fetch_modes`], and the tool
    /// is left out for an engine serving none.
    #[must_use]
    pub fn specs(&self) -> Vec<ToolSpec> {
        spec::specs(&self.engine.descriptor().fetch_modes, self.scope.writes)
    }

    /// Runs the tool `name` with the model's JSON `args` and returns its
    /// compact JSON result. `null` args read as no arguments.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`] for an unknown tool, arguments that are
    ///   not an object, name `namespace` or `reach`, name a key the tool does
    ///   not take, or carry a missing, mistyped or out-of-range value.
    /// - [`Error::Unsupported`] for a write tool on read-only tools, and for
    ///   `memory_fetch` on an engine serving no fetch mode.
    /// - Whatever the engine returns for the request.
    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        if !TOOL_NAMES.contains(&name) {
            return Err(unknown_tool(name));
        }
        if spec::is_write_tool(name) && !self.scope.writes {
            return Err(Error::Unsupported(format!(
                "{name} is not offered: these memory tools are read-only"
            )));
        }
        let engine = self.engine.as_ref();
        let scope = &self.scope;
        match name {
            MEMORY_RECALL => read::recall(engine, scope, &args).await,
            MEMORY_FETCH => read::fetch(engine, scope, &args).await,
            MEMORY_LIST => read::list(engine, scope, &args).await,
            MEMORY_GET => read::get(engine, scope, &args).await,
            MEMORY_EXPLORE => read::explore(engine, scope, &args).await,
            MEMORY_STORE => write::store(engine, scope, &args).await,
            MEMORY_FORGET => write::forget(engine, scope, &args).await,
            _ => Err(unknown_tool(name)),
        }
    }
}

fn unknown_tool(name: &str) -> Error {
    Error::InvalidRequest(format!(
        "unknown memory tool `{name}`; expected one of {}",
        TOOL_NAMES.join(", ")
    ))
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
