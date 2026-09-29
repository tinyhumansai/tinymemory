//! Workspace-backed conversation thread/message storage: JSONL threads and
//! messages under `<workspace>/memory/conversations/`, a local trigram /
//! CJK-bigram inverted index for cross-thread substring search, and the
//! `ConversationEventBus` persistence-subscriber seam.
//!
//! Pure library: `serde`, `parking_lot`, `uuid`, `chrono` and `async-trait`
//! only. No SQLite, HTTP or engine dependency, so a host can store
//! transcripts without linking a memory engine.
//!
//! This crate is the canonical copy of the store. It carries the per-root
//! lifecycle / metadata / per-thread locking, the race-free cold index build,
//! idempotent deterministic message ids and full-width ASCII folding that
//! OpenHuman had developed beyond the older `tinycortex` port. The on-disk
//! format is unchanged.
//!
//! ## Layout
//!
//! - `types` - the on-disk wire types (threads, messages, patches, hits).
//! - `tokenize` - multilingual normalization + character n-gram tokenizer.
//! - `inverted_index` - in-memory index over message content.
//! - `store` - the JSONL [`ConversationStore`] and its free-function API.
//! - [`bus`] - channel-event persistence subscriber abstracted behind
//!   [`bus::ConversationEventBus`].

pub mod bus;
mod inverted_index;
#[allow(clippy::module_inception)]
mod store;
mod tokenize;
mod types;

pub use store::{
    append_message, delete_messages_from, delete_thread, ensure_thread, get_messages, list_threads,
    purge_threads, update_message, update_thread_labels, update_thread_title,
    ConversationPurgeStats, ConversationStore,
};
pub use types::{
    is_deterministic_message_id, reply_run_id, run_reply_message_id, ConversationMessage,
    ConversationMessagePatch, ConversationThread, CreateConversationThread, CrossThreadHit,
};
