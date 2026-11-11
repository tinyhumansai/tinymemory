//! How a TinyMemory item is laid out as CortexDB events.
//!
//! # Scopes
//!
//! Every item lives in one scope per kind under its namespace node, below the
//! TinyMemory root [`ROOT_SCOPE`] ([`scope_path`]):
//!
//! ```text
//! app:tinymemory/app:{documents,conversations,learnings}                 the root node
//! app:tinymemory/agent:researcher/app:{documents,conversations,learnings} an agent
//! app:tinymemory/team:acme/agent:writer/app:learnings                    a team member
//! ```
//!
//! So within every node, learnings, documents and conversations are separate
//! scopes, and CortexDB can recall, retain and erase each on its own. The
//! namespace segments map onto CortexDB's built-in scope types (`agent`,
//! `team`, `user`, `ws`, `project`, `source`), which every shipped
//! deployment preset allows. A brain's per-source documents therefore live in
//! `app:tinymemory/source:pdf/app:documents`. The hosted backend additionally re-roots every scope under the
//! caller's tenant, which is invisible here. A
//! [`tinymemory_api::MetaFilter`]'s `kinds` and `reach` pick which scopes are
//! read (see `engine::scopes`).
//!
//! # Events
//!
//! A learning is one event. A conversation is one event per turn, appended
//! in order. A document is one event, or, when its envelope would be too big
//! for one, one event per piece of its body ([`chunks`]), appended in order;
//! each piece carries its index and, when available, the page range and
//! section it covers, and the pieces concatenate back to the body.
//!
//! No event is sent whose encoded envelope is over
//! [`chunks::MAX_EVENT_TEXT_BYTES`] ([`Envelope::encode_checked`]): CortexDB
//! refuses an experience over 1 MiB of text, and a conversation turn or a
//! learning cannot be split. An [`Envelope`] carries the item id, kind, the
//! event's own text (the body, the turn's text, or the learning's
//! statement), the item's full [`MemoryMeta`], and the kind's extra fields.
//! CortexDB's experience schema is closed (an unknown field is a 422), and
//! `context.labels` is its app-metadata extension point. So an event is
//! written as v3: `content.text` is the event's own text, which CortexDB
//! extracts from and splits for search at sentence boundaries, and the rest
//! of the envelope rides in `tm:e:<NN>:` labels ([`Envelope::encode_checked`]).
//! An event with empty text, or too much envelope for its labels, is written
//! as v2: the whole envelope as JSON text, as every event was before v3.
//! Both read back ([`decode_event`]); anything else is somebody else's event
//! and is ignored.
//!
//! Each event also carries lookup labels (see [`labels`]) and, when the item
//! has one, `context.observed_at`.
//!
//! # Local paths
//!
//! No local path leaves the machine ([`wire_meta`]): an envelope keeps a
//! file's name but not its folders, and no `folder`. A `workspace` that is
//! an absolute path (an agent's working folder) is left out too; a logical
//! workspace id is kept. In their place the envelope carries digests
//! ([`PathDigests`]) that a `file_path`, `folder` or `workspace` filter is
//! matched against, exactly as the full value would be. Reads therefore give
//! back a file's name as its `file_path`, and no folder or absolute
//! workspace.
//!
//! # Identity
//!
//! The item id is [`StoreItem::fingerprint`]: a content digest, so storing
//! an identical item again resolves to the same id and is detected as a
//! replay by looking that id's label up.

pub(crate) mod chunks;
pub(crate) mod labels;
mod layout;
mod rebuild;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tinymemory_api::chrono::{DateTime, Utc};
use tinymemory_api::{
    DocumentBody, ItemKind, LearningKind, MemoryMeta, Namespace, Role, StoreItem, ToolCallRef,
};

use crate::cortex::error::{Error, Result};

pub(crate) use layout::ScopeLayout;
pub(crate) use rebuild::{Decoded, decode_event, rebuild, rebuild_whole};

/// The TinyMemory root every kind scope sits under.
pub(crate) const ROOT_SCOPE: &str = "app:tinymemory";

/// The envelope version of an event whose text is the whole JSON envelope:
/// every event before v3, and a v3-era event whose envelope does not fit in
/// its labels.
const V2: u8 = 2;

/// The envelope version of an event whose text is the item's own text and
/// whose envelope (all but the text) rides in its labels.
const V3: u8 = 3;

/// The label prefix of a v3 envelope part: `tm:e:<NN>:<JSON slice>`.
const PART_PREFIX: &str = "tm:e:";

/// The most bytes of envelope JSON one part label carries, so a label stays
/// within the 256 bytes CortexDB asks labels to keep to.
const PART_BYTES: usize = 240;

/// The most labels one event carries, as CortexDB asks.
const MAX_LABELS: usize = 64;

/// The longest readable label written; a longer one is left out.
const MAX_LABEL_BYTES: usize = 256;

/// One event as it is sent: its `content.text` and `context.labels`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Encoded {
    /// The item's own text (v3), or the whole JSON envelope (v2).
    pub(crate) text: String,
    /// The lookup labels, then (v3) the readable labels and the envelope
    /// parts.
    pub(crate) labels: Vec<String>,
}

/// The leaf segment of `kind`'s scope inside a namespace node.
pub(crate) fn kind_leaf(kind: ItemKind) -> &'static str {
    match kind {
        ItemKind::Document => "app:documents",
        ItemKind::Conversation => "app:conversations",
        ItemKind::Learning => "app:learnings",
    }
}

/// The scope items of `kind` at `namespace` live in.
pub(crate) fn scope_path(namespace: &Namespace, kind: ItemKind) -> String {
    let mut path = String::from(ROOT_SCOPE);
    for segment in namespace.segments() {
        path.push('/');
        path.push_str(&segment.to_string());
    }
    path.push('/');
    path.push_str(kind_leaf(kind));
    path
}

/// The namespace and kind of a TinyMemory scope path, wherever it is rooted
/// (the hosted backend prefixes the caller's tenant); `None` for any other
/// scope.
pub(crate) fn parse_scope(path: &str) -> Option<(Namespace, ItemKind)> {
    let mut parts = path.split('/');
    parts.by_ref().find(|part| *part == ROOT_SCOPE)?;
    let rest: Vec<&str> = parts.collect();
    let (leaf, nodes) = rest.split_last()?;
    let kind = ItemKind::ALL
        .into_iter()
        .find(|kind| kind_leaf(*kind) == *leaf)?;
    let namespace = nodes.join("/").parse().ok()?;
    Some((namespace, kind))
}

/// One event's payload: the item it belongs to and the event's share of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Envelope {
    /// [`V2`] or [`V3`] as stored; [`V3`] for an envelope laid out here.
    pub(crate) v: u8,
    /// The item id ([`StoreItem::fingerprint`]).
    pub(crate) id: String,
    /// The item's kind.
    pub(crate) kind: ItemKind,
    /// The document body, the turn's text, or the learning's statement.
    pub(crate) text: String,
    /// The item's metadata on every event, without local paths
    /// ([`wire_meta`]).
    pub(crate) meta: MemoryMeta,
    /// What filters match the paths `meta` leaves out against.
    #[serde(flatten)]
    pub(crate) paths: PathDigests,
    /// Document title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    /// Document MIME type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) mime: Option<String>,
    /// Learning kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) learning_kind: Option<LearningKind>,
    /// Learning confidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) confidence: Option<f32>,
    /// Learning evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) evidence: Option<String>,
    /// Which turn of a conversation this event is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn: Option<TurnInfo>,
    /// Which piece of a chunked document this event is; absent for a
    /// document written as one event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chunk: Option<ChunkInfo>,
}

/// A piece of a chunked document: its place, and what it covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChunkInfo {
    /// Zero-based position.
    pub(crate) index: u32,
    /// How many pieces the document has.
    pub(crate) count: u32,
    /// The first and last page the piece covers, counted from 1, when the
    /// document marks its pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) pages: Option<[u32; 2]>,
    /// The title of the section the piece starts in, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) section: Option<String>,
}

/// Room left in a piece's envelope for its section title: the longest title
/// at the worst JSON escaping, plus its key.
const SECTION_RESERVE: usize = chunks::MAX_SECTION_CHARS * 6 + 32;

/// Digests of the local paths an envelope leaves out ([`wire_meta`]), each
/// a [`labels::path_digest`], so a filter on the full value still matches.
/// A path digest is as strong as an item id, so a match is the match.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PathDigests {
    /// The absolute `workspace`.
    #[serde(default, rename = "ws", skip_serializing_if = "Option::is_none")]
    pub(crate) workspace: Option<String>,
    /// The `file_path` and each folder above it ([`prefixes`]).
    #[serde(default, rename = "fp", skip_serializing_if = "Vec::is_empty")]
    pub(crate) file_path: Vec<String>,
    /// The `folder` and each folder above it ([`prefixes`]).
    #[serde(default, rename = "fd", skip_serializing_if = "Vec::is_empty")]
    pub(crate) folder: Vec<String>,
}

impl PathDigests {
    /// The digests of `meta`'s local paths.
    fn of(meta: &MemoryMeta) -> Self {
        Self {
            workspace: meta
                .workspace
                .as_deref()
                .filter(|workspace| is_absolute(workspace))
                .map(labels::path_digest),
            file_path: prefixes(meta.file_path.as_deref()),
            folder: prefixes(meta.folder.as_deref()),
        }
    }
}

/// The digest of `path` and of each prefix of it that ends before a `/`:
/// every value a `MetaFilter` path filter can name and still match `path`
/// (an exact path, or a folder above it at a `/`). Only `/` ends a folder,
/// as in `MetaFilter`'s own path matching, so a `\` never does.
fn prefixes(path: Option<&str>) -> Vec<String> {
    let Some(path) = path else {
        return Vec::new();
    };
    let mut out: Vec<String> = path
        .match_indices('/')
        .map(|(at, _)| labels::path_digest(&path[..at]))
        .collect();
    out.push(labels::path_digest(path));
    out.dedup();
    out
}

/// A conversation turn's place and attributes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct TurnInfo {
    /// Zero-based position.
    pub(crate) index: u32,
    /// How many turns the conversation has.
    pub(crate) count: u32,
    /// Who spoke.
    pub(crate) role: Role,
    /// When, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) at: Option<DateTime<Utc>>,
    /// Tool calls the turn made.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) tool_calls: Vec<ToolCallRef>,
}

impl Envelope {
    /// A bare envelope for item `id` of `kind`.
    fn new(id: &str, kind: ItemKind, text: String, meta: &MemoryMeta) -> Self {
        Self {
            v: V3,
            id: id.to_string(),
            kind,
            text,
            meta: wire_meta(meta),
            paths: PathDigests::of(meta),
            title: None,
            mime: None,
            learning_kind: None,
            confidence: None,
            evidence: None,
            turn: None,
            chunk: None,
        }
    }

    /// The position of this event within its item: a conversation turn's
    /// index, a document piece's index, or `None` for an item written as
    /// one event.
    pub(crate) fn part(&self) -> Option<u32> {
        self.turn
            .as_ref()
            .map(|turn| turn.index)
            .or_else(|| self.chunk.as_ref().map(|chunk| chunk.index))
    }

    /// The envelopes `item` is written as, one per event, in write order.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for a document whose body is still a URI,
    /// or a conversation with more turns than a `u32` counts.
    pub(crate) fn for_item(item: &StoreItem, id: &str) -> Result<Vec<Self>> {
        match item {
            StoreItem::Document {
                title,
                body,
                mime,
                meta,
            } => {
                let DocumentBody::Text(text) = body else {
                    return Err(Error::InvalidRequest(
                        "document body is an unresolved uri".to_string(),
                    ));
                };
                let mut whole = Self::new(id, ItemKind::Document, String::new(), meta);
                whole.title.clone_from(title);
                whole.mime.clone_from(mime);
                let paged = text.contains(chunks::PAGE_BREAK);
                let Some(pieces) = chunks::split(
                    text,
                    whole.piece_overhead(paged)?,
                    chunks::DOCUMENT_CHUNK_TARGET_BYTES,
                    chunks::MAX_EVENT_TEXT_BYTES,
                ) else {
                    // The metadata leaves no room for a piece: the document
                    // is written whole if it fits, and refused here if not,
                    // never cut into pieces that would each be over the limit.
                    whole.text.clone_from(text);
                    whole.encode_checked()?;
                    return Ok(vec![whole]);
                };
                if pieces.len() <= 1 {
                    // One piece fits under the limit with a chunk field, so
                    // the whole envelope (which has none) does too; checked
                    // all the same, as every envelope this returns is.
                    whole.text.clone_from(text);
                    whole.encode_checked()?;
                    return Ok(vec![whole]);
                }
                let count = u32::try_from(pieces.len()).map_err(|_| {
                    Error::InvalidRequest("document has too many pieces".to_string())
                })?;
                Ok((0..count)
                    .zip(pieces)
                    .map(|(index, piece)| {
                        let mut envelope = whole.clone();
                        envelope.text = piece.text.to_string();
                        envelope.chunk = Some(ChunkInfo {
                            index,
                            count,
                            pages: piece.pages.map(|(first, last)| [first, last]),
                            section: piece.section,
                        });
                        envelope
                    })
                    .collect())
            }
            StoreItem::Learning {
                text,
                kind,
                confidence,
                evidence,
                meta,
            } => {
                let mut envelope = Self::new(id, ItemKind::Learning, text.clone(), meta);
                envelope.learning_kind = Some(*kind);
                envelope.confidence = Some(*confidence);
                envelope.evidence.clone_from(evidence);
                Ok(vec![envelope])
            }
            StoreItem::Conversation { turns, meta } => {
                let count = u32::try_from(turns.len()).map_err(|_| {
                    Error::InvalidRequest("conversation has too many turns".to_string())
                })?;
                let mut out = Vec::with_capacity(turns.len());
                for (index, turn) in (0..count).zip(turns) {
                    let mut envelope =
                        Self::new(id, ItemKind::Conversation, turn.text.clone(), meta);
                    envelope.turn = Some(TurnInfo {
                        index,
                        count,
                        role: turn.role,
                        at: turn.at,
                        tool_calls: turn.tool_calls.clone(),
                    });
                    out.push(envelope);
                }
                Ok(out)
            }
        }
    }

    /// Reads a v2 envelope from an event's text, whichever read path it
    /// came from.
    ///
    /// The two read paths may disagree on the bytes: `/v1/events` returns
    /// the text as stored, while an older `/v1/recall` rendered it for a
    /// reader and prefixed the speaker (`[user] {...}`). The prefix is
    /// stripped only when the text does not parse without it. Anything that
    /// is not a v2 envelope is `None`.
    pub(crate) fn decode(text: &str) -> Option<Self> {
        let parsed = serde_json::from_str::<Self>(text).ok().or_else(|| {
            let rendered = text.strip_prefix('[')?;
            let (_role, rest) = rendered.split_once("] ")?;
            serde_json::from_str::<Self>(rest).ok()
        })?;
        (parsed.v == V2).then_some(parsed)
    }

    /// Reads a v3 envelope from an event's text and labels: the envelope
    /// parts (`tm:e:00:`, `tm:e:01:`, …) joined in order, with `text` as its
    /// text. `None` when the parts are absent, not numbered `0..n`, or not a
    /// v3 envelope.
    pub(crate) fn from_labels<'a>(
        text: &str,
        labels: impl IntoIterator<Item = &'a str>,
    ) -> Option<Self> {
        let mut parts: Vec<(usize, &str)> = Vec::new();
        for label in labels {
            if let Some(rest) = label.strip_prefix(PART_PREFIX) {
                let (index, json) = rest.split_once(':')?;
                parts.push((index.parse().ok()?, json));
            }
        }
        parts.sort_by_key(|(index, _)| *index);
        if parts.is_empty()
            || parts
                .iter()
                .enumerate()
                .any(|(at, (index, _))| at != *index)
        {
            return None;
        }
        let json: String = parts.into_iter().map(|(_, json)| json).collect();
        let mut envelope = serde_json::from_str::<Self>(&json).ok()?;
        if envelope.v != V3 {
            return None;
        }
        envelope.text = text.to_string();
        Some(envelope)
    }

    /// The whole envelope as v2 JSON: what a v2 event's text holds, and the
    /// size every limit here is checked against.
    ///
    /// # Errors
    ///
    /// [`Error::Engine`] if serialisation fails, which plain data cannot.
    pub(crate) fn encode(&self) -> Result<String> {
        let mut v2 = self.clone();
        v2.v = V2;
        json_of(&v2)
    }

    /// The event as sent, refused when its v2 JSON is over
    /// [`chunks::MAX_EVENT_TEXT_BYTES`]: CortexDB would refuse such an event
    /// as v2, so nothing of the item is sent. (Checking the v2 size keeps one
    /// limit for both layouts, and a v3 text is always shorter.)
    ///
    /// The event is v3 (the item's own text, the envelope in labels) unless
    /// its text is empty or its labels would be more than [`MAX_LABELS`];
    /// then it is v2, as every event was before.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidRequest`] for an envelope over the limit (a
    /// conversation turn or a learning that long, or metadata too large to
    /// leave room for a document piece); as [`Envelope::encode`] otherwise.
    pub(crate) fn encode_checked(&self) -> Result<Encoded> {
        let v2 = self.encode()?;
        if v2.len() > chunks::MAX_EVENT_TEXT_BYTES {
            return Err(Error::InvalidRequest(format!(
                "a {:?} event would be {} bytes; CortexDB refuses an event over 1 MiB, so at \
                 most {} are sent",
                self.kind,
                v2.len(),
                chunks::MAX_EVENT_TEXT_BYTES
            )));
        }
        let mut lookup = labels::for_item(&self.id, &self.meta, self.paths.workspace.as_deref());
        lookup.extend(self.paths.workspace.as_deref().map(labels::workspace));
        let mut head = self.clone();
        head.v = V3;
        head.text = String::new();
        let parts = part_labels(&json_of(&head)?);
        let readable = self.readable_labels();
        if self.text.is_empty() || lookup.len() + readable.len() + parts.len() > MAX_LABELS {
            return Ok(Encoded {
                text: v2,
                labels: lookup,
            });
        }
        let mut labels = lookup;
        labels.extend(readable);
        labels.extend(parts);
        Ok(Encoded {
            text: self.text.clone(),
            labels,
        })
    }

    /// Whether the persisted metadata describes the same fingerprint identity
    /// as `meta`, ignoring the conversation channel and volatile provenance.
    /// Local paths are compared through the digests the envelope stores.
    pub(crate) fn matches_meta_identity(&self, meta: &MemoryMeta) -> bool {
        let mut expected = wire_meta(meta);
        let mut held = self.meta.clone();
        for value in [&mut expected, &mut held] {
            value.channel = None;
            value.observed_at = None;
            value.observed_actor = None;
            if let Some(call) = value.tool_call.as_mut() {
                call.id = None;
            }
        }
        held == expected && self.paths == PathDigests::of(meta)
    }

    /// Labels a person reading the events can make sense of: the item kind,
    /// the agent (`agent:<id>`), an app thread (`thread:thread-<uuid>`, never
    /// a channel's thread), and for a document its file, and a piece's pages
    /// and section. Never filtered on (the lookup labels are), and left out
    /// when longer than [`MAX_LABEL_BYTES`].
    fn readable_labels(&self) -> Vec<String> {
        let mut out = vec![format!("kind:{}", self.kind.as_str())];
        if let Some(agent) = self
            .meta
            .agent_id
            .as_deref()
            .filter(|a| !a.trim().is_empty())
        {
            out.push(format!("agent:{agent}"));
        }
        if let Some(thread) = self.meta.thread_id.as_deref().filter(|t| app_thread(t)) {
            out.push(format!("thread:{thread}"));
        }
        if let Some(path) = &self.meta.file_path {
            out.push(format!("file:{path}"));
        }
        if let Some(chunk) = &self.chunk {
            match chunk.pages {
                Some([first, last]) if first == last => out.push(format!("page:{first}")),
                Some([first, last]) => out.push(format!("page:{first}-{last}")),
                None => {}
            }
            if let Some(section) = &chunk.section {
                out.push(format!("section:{section}"));
            }
        }
        out.retain(|label| label.len() <= MAX_LABEL_BYTES);
        out
    }

    /// The encoded size of this envelope as a document piece with an empty
    /// text: the room every piece's own text is added to. A page range is
    /// reserved only for a document that marks pages (`paged`), since only
    /// its pieces carry one.
    fn piece_overhead(&self, paged: bool) -> Result<usize> {
        let mut probe = self.clone();
        probe.chunk = Some(ChunkInfo {
            index: u32::MAX,
            count: u32::MAX,
            pages: paged.then_some([u32::MAX, u32::MAX]),
            section: None,
        });
        Ok(probe.encode()?.len() + SECTION_RESERVE)
    }

    /// The experience request appending this envelope as `encoded`, keyed by
    /// its own body (`transport::body_idempotency_key`), so an identical
    /// retry is a replay.
    pub(crate) fn request(&self, encoded: &Encoded, layout: &ScopeLayout) -> Value {
        let (modality, role) = match (&self.kind, &self.turn) {
            (ItemKind::Conversation, Some(turn)) => ("conversation", role_of(turn.role)),
            (ItemKind::Document, _) => ("document", "user"),
            _ => ("observation", "user"),
        };
        let observed_at = self
            .turn
            .as_ref()
            .and_then(|turn| turn.at)
            .or(self.meta.observed_at);
        let mut context = serde_json::Map::new();
        context.insert("labels".to_string(), json!(encoded.labels));
        if let Some(at) = observed_at {
            context.insert("observed_at".to_string(), json!(at.to_rfc3339()));
        }
        let mut request = json!({
            "scope": layout.path(&self.meta.namespace, self.kind),
            "modality": modality,
            "content": { "kind": "message", "role": role, "text": encoded.text },
            "context": Value::Object(context),
        });
        // Index and embed, derive nothing (no facts, beliefs or concepts)
        // for an item that opts out, and for a tool's output: raw tool
        // results are searchable but are not memory about the person.
        let tool_turn = self
            .turn
            .as_ref()
            .is_some_and(|turn| turn.role == Role::Tool);
        if self.meta.derive == Some(false) || tool_turn {
            request["directives"] = json!({ "extract": [] });
        }
        request["idempotency_key"] =
            json!(crate::cortex::transport::body_idempotency_key(&request));
        request
    }
}

/// `meta` as it may leave the machine: a file's name without its folders,
/// no `folder`, and no absolute `workspace` (local paths, which name a
/// person's home folder).
fn wire_meta(meta: &MemoryMeta) -> MemoryMeta {
    MemoryMeta {
        file_path: meta.file_path.as_deref().and_then(file_name),
        folder: None,
        workspace: meta
            .workspace
            .clone()
            .filter(|workspace| !is_absolute(workspace)),
        ..meta.clone()
    }
}

/// Whether `thread_id` is an app thread's opaque id, `thread-<uuid>`, the
/// only thread id written as a readable label. Any other (a channel's can
/// name the person at the other end) is labelled by its digest alone.
fn app_thread(thread_id: &str) -> bool {
    let Some(uuid) = thread_id.strip_prefix("thread-") else {
        return false;
    };
    let groups: Vec<&str> = uuid.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(group, len)| {
            group.len() == len
                && group
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

/// Whether `path` is absolute on any system: `/…`, `~…`, `\\…` or `C:…`.
pub(crate) fn is_absolute(path: &str) -> bool {
    let drive = matches!(path.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic());
    path.starts_with(['/', '~', '\\']) || drive
}

/// The last component of `path`, split at `/` or `\`, whichever system
/// wrote it; `None` when it is empty.
fn file_name(path: &str) -> Option<String> {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// `value` as compact JSON.
fn json_of(value: &Envelope) -> Result<String> {
    serde_json::to_string(value)
        .map_err(|_| Error::Engine("an item envelope could not be serialised".to_string()))
}

/// `json` cut into numbered part labels of at most [`PART_BYTES`] bytes
/// each, at char boundaries.
fn part_labels(json: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = json;
    while !rest.is_empty() {
        let mut end = rest.len().min(PART_BYTES);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        out.push(format!("{PART_PREFIX}{:02}:{}", out.len(), &rest[..end]));
        rest = &rest[end..];
    }
    out
}

/// CortexDB's four-value message role for a turn's speaker.
fn role_of(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::System => "system",
        Role::Tool => "tool",
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
