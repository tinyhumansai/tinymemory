//! Live in-prompt window that advances as the host compacts a thread.

use std::collections::HashSet;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

/// The first user turn of the thread that is still verbatim in the prompt.
///
/// A pre-turn hook initializes the window from the latest checkpoint. Each
/// compaction advances it past the user turns folded into that checkpoint.
/// It never passes the user turn being answered, so live context stays out of
/// memory recall. Replaying an identical fold is idempotent, while duplicate
/// message text within one fold still counts as separate turns.
#[derive(Debug)]
pub struct InPromptFrom {
    from: AtomicU32,
    start: u32,
    ceiling: u32,
    folds: Mutex<Folds>,
}

#[derive(Debug, Default)]
struct Folds {
    seen: HashSet<u64>,
    first_head: Option<u64>,
}

fn fingerprint<'a>(texts: impl IntoIterator<Item = &'a str>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for text in texts {
        text.hash(&mut hasher);
    }
    hasher.finish()
}

impl InPromptFrom {
    /// Starts at `from`, capped at the user-turn index `ceiling`.
    #[must_use]
    pub fn new(from: u32, ceiling: u32) -> Self {
        let start = from.min(ceiling);
        Self {
            from: AtomicU32::new(start),
            start,
            ceiling,
            folds: Mutex::default(),
        }
    }

    /// The first user turn still verbatim in the prompt.
    #[must_use]
    pub fn get(&self) -> u32 {
        self.from.load(Ordering::Acquire)
    }

    /// Advances past folded user messages, two transcript positions per
    /// message. Duplicate folds are ignored; returns the current value for an
    /// empty fold.
    pub fn advance_past<I>(&self, folded_user_texts: I) -> u32
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let texts: Vec<I::Item> = folded_user_texts.into_iter().collect();
        let Some(first) = texts.first() else {
            return self.get();
        };
        let mut folds = self
            .folds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let from = self.get();
        if !folds
            .seen
            .insert(fingerprint(texts.iter().map(AsRef::as_ref)))
        {
            return from;
        }

        let head = fingerprint([first.as_ref()]);
        let step = u32::try_from(texts.len())
            .unwrap_or(u32::MAX)
            .saturating_mul(2);
        let next = match folds.first_head {
            None => {
                folds.first_head = Some(head);
                from.saturating_add(step)
            }
            Some(first_head) if first_head == head => from.max(self.start.saturating_add(step)),
            Some(_) => from.saturating_add(step),
        }
        .min(self.ceiling);
        self.from.store(next, Ordering::Release);
        next
    }
}
