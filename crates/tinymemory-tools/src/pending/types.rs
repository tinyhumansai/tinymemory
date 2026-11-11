//! The queue's persisted shapes and the gate that fences it.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tinymemory_api::{Namespace, Reach, StoreItem};

/// Bounds on one queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most entries held.
    pub max_entries: usize,
    /// Most serialized bytes held.
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 256,
            max_bytes: 2 * 1024 * 1024,
        }
    }
}

/// Why a write was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PendingError {
    /// The queue is at its entry bound; nothing was queued.
    #[error("memory write queue full; nothing was queued")]
    Full,
    /// The store could not be read or written, or the bytes bound was hit;
    /// durable queueing was not confirmed.
    #[error("memory write queue unavailable; durable queueing was not confirmed")]
    Unavailable,
}

/// One write an agent tool asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // one short-lived value per queued write; boxing would only add noise
pub enum WriteOperation {
    /// Store a learning.
    Learn {
        /// The scrubbed item.
        item: StoreItem,
    },
    /// Forget items by id.
    Forget {
        /// Sorted, de-duplicated ids.
        ids: Vec<String>,
        /// The trusted reach they are forgotten under.
        reach: Reach,
    },
}

impl WriteOperation {
    /// Whether `other` is the same write, so an adjacent retry merges.
    #[must_use]
    pub fn coalesces(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Learn { item: a }, Self::Learn { item: b }) => {
                a.fingerprint() == b.fingerprint()
            }
            (Self::Forget { ids: a, reach: ar }, Self::Forget { ids: b, reach: br }) => {
                a == b && ar == br
            }
            _ => false,
        }
    }
}

/// What a drain checks the active owner against: an opaque, host-defined
/// description of the owner and layout a write was admitted under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding(pub String);

/// The erasure generation a write was admitted under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Epoch {
    /// The process that admitted it.
    pub process: String,
    /// Its [`WriteGate`] generation then.
    pub generation: u64,
}

/// One queued write.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// Unique id of this entry.
    pub ticket: String,
    /// The owner and layout it was admitted under.
    pub binding: Binding,
    /// The trusted scope root it writes under.
    pub scope_root: Namespace,
    /// The erasure generation it was admitted under.
    #[serde(default)]
    pub epoch: Option<Epoch>,
    /// The work.
    pub operation: WriteOperation,
    /// Stable, content-free last outcome; never an engine message.
    pub status: Option<String>,
}

/// The acknowledgement for a queued write: durable locally, not yet in the
/// engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ack {
    /// A learning, by fingerprint.
    Learned {
        /// The item's fingerprint.
        id: String,
    },
    /// A forget of `queued` ids.
    Forgotten {
        /// How many ids.
        queued: usize,
    },
}

impl Ack {
    pub(super) fn of(operation: &WriteOperation) -> Self {
        match operation {
            WriteOperation::Learn { item } => Self::Learned {
                id: item.fingerprint(),
            },
            WriteOperation::Forget { ids, .. } => Self::Forgotten { queued: ids.len() },
        }
    }
}

/// Runs one queued write against the engine.
///
/// `Err` is a stable error code (never an engine message); it becomes the
/// entry's status and stops the drain. A host bounds the run with its own
/// timeout and returns `"TIMEOUT"`.
#[async_trait]
pub trait WriteExecutor: Send + Sync {
    /// Applies `operation`.
    ///
    /// # Errors
    /// A stable error code.
    async fn run(&self, operation: &WriteOperation) -> Result<(), String>;
}

/// The erasure state of one owner: a generation that advances on every
/// fence, and a count of fences in force.
#[derive(Debug)]
pub struct WriteGate {
    process: String,
    admission: Mutex<()>,
    generation: AtomicU64,
    fences: AtomicU64,
}

/// Held while an erasure fences the owner; dropping it lifts the fence.
#[derive(Debug)]
#[must_use = "dropping the token lifts the fence"]
pub struct FenceToken<'a> {
    gate: &'a WriteGate,
    generation: u64,
}

/// An owned fence token for a gate shared through [`std::sync::Arc`].
///
/// Unlike [`FenceToken`], this token can be stored in a fence object that also
/// owns the gate. Dropping it lifts exactly one active fence.
#[must_use = "dropping the token lifts the fence"]
#[derive(Debug)]
pub(super) struct OwnedFenceToken {
    gate: std::sync::Arc<WriteGate>,
    generation: u64,
}

impl WriteGate {
    /// A gate for `process` (an id unique to this process run).
    #[must_use]
    pub fn new(process: impl Into<String>) -> Self {
        Self {
            process: process.into(),
            admission: Mutex::new(()),
            generation: AtomicU64::new(0),
            fences: AtomicU64::new(0),
        }
    }

    /// The current generation. Work captured at a generation is retired when
    /// it moves.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// The process id stamped on epochs.
    #[must_use]
    pub fn process(&self) -> &str {
        &self.process
    }

    pub(super) fn admission(&self) -> (bool, Epoch) {
        let _admission = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        (
            self.fences.load(Ordering::SeqCst) > 0,
            Epoch {
                process: self.process.clone(),
                generation: self.generation.load(Ordering::SeqCst),
            },
        )
    }

    /// Starts a fence: advances the generation, so earlier work is retired
    /// ([`super::PendingWrites::clear_stale`] with [`FenceToken::generation`])
    /// and in-flight drains stop.
    pub fn begin_fence(&self) -> FenceToken<'_> {
        let generation = self.start_fence();
        FenceToken {
            gate: self,
            generation,
        }
    }

    /// Starts an owned fence token while retaining this gate through an
    /// [`std::sync::Arc`].
    pub(super) fn begin_fence_owned(self: &std::sync::Arc<Self>) -> OwnedFenceToken {
        let generation = self.start_fence();
        OwnedFenceToken {
            gate: self.clone(),
            generation,
        }
    }

    fn start_fence(&self) -> u64 {
        let _admission = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        self.fences.fetch_add(1, Ordering::SeqCst);
        let generation = self.generation.load(Ordering::SeqCst) + 1;
        self.generation.store(generation, Ordering::SeqCst);
        generation
    }

    fn finish_fence(&self) {
        let _admission = self.admission.lock().unwrap_or_else(|e| e.into_inner());
        self.fences.fetch_sub(1, Ordering::SeqCst);
    }

    /// Whether any fence is in force.
    #[must_use]
    pub fn is_fenced(&self) -> bool {
        self.fences.load(Ordering::SeqCst) > 0
    }
}

impl FenceToken<'_> {
    /// The generation this fence opened.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for FenceToken<'_> {
    fn drop(&mut self) {
        self.gate.finish_fence();
    }
}

impl OwnedFenceToken {
    /// The generation this fence opened.
    #[must_use]
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }
}

impl Drop for OwnedFenceToken {
    fn drop(&mut self) {
        self.gate.finish_fence();
    }
}
