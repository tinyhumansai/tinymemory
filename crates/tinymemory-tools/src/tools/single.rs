//! One configurable model-facing name for recall, fetch, learn, and forget.
//!
//! The facade reuses the argument shaping and engine reads in this module's
//! parent. Its writes are accepted only by an injected WriteSink; the
//! PendingWriteSink adapts that port to PendingWrites. The host still owns
//! queue partitions, the active binding and gate, drain scheduling and
//! execution, settlement notifications, and the exclusive barrier that makes
//! erasure wait for an in-flight engine write.

use std::sync::Arc;

use chrono::Utc;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tinymemory_api::{
    Citation, Error, FetchMode, LearningKind, MemoryMeta, MetaFilter, Namespace, Reach, RecallMode,
    SourceKind, SourceRef, StoreItem, ThreadWindow, ToolCallRef,
};

use crate::confine::{Confinement, Policy};
use crate::list_store::ListStore;
use crate::ops::{LearningOptions, MemoryOps};
use crate::pending::{Ack, Binding, Entry, PendingWrites, WriteGate, WriteOperation};
use crate::tools::args::{self, Args};
use crate::tools::spec::schema::{DEFAULT_CONFIDENCE, DEFAULT_LEARNING_KIND, LEARNING_KINDS};
use crate::tools::{MemoryTools, ToolScope};

const TOOL_NAME: &str = "memory";
const ACTIONS: [&str; 4] = ["recall", "fetch", "learn", "forget"];
const FETCH_TOP: usize = 5;
const LEARN_QUEUED: &str = "queued; not yet saved to memory";
const FORGET_QUEUED: &str = "queued; not yet removed from memory";

/// Whether an action reads memory or changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionClass {
    /// Recall and fetch only read memory.
    ReadOnly,
    /// Learn and forget change memory.
    Write,
}

/// Whether an action can safely run alongside another call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrencyClass {
    /// Independent reads can run together.
    Safe,
    /// Writes must be serialized by the host tool runtime.
    Exclusive,
}

/// Per-call context that changes read behavior without changing the trusted
/// identity carried by ToolScope.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CallContext {
    /// The calling thread's turns still present in the prompt, when known.
    pub thread_window: Option<ThreadWindow>,
}

/// The configurable tool name, description, and one action-discriminated
/// parameter schema.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SingleToolSpec {
    /// The name a host registers for this tool.
    pub name: String,
    /// Instructions shown to the model.
    pub description: String,
    /// The closed, action-discriminated JSON parameter schema.
    pub parameters: Value,
}

/// Full evidence returned alongside one successful recall action.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallDetails {
    /// The exact non-blank question the native read sent to the engine.
    pub question: String,
    /// Every citation returned by the engine, including its metadata.
    pub citations: Vec<Citation>,
}

/// The compact model-facing value and optional details of one successful call.
#[derive(Debug, Clone, PartialEq)]
pub struct SingleToolOutcome {
    /// The JSON value the ordinary [`SingleTool::call`] returns.
    pub value: Value,
    /// Present for every successful recall, including an empty recall.
    pub recall: Option<RecallDetails>,
}

/// Classifies an action for host permission enforcement.
///
/// Unknown, missing, non-string, and whitespace-padded actions default to the
/// restrictive write class. Classification uses the raw action string; call
/// dispatch separately trims valid action names.
#[must_use]
pub fn permission_for(args: &Value) -> PermissionClass {
    match action(args) {
        Some("recall" | "fetch") => PermissionClass::ReadOnly,
        _ => PermissionClass::Write,
    }
}

/// Classifies an action for host concurrency enforcement.
///
/// Unknown, missing, non-string, and whitespace-padded actions default to
/// exclusive. Classification uses the raw action string.
#[must_use]
pub fn concurrency_for(args: &Value) -> ConcurrencyClass {
    match action(args) {
        Some("recall" | "fetch") => ConcurrencyClass::Safe,
        _ => ConcurrencyClass::Exclusive,
    }
}

/// Whether `args` names a recognized write action after trimming whitespace.
///
/// Unknown, missing, and non-string actions are not writes, so hosts continue
/// to apply their ordinary call budget to those invalid actions.
#[must_use]
pub fn is_write_action(args: &Value) -> bool {
    matches!(action(args).map(str::trim), Some("learn" | "forget"))
}

/// A host prequeue policy hook for accepting a validated memory write.
///
/// Hosts can wrap [`PendingWriteSink`] with their existing scrub and policy
/// checks before delegating. The adapter itself only queues the operation;
/// it does not scrub or apply host-specific policy. Implementations return
/// only after the write is durable locally and do not report an engine write
/// as complete.
pub trait WriteSink: Send + Sync {
    /// Checks host write policy before the action arguments are parsed.
    ///
    /// Hosts can preserve refusal ordering here, such as reporting disabled
    /// memory before a malformed write request.
    ///
    /// # Errors
    ///
    /// The host's typed memory refusal when the write must not proceed.
    fn preflight(&self, _scope: &ToolScope) -> tinymemory_api::Result<()> {
        Ok(())
    }

    /// Durably queues one operation under the trusted scope.
    ///
    /// # Errors
    ///
    /// A host policy refusal, or [`Error::Unavailable`] when durable queueing
    /// fails.
    fn enqueue(&self, operation: WriteOperation, scope: &ToolScope) -> tinymemory_api::Result<Ack>;
}

/// A host-configured adapter from tool writes to PendingWrites.
///
/// The caller supplies one queue, active owner binding, and write gate. The
/// scope root comes from trusted reach.at, or place for an unrestricted
/// single-tenant scope. This adapter does not create a drain task or provide
/// the host's exclusive in-flight erase barrier. It also does not scrub or
/// enforce host-specific prequeue policy; wrap it in a [`WriteSink`] that
/// applies those checks before forwarding the operation.
pub struct PendingWriteSink<S: ListStore<Entry> + 'static> {
    queue: Arc<PendingWrites<S>>,
    gate: Arc<WriteGate>,
    binding: Binding,
}

impl<S: ListStore<Entry> + 'static> std::fmt::Debug for PendingWriteSink<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingWriteSink")
            .field("queue", &self.queue)
            .field("binding", &self.binding)
            .finish_non_exhaustive()
    }
}

impl<S: ListStore<Entry> + 'static> PendingWriteSink<S> {
    /// Binds tool writes to the host-selected queue, erasure gate, and owner.
    #[must_use]
    pub fn new(queue: Arc<PendingWrites<S>>, gate: Arc<WriteGate>, binding: Binding) -> Self {
        Self {
            queue,
            gate,
            binding,
        }
    }
}

impl<S: ListStore<Entry> + 'static> WriteSink for PendingWriteSink<S> {
    fn enqueue(&self, operation: WriteOperation, scope: &ToolScope) -> tinymemory_api::Result<Ack> {
        let scope_root = scope
            .reach
            .as_ref()
            .map(|reach| reach.at.clone())
            .unwrap_or_else(|| scope.place.clone());
        self.queue
            .enqueue(operation, scope_root, self.binding.clone(), &self.gate)
            .map_err(|error| Error::Unavailable(error.to_string()))
    }
}

/// One model-facing name that dispatches its action to TinyMemory.
pub struct SingleTool {
    name: String,
    tools: Option<MemoryTools>,
    scope: ToolScope,
    write_sink: Option<Arc<dyn WriteSink>>,
}

impl std::fmt::Debug for SingleTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SingleTool")
            .field("name", &self.name)
            .field("tools", &self.tools)
            .field("scope", &self.scope)
            .field("has_write_sink", &self.write_sink.is_some())
            .finish()
    }
}

impl SingleTool {
    /// One tool named name, over the engine and trusted scope in tools.
    #[must_use]
    pub fn new(name: impl Into<String>, tools: MemoryTools) -> Self {
        Self {
            name: name.into(),
            scope: tools.scope.clone(),
            tools: Some(tools),
            write_sink: None,
        }
    }

    /// One write-only tool that queues learn and forget actions without
    /// requiring a memory engine to be configured or available.
    ///
    /// The trusted scope and write sink are used by the same validation,
    /// metadata stamping, preflight, and durable acknowledgement path as
    /// [`SingleTool::new`]. Recall and fetch are unsupported by this facade.
    #[must_use]
    pub fn queued(name: impl Into<String>, scope: ToolScope, sink: Arc<dyn WriteSink>) -> Self {
        Self {
            name: name.into(),
            tools: None,
            scope,
            write_sink: Some(sink),
        }
    }

    /// Installs the host's durable write port. Without one, learn and forget
    /// return an error and never fall through to a direct engine write.
    #[must_use]
    pub fn with_write_sink(mut self, sink: Arc<dyn WriteSink>) -> Self {
        self.write_sink = Some(sink);
        self
    }

    /// The single tool's name, description, and parameter schema.
    #[must_use]
    pub fn spec(&self) -> SingleToolSpec {
        let (description, parameters) = if let Some(tools) = &self.tools {
            (
                concat!(
                    "Long-term memory. Set action to recall evidence for a question, fetch matching ",
                    "items five at a time, learn a fact or preference, or forget items by id. ",
                    "Recall before asking for information the user may already have shared. ",
                    "Learn and forget responses acknowledge durable local queueing; they do not ",
                    "mean a remote write has finished."
                )
                .to_string(),
                schema(tools.engine.descriptor().fetch_modes.as_slice()),
            )
        } else {
            (
                concat!(
                    "Long-term memory writes. Set action to learn a fact or preference, or ",
                    "forget items by id. Responses acknowledge durable local queueing; they ",
                    "do not mean a remote write has finished."
                )
                .to_string(),
                write_schema(),
            )
        };
        SingleToolSpec {
            name: self.name.clone(),
            description,
            parameters,
        }
    }

    /// Classifies an action for host permission enforcement.
    ///
    /// Unknown and missing actions default to the restrictive write class.
    #[must_use]
    pub fn permission_for(&self, args: &Value) -> PermissionClass {
        self::permission_for(args)
    }

    /// Classifies an action for host concurrency enforcement.
    ///
    /// Unknown and missing actions default to exclusive.
    #[must_use]
    pub fn concurrency_for(&self, args: &Value) -> ConcurrencyClass {
        self::concurrency_for(args)
    }

    /// Runs the action in args and returns its compact JSON result.
    ///
    /// The current thread in the trusted scope is authoritative. A matching
    /// prompt window excludes only those turns; a missing or mismatched
    /// window excludes the whole current thread.
    ///
    /// # Errors
    ///
    /// Invalid actions or arguments, reads rejected by the engine, writes
    /// rejected by the queue, and writes without an installed sink.
    pub async fn call(&self, args: Value, context: CallContext) -> tinymemory_api::Result<Value> {
        self.call_with_details(args, context)
            .await
            .map(|outcome| outcome.value)
    }

    /// Runs an action and returns its compact value plus full recall details.
    ///
    /// The compact value is byte-for-byte the value [`Self::call`] returns.
    /// Recall details are returned as data only: this method does not deliver
    /// citations or publish host events. The action name is trimmed for
    /// dispatch, while permission and concurrency classification use the raw
    /// argument so padded names remain conservatively classified.
    ///
    /// # Errors
    ///
    /// The same invalid-action, invalid-argument, engine, and write-queue
    /// errors as [`Self::call`].
    pub async fn call_with_details(
        &self,
        args: Value,
        context: CallContext,
    ) -> tinymemory_api::Result<SingleToolOutcome> {
        let raw_action =
            action(&args).ok_or_else(|| args::invalid(TOOL_NAME, "action is required"))?;
        let action = raw_action.trim();
        if !ACTIONS.contains(&action) {
            return Err(args::invalid(
                TOOL_NAME,
                &format!("unknown action {raw_action}; use recall, fetch, learn or forget"),
            ));
        }
        if matches!(action, "learn" | "forget") && !self.scope.writes {
            return Err(Error::Unsupported(
                "memory writes are not offered in this read-only scope".into(),
            ));
        }
        match action {
            "recall" => match self.tools.as_ref() {
                Some(_) => {
                    self.recall_with_details(&args, &context)
                        .await
                        .map(|(value, recall)| SingleToolOutcome {
                            value,
                            recall: Some(recall),
                        })
                }
                None => Err(Error::Unsupported(
                    "memory reads are not configured for this queued tool".into(),
                )),
            },
            "fetch" => match self.tools.as_ref() {
                Some(_) => self
                    .fetch(&args, &context)
                    .await
                    .map(|value| SingleToolOutcome {
                        value,
                        recall: None,
                    }),
                None => Err(Error::Unsupported(
                    "memory reads are not configured for this queued tool".into(),
                )),
            },
            "learn" => self.learn(&args).map(|value| SingleToolOutcome {
                value,
                recall: None,
            }),
            "forget" => self.forget(&args).map(|value| SingleToolOutcome {
                value,
                recall: None,
            }),
            _ => Err(args::invalid(TOOL_NAME, "unknown action")),
        }
    }

    async fn recall_with_details(
        &self,
        value: &Value,
        context: &CallContext,
    ) -> tinymemory_api::Result<(Value, RecallDetails)> {
        let mut value = action_args(value, false)?;
        let filter = take_filter(&mut value)?;
        let mut value = without_model_limit(value);
        if let Value::Object(object) = &mut value {
            object.remove("instructions");
        }
        let answer = super::read::recall_answer_with_filter(
            self.tools
                .as_ref()
                .ok_or_else(|| Error::Unsupported("memory reads are not configured".into()))?
                .engine
                .as_ref(),
            &self.scope,
            &value,
            filter,
            RecallMode::Evidence,
            None,
            calling_thread(&self.scope, context),
        )
        .await?;
        // The read parser has validated this non-blank string and passes it
        // unchanged to RecallRequest::question.
        let question = value["question"]
            .as_str()
            .ok_or_else(|| {
                args::invalid(TOOL_NAME, "`question` is required and must not be blank")
            })?
            .to_string();
        let citations = answer.citations;
        let item_count = citations.len();
        let value = if answer.answer.trim().is_empty() {
            json!({ "evidence": "", "note": "Nothing relevant is stored in memory." })
        } else {
            json!({
                "evidence": super::render::capped_evidence(&answer.answer),
                "items": item_count
            })
        };
        Ok((
            value,
            RecallDetails {
                question,
                citations,
            },
        ))
    }

    async fn fetch(&self, value: &Value, context: &CallContext) -> tinymemory_api::Result<Value> {
        let mut value = action_args(value, false)?;
        let filter = take_filter(&mut value)?;
        let value = without_model_limit(value);
        let page = super::read::fetch_page_first_with_filter(
            self.tools
                .as_ref()
                .ok_or_else(|| Error::Unsupported("memory reads are not configured".into()))?
                .engine
                .as_ref(),
            &self.scope,
            &value,
            filter,
            Some(FETCH_TOP),
            calling_thread(&self.scope, context),
        )
        .await?;
        let mut result = json!({
            "matches": super::render::fetch_page_text(&page),
            "items": page.hits.len()
        });
        if let Some(cursor) = page.next_cursor {
            result["next_cursor"] = json!(cursor);
        }
        Ok(result)
    }

    fn learn(&self, value: &Value) -> tinymemory_api::Result<Value> {
        let sink = self
            .write_sink
            .as_ref()
            .ok_or_else(|| Error::Unsupported("memory write queue is not configured".into()))?;
        sink.preflight(&self.scope)?;
        let item = self.learning_item(value)?;
        match sink.enqueue(WriteOperation::Learn { item }, &self.scope)? {
            Ack::Learned { id } => Ok(json!({ "id": id, "status": LEARN_QUEUED })),
            Ack::Forgotten { .. } => Err(Error::Unavailable(
                "memory queue returned an acknowledgement for the wrong operation".into(),
            )),
        }
    }

    fn forget(&self, value: &Value) -> tinymemory_api::Result<Value> {
        let sink = self
            .write_sink
            .as_ref()
            .ok_or_else(|| Error::Unsupported("memory write queue is not configured".into()))?;
        sink.preflight(&self.scope)?;
        let value = action_args(value, false)?;
        let args = Args::parse(TOOL_NAME, &value, &["ids"])?;
        let raw_ids = args.strings("ids")?;
        let mut ids: Vec<String> = raw_ids
            .into_iter()
            .map(|id| id.trim().to_owned())
            .filter(|id| !id.is_empty())
            .collect();
        if ids.is_empty() {
            return Err(args.field_error("ids", "must list at least one non-blank id"));
        }
        ids.sort();
        ids.dedup();
        let reach = self
            .scope
            .reach
            .clone()
            .unwrap_or_else(|| Reach::subtree(Namespace::ROOT));
        match sink.enqueue(WriteOperation::Forget { ids, reach }, &self.scope)? {
            Ack::Forgotten { queued } => Ok(json!({
                "queued": queued,
                "status": FORGET_QUEUED
            })),
            Ack::Learned { .. } => Err(Error::Unavailable(
                "memory queue returned an acknowledgement for the wrong operation".into(),
            )),
        }
    }

    fn learning_item(&self, value: &Value) -> tinymemory_api::Result<StoreItem> {
        let value = action_args(value, true)?;
        let args = Args::parse(
            TOOL_NAME,
            &value,
            &["text", "kind", "confidence", "evidence"],
        )?;
        let kind = args
            .string("kind")?
            .map(|kind| {
                serde_json::from_value::<LearningKind>(Value::String(kind)).map_err(|_| {
                    args.field_error(
                        "kind",
                        &format!("must be one of {}", LEARNING_KINDS.join(", ")),
                    )
                })
            })
            .transpose()?;
        let mut item = MemoryOps::new(confinement(&self.scope)).learning_item(LearningOptions {
            text: args.required_string("text")?,
            kind,
            confidence: if args.has("confidence") {
                Some(args.unit("confidence", DEFAULT_CONFIDENCE)?)
            } else {
                None
            },
            meta: None,
            host_meta: Some(self.learn_meta()),
            default_namespace: self.scope.place.clone(),
            now: Utc::now(),
        })?;
        if let StoreItem::Learning {
            evidence: stored_evidence,
            ..
        } = &mut item
        {
            *stored_evidence = args.string("evidence")?;
        }
        Ok(item)
    }

    fn learn_meta(&self) -> MemoryMeta {
        let call = self.scope.tool_call.clone().unwrap_or_else(|| ToolCallRef {
            name: self.name.clone(),
            id: None,
        });
        MemoryMeta {
            namespace: self.scope.place.clone(),
            workspace: self.scope.workspace.clone(),
            thread_id: self.scope.thread_id.clone(),
            agent_id: self.scope.agent_id.clone(),
            tool_call: Some(call),
            source: SourceRef {
                kind: SourceKind::Agent,
                id: None,
            },
            ..MemoryMeta::default()
        }
    }
}

fn action(value: &Value) -> Option<&str> {
    value.get("action").and_then(Value::as_str)
}

fn action_args(value: &Value, drop_meta: bool) -> tinymemory_api::Result<Value> {
    let Value::Object(mut object) = value.clone() else {
        return Err(args::invalid(TOOL_NAME, "arguments must be a json object"));
    };
    object.remove("action");
    if drop_meta {
        object.remove("meta");
    }
    Ok(Value::Object(object))
}

fn take_filter(value: &mut Value) -> tinymemory_api::Result<Option<MetaFilter>> {
    let filter = value
        .as_object_mut()
        .and_then(|object| object.remove("filter"))
        .filter(|filter| !filter.is_null());
    filter
        .map(|filter| {
            serde_json::from_value(filter)
                .map_err(|_| args::invalid(TOOL_NAME, "filter has an invalid shape"))
        })
        .transpose()
}

fn without_model_limit(mut value: Value) -> Value {
    if let Value::Object(object) = &mut value {
        object.remove("limit");
    }
    value
}

fn confinement(scope: &ToolScope) -> Confinement {
    Confinement {
        allowed: scope
            .reach
            .clone()
            .unwrap_or_else(|| Reach::subtree(Namespace::ROOT)),
        policy: Policy::Refuse,
    }
}

fn calling_thread(scope: &ToolScope, context: &CallContext) -> Option<ThreadWindow> {
    let thread_id = scope.thread_id.as_deref()?;
    Some(
        context
            .thread_window
            .as_ref()
            .filter(|window| window.thread_id == thread_id)
            .cloned()
            .unwrap_or_else(|| ThreadWindow::whole(thread_id)),
    )
}

fn schema(fetch_modes: &[FetchMode]) -> Value {
    let mut recall = super::spec::schema::recall();
    if let Some(properties) = recall["properties"].as_object_mut() {
        properties.remove("instructions");
        properties.remove("limit");
    }
    let mut fetch = super::spec::schema::fetch(fetch_modes);
    fetch["properties"]
        .as_object_mut()
        .map(|properties| properties.remove("limit"));
    if let Some(default) = fetch_modes.first() {
        fetch["properties"]["mode"]["default"] = json!(default.as_str());
    }
    replace_filter_schema(&mut recall);
    replace_filter_schema(&mut fetch);
    let branches = [
        branch("recall", recall),
        branch("fetch", fetch),
        branch("learn", learn_schema()),
        branch("forget", forget_schema()),
    ];
    let mut properties = Map::new();
    properties.insert(
        "action".into(),
        json!({
            "type": "string",
            "enum": ACTIONS,
            "description": "Which memory operation to perform."
        }),
    );
    for branch in &branches {
        if let Some(branch_properties) = branch["properties"].as_object() {
            for (key, value) in branch_properties {
                if key != "action" {
                    properties
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
            }
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": ["action"],
        "oneOf": branches,
        "additionalProperties": false
    })
}

fn write_schema() -> Value {
    let branches = [
        branch("learn", learn_schema()),
        branch("forget", forget_schema()),
    ];
    let mut properties = Map::new();
    properties.insert(
        "action".into(),
        json!({
            "type": "string",
            "enum": ["learn", "forget"],
            "description": "Which memory write operation to perform."
        }),
    );
    for branch in &branches {
        if let Some(branch_properties) = branch["properties"].as_object() {
            for (key, value) in branch_properties {
                if key != "action" {
                    properties
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
            }
        }
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": ["action"],
        "oneOf": branches,
        "additionalProperties": false
    })
}

fn replace_filter_schema(parameters: &mut Value) {
    if parameters["properties"].get("filter").is_some() {
        parameters["properties"]["filter"] = json!({
            "type": "object",
            "description": "Metadata filters supported by the TinyMemory API."
        });
    }
}

fn branch(action: &str, parameters: Value) -> Value {
    let mut properties = parameters["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    properties.insert("action".into(), json!({"const": action}));
    let mut required = parameters["required"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    required.push(json!("action"));
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn learn_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "text": {
                "type": "string",
                "description": "The self-contained statement worth remembering."
            },
            "kind": {
                "type": "string",
                "enum": LEARNING_KINDS,
                "default": DEFAULT_LEARNING_KIND,
                "description": "What kind of statement it is."
            },
            "confidence": {
                "type": "number",
                "minimum": 0.0,
                "maximum": 1.0,
                "default": DEFAULT_CONFIDENCE
            },
            "evidence": {
                "type": "string",
                "description": "What supports the statement."
            }
        },
        "required": ["text"]
    })
}

fn forget_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "ids": {
                "type": "array",
                "items": {"type": "string"},
                "minItems": 1,
                "description": "The memory ids to remove."
            }
        },
        "required": ["ids"]
    })
}
