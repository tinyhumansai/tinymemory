//! The JSON Schema for each tool's arguments, and the limits and defaults the
//! schemas advertise and the argument parser enforces.
//!
//! Every object sets `additionalProperties: false`, and no schema has a
//! `namespace` or `reach` property: those are fixed by the host.

pub(crate) use crate::ops::DEFAULT_LEARNING_CONFIDENCE as DEFAULT_CONFIDENCE;
pub(crate) use crate::ops::{DEFAULT_BUCKETS, DEFAULT_LIMIT, MAX_BUCKETS, MAX_LIMIT};
use serde_json::{Map, Value, json};
use tinymemory_api::explore::MAX_GET_IDS;
use tinymemory_api::{FetchMode, ItemKind, SourceKind};

/// The most ids one `memory_get` or `memory_forget` call names.
pub(crate) const MAX_IDS: usize = MAX_GET_IDS;

/// A learning's kind when the model gives none.
pub(crate) const DEFAULT_LEARNING_KIND: &str = "fact";

/// The facets `memory_explore` groups by. The namespace facet is left out on
/// purpose: the namespace is the host's, not the model's.
pub(crate) const EXPLORE_FACETS: [&str; 13] = [
    "kind",
    "source",
    "source_id",
    "workspace",
    "folder",
    "file_path",
    "language",
    "repo",
    "url",
    "thread",
    "agent",
    "tool_call",
    "tag",
];

/// The learning kinds a model may name.
pub(crate) const LEARNING_KINDS: [&str; 5] =
    ["preference", "fact", "procedure", "correction", "other"];

/// The conversation roles a model may name.
pub(crate) const ROLES: [&str; 4] = ["user", "assistant", "system", "tool"];

/// The fields of the model-facing filter, a subset of
/// [`tinymemory_api::MetaFilter`].
pub(crate) const FILTER_FIELDS: [&str; 12] = [
    "kinds",
    "sources",
    "tags_any",
    "workspace",
    "folder",
    "file_path",
    "repo",
    "url",
    "thread_id",
    "agent_id",
    "observed_after",
    "observed_before",
];

/// The fetch mode used when the model names none: hybrid when the engine
/// serves it, otherwise the first mode it lists.
pub(crate) fn default_mode(modes: &[FetchMode]) -> Option<FetchMode> {
    if modes.contains(&FetchMode::Hybrid) {
        Some(FetchMode::Hybrid)
    } else {
        modes.first().copied()
    }
}

/// `memory_recall`'s arguments.
pub(crate) fn recall() -> Value {
    object(
        [
            (
                "question",
                text("The question to answer, in natural language."),
            ),
            ("filter", filter()),
            (
                "limit",
                limit("Most memories the answer may cite.", DEFAULT_LIMIT),
            ),
            (
                "instructions",
                text("Optional extra instructions for how to answer (length, format, focus)."),
            ),
            ("refers_to", refers_to()),
        ],
        &["question"],
    )
}

/// `memory_fetch`'s arguments; `mode` lists exactly `modes`.
pub(crate) fn fetch(modes: &[FetchMode]) -> Value {
    let names: Vec<&str> = modes.iter().map(|mode| mode.as_str()).collect();
    let mut mode = json!({
        "type": "string",
        "enum": names,
        "description": "How to rank: `keyword` (lexical match), `vector` (meaning) or \
                        `hybrid` (both), as this memory offers them.",
    });
    if let (Some(default), Some(schema)) = (default_mode(modes), mode.as_object_mut()) {
        schema.insert("default".to_string(), json!(default.as_str()));
    }
    object(
        [
            ("query", text("What to search for.")),
            ("mode", mode),
            ("filter", filter()),
            ("limit", limit("Most memories to return.", DEFAULT_LIMIT)),
            ("cursor", cursor()),
            ("refers_to", refers_to()),
        ],
        &["query"],
    )
}

/// `memory_list`'s arguments.
pub(crate) fn list() -> Value {
    object(
        [
            ("filter", filter()),
            ("limit", limit("Most memories to return.", DEFAULT_LIMIT)),
            ("cursor", cursor()),
        ],
        &[],
    )
}

/// `memory_get`'s arguments.
pub(crate) fn get() -> Value {
    object([("ids", ids("The ids of the memories to read."))], &["ids"])
}

/// `memory_explore`'s arguments.
pub(crate) fn explore() -> Value {
    object(
        [
            (
                "facet",
                json!({
                    "type": "string",
                    "enum": EXPLORE_FACETS,
                    "description": "The dimension to group memories by.",
                }),
            ),
            ("filter", filter()),
            (
                "limit",
                limit_between("Most values to return.", DEFAULT_BUCKETS, MAX_BUCKETS),
            ),
        ],
        &["facet"],
    )
}

/// `memory_store`'s arguments.
pub(crate) fn store() -> Value {
    let learning = object(
        [
            ("text", text("The statement, self-contained and specific.")),
            (
                "learning_kind",
                json!({
                    "type": "string",
                    "enum": LEARNING_KINDS,
                    "default": DEFAULT_LEARNING_KIND,
                    "description": "What kind of statement it is.",
                }),
            ),
            (
                "confidence",
                json!({
                    "type": "number",
                    "minimum": 0.0,
                    "maximum": 1.0,
                    "default": DEFAULT_CONFIDENCE,
                    "description": "How sure you are, from 0 to 1.",
                }),
            ),
            ("evidence", text("What supports the statement.")),
        ],
        &["text"],
    );
    let document = object(
        [
            ("title", text("The document's title.")),
            ("text", text("The document's body, normally markdown.")),
        ],
        &["text"],
    );
    let turn = object(
        [
            (
                "role",
                json!({ "type": "string", "enum": ROLES, "description": "Who spoke." }),
            ),
            ("text", text("What was said.")),
        ],
        &["role", "text"],
    );
    let conversation = object(
        [(
            "turns",
            json!({
                "type": "array",
                "items": turn,
                "minItems": 1,
                "description": "The turns, in order.",
            }),
        )],
        &["turns"],
    );
    object(
        [
            (
                "learning",
                described(learning, "A distilled statement worth remembering."),
            ),
            ("document", described(document, "A text to remember whole.")),
            (
                "conversation",
                described(conversation, "An exchange to remember."),
            ),
            ("tags", strings("Free-form tags to file the memory under.")),
        ],
        &[],
    )
}

/// `memory_forget`'s arguments.
pub(crate) fn forget() -> Value {
    object(
        [
            ("ids", ids("The ids of the memories to remove.")),
            (
                "filter",
                described(
                    filter(),
                    "Remove every memory matching this filter; it must set at least one field.",
                ),
            ),
        ],
        &[],
    )
}

/// The model-facing filter: [`FILTER_FIELDS`], never a namespace or reach.
fn filter() -> Value {
    let kinds: Vec<&str> = ItemKind::ALL.iter().map(|kind| kind.as_str()).collect();
    let sources: Vec<&str> = SourceKind::ALL.iter().map(|kind| kind.as_str()).collect();
    let mut schema = object(
        [
            (
                "kinds",
                enum_list(&kinds, "Only these kinds of memory; empty means all."),
            ),
            (
                "sources",
                enum_list(
                    &sources,
                    "Only memories from these kinds of source; empty means all.",
                ),
            ),
            (
                "tags_any",
                strings("Only memories carrying at least one of these tags."),
            ),
            ("workspace", text("Exact workspace.")),
            ("folder", text("Folder, exact or as a path prefix.")),
            ("file_path", text("File path, exact or as a path prefix.")),
            ("repo", text("Exact repository, as `owner/name` or a URL.")),
            ("url", text("Exact URL the memory was read from.")),
            ("thread_id", text("Exact conversation thread id.")),
            ("agent_id", text("Exact id of the agent that produced it.")),
            (
                "observed_after",
                timestamp("Only memories observed at or after this RFC 3339 time."),
            ),
            (
                "observed_before",
                timestamp("Only memories observed before this RFC 3339 time."),
            ),
        ],
        &[],
    );
    if let Some(map) = schema.as_object_mut() {
        map.insert(
            "description".to_string(),
            json!("Narrows which memories are considered; every field set must match."),
        );
    }
    schema
}

/// A closed object with these properties, `required` listed only when
/// non-empty.
fn object<const N: usize>(properties: [(&str, Value); N], required: &[&str]) -> Value {
    let properties: Map<String, Value> = properties
        .into_iter()
        .map(|(name, schema)| (name.to_string(), schema))
        .collect();
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    if let (false, Some(map)) = (required.is_empty(), schema.as_object_mut()) {
        map.insert("required".to_string(), json!(required));
    }
    schema
}

fn described(mut schema: Value, description: &str) -> Value {
    if let Some(map) = schema.as_object_mut() {
        map.insert("description".to_string(), json!(description));
    }
    schema
}

/// The days a question is about. The wording is measured: with it, the
/// managed chat model set `refers_to` on 287 of 289 time questions in the
/// app's 16 languages and Hinglish (97% right); without the property it set
/// no date at all.
fn refers_to() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "description": "The local calendar dates the question is about, resolved against the \
                        Current Date & Time line. Set it whenever the question mentions or \
                        implies a time, in any language: \"yesterday\", \"last Saturday\", \
                        \"in March\", \"kal\", \"pichle hafte\", \"el viernes\". A single \
                        day has from == to. If a word can mean the past or the future (Hindi \
                        \"kal\"), cover both days. Prefer this over filter.observed_after/\
                        observed_before: those drop every memory recorded on another day, this \
                        only ranks the matching days first.",
        "properties": {
            "from": {
                "type": "string",
                "format": "date",
                "description": "First local date, YYYY-MM-DD.",
            },
            "to": {
                "type": "string",
                "format": "date",
                "description": "Last local date, YYYY-MM-DD, inclusive.",
            },
        },
        "required": ["from", "to"],
    })
}

fn text(description: &str) -> Value {
    json!({ "type": "string", "description": description })
}

fn timestamp(description: &str) -> Value {
    json!({ "type": "string", "format": "date-time", "description": description })
}

fn strings(description: &str) -> Value {
    json!({ "type": "array", "items": { "type": "string" }, "description": description })
}

fn enum_list(values: &[&str], description: &str) -> Value {
    json!({
        "type": "array",
        "items": { "type": "string", "enum": values },
        "description": description,
    })
}

fn ids(description: &str) -> Value {
    json!({
        "type": "array",
        "items": { "type": "string" },
        "minItems": 1,
        "maxItems": MAX_IDS,
        "description": description,
    })
}

fn limit(description: &str, default: usize) -> Value {
    limit_between(description, default, MAX_LIMIT)
}

fn limit_between(description: &str, default: usize, maximum: usize) -> Value {
    json!({
        "type": "integer",
        "minimum": 1,
        "maximum": maximum,
        "default": default,
        "description": description,
    })
}

fn cursor() -> Value {
    text("The `next_cursor` of a previous result, to continue from it.")
}
