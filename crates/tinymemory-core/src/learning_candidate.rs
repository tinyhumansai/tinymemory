//! Learning candidate buffer — Phase 1 of issue #566.
//!
//! Defines the taxonomy types ([`FacetClass`], [`CueFamily`], [`EvidenceRef`]),
//! the unit-of-work [`LearningCandidate`], and a thread-safe ring-buffer
//! [`Buffer`] that collects candidates emitted by producers (Phase 2) before
//! they are consumed by the stability detector (Phase 3).
//!
//! The buffer is bounded: when full it evicts the oldest entry (FIFO overflow).
//! A global singleton is exposed via [`global()`]; individual tests may
//! construct their own [`Buffer`] with `Buffer::new(capacity)`.

use std::collections::VecDeque;
use std::sync::OnceLock;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

// ── Taxonomy ────────────────────────────────────────────────────────────────

/// Six-class taxonomy of what the cache can hold.
///
/// Keys are stored with a class prefix, e.g. `style/verbosity` or
/// `tooling/package_manager`. The class determines the half-life and
/// class budget used by the stability detector (Phase 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FacetClass {
    /// Communication style preferences — verbosity, formality, code format.
    Style,
    /// Stable biographical facts — timezone, name, language, role.
    Identity,
    /// Developer toolchain preferences — package manager, editor, OS, language.
    Tooling,
    /// Hard user vetoes — things the user has explicitly rejected or forbidden.
    Veto,
    /// Active user goals or ongoing projects.
    Goal,
    /// Preferred communication channel or platform.
    Channel,
}

/// How a candidate signal was produced — determines the weight multiplier
/// applied in the stability formula.
///
/// Higher-weight families contribute more strongly per evidence item.
/// The weights here are the canonical values from the Phase 1 plan:
/// `Explicit=1.0`, `Structural=0.9`, `Behavioral=0.7`, `Recurrence=0.6`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CueFamily {
    /// Direct declaration of intent by the user (highest weight — 1.0).
    ///
    /// Examples: "I prefer pnpm", "my timezone is PST", "always use terse replies".
    Explicit,
    /// Inferred from structured file or provider metadata (weight 0.9).
    ///
    /// Examples: `package.json#packageManager`, Gmail display name, Slack workspace.
    Structural,
    /// Inferred by heuristics or LLM from observed behaviour (weight 0.7).
    ///
    /// Examples: rolling edit-window ratio, correction-repeat signal, reflection hook output.
    Behavioral,
    /// Materialized from recurrence statistics in the memory tree (weight 0.6).
    ///
    /// Examples: tree-topic hotness, source_weight per channel.
    Recurrence,
}

impl CueFamily {
    /// Weight multiplier for this cue family in the stability formula.
    ///
    /// Phase 1 canonical values (matches the plan):
    /// `Explicit=1.0`, `Structural=0.9`, `Behavioral=0.7`, `Recurrence=0.6`.
    pub fn weight(self) -> f64 {
        match self {
            CueFamily::Explicit => 1.0,
            CueFamily::Structural => 0.9,
            CueFamily::Behavioral => 0.7,
            CueFamily::Recurrence => 0.6,
        }
    }
}

// ── Evidence reference ───────────────────────────────────────────────────────

/// Where a candidate's evidence points. Defined in the contract crate — the
/// memory store persists it, so both sides must name one type. See
/// [`tinymemory_api::host::EvidenceRef`].
pub use tinymemory_api::host::EvidenceRef;

// ── Learning candidate ───────────────────────────────────────────────────────

/// A single unit of learning evidence emitted by a producer and queued in the
/// [`Buffer`].
///
/// Each candidate asserts a specific `(class, key, value)` triple alongside
/// the evidence that backs it. The stability detector (Phase 3) aggregates
/// competing candidates for the same `(class, key)` pair and resolves them
/// into a single cache entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningCandidate {
    /// Which facet class this evidence touches.
    pub class: FacetClass,
    /// Canonical slug key within the class, e.g. `"verbosity"`, `"package_manager"`.
    ///
    /// Convention: `snake_case`, lowercase, no class prefix (the class carries that).
    pub key: String,
    /// Canonical value string, e.g. `"terse"`, `"pnpm"`, `"UTC+5:30"`.
    pub value: String,
    /// How this candidate was produced.
    pub cue_family: CueFamily,
    /// Pointer to the backing evidence in the memory substrate.
    pub evidence: EvidenceRef,
    /// Source-provided confidence hint, `0.0..=1.0`.
    ///
    /// This is an initial hint; the stability detector will reweight it using
    /// the cue-family weight and recency decay.
    pub initial_confidence: f64,
    /// When this candidate was observed, as seconds since the Unix epoch.
    pub observed_at: f64,
}

// ── Buffer ───────────────────────────────────────────────────────────────────

/// Thread-safe, bounded ring-buffer of [`LearningCandidate`] items.
///
/// Backed by a `parking_lot::Mutex<VecDeque<LearningCandidate>>`. When full
/// the oldest entry is evicted to make room (FIFO overflow). This keeps
/// memory bounded and naturally prioritises recent evidence.
///
/// The global singleton has a default capacity of 1024. Tests should
/// construct their own buffer via [`Buffer::new`].
pub struct Buffer {
    inner: Mutex<VecDeque<LearningCandidate>>,
    capacity: usize,
}

impl Buffer {
    /// Create a new buffer with the given capacity.
    ///
    /// `capacity` must be ≥ 1. A capacity of zero would make every `push`
    /// a no-op; callers should use a non-zero value.
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            inner: Mutex::new(VecDeque::with_capacity(cap)),
            capacity: cap,
        }
    }

    /// Push a candidate onto the buffer.
    ///
    /// If the buffer is already at capacity, the oldest entry is evicted first
    /// (FIFO overflow). This ensures the buffer always reflects the most recent
    /// evidence.
    pub fn push(&self, candidate: LearningCandidate) {
        let mut guard = self.inner.lock();
        if guard.len() >= self.capacity {
            guard.pop_front(); // evict oldest
        }
        guard.push_back(candidate);
    }

    /// Drain all candidates from the buffer and return them in FIFO order.
    ///
    /// After this call the buffer is empty.
    pub fn drain(&self) -> Vec<LearningCandidate> {
        let mut guard = self.inner.lock();
        guard.drain(..).collect()
    }

    /// Clone all candidates without removing them.
    ///
    /// Useful for inspection or debugging.
    pub fn peek(&self) -> Vec<LearningCandidate> {
        let guard = self.inner.lock();
        guard.iter().cloned().collect()
    }

    /// Current number of candidates in the buffer.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    /// Returns `true` when the buffer holds no candidates.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum number of candidates the buffer will hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

// ── Global singleton ─────────────────────────────────────────────────────────

static GLOBAL_BUFFER: OnceLock<Buffer> = OnceLock::new();

/// Return the global [`Buffer`] singleton.
///
/// Initialised on first call with a default capacity of 1024. All producers
/// push into this buffer; the stability detector drains it.
pub fn global() -> &'static Buffer {
    GLOBAL_BUFFER.get_or_init(|| Buffer::new(1024))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "learning_candidate_tests.rs"]
mod tests;
