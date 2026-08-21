//! Host recall policy — the *product* rules that decide what a recall must not
//! return, kept out of the storage engine.
//!
//! # Why this module exists
//!
//! `UnifiedMemory` is a candidate to move into the `tinycortex` crate. A memory
//! *engine* answers "which rows rank highest for this query"; it must not read
//! the host's execution context to do it. Until this module existed, the
//! `Memory::recall` implementation reached directly into
//! `crate::thread_context` — an agent-harness
//! task-local — from inside the persistence layer. Shipping that into a
//! persistence crate would have baked an OpenHuman chat-turn concept into a
//! storage engine, which is very hard to undo afterwards.
//!
//! The engine now takes the exclusion as an explicit parameter
//! ([`UnifiedMemory::recall_excluding_session`]); this module is the single
//! place that resolves it from ambient host state, and it is deliberately
//! **not** part of the `namespace_store` move set.
//!
//! # The self-echo guard
//!
//! When a recall runs inside a live chat turn, the harness has an ambient
//! "current thread" id (set by the web channel around `agent.run_single`, see
//! `web_chat::run_task`) and the turn's own user message was *just* auto-saved
//! as a `[conversation]` document tagged with that same id (see
//! `agent::harness::session::turn::core`). Without this guard the agent's own
//! `memory_recall` surfaces the very request that triggered it as the top
//! "relevant" memory and echoes the user's text back at them.
//!
//! Outside a chat turn — cron, CLI, tests, standalone — the ambient id is
//! `None`, no exclusion applies, and recall behaves exactly as it would with no
//! guard at all.
//!
//! # Known limitation (why the read is still ambient)
//!
//! Ideally the exclusion would be threaded down from the caller that knows the
//! session id, so nothing anywhere reads a task-local. That is not reachable
//! today: both [`crate::Memory`] and
//! [`crate::RecallOpts`] are re-exported verbatim from the
//! vendored `tinycortex` crate, `RecallOpts` has exactly five fields and none of
//! them is an exclusion, and the trait method takes no further argument. Every
//! in-turn caller (`memory_recall` tool, memory loader, channel context, flow
//! memory tools) holds an `Arc<dyn Memory>`, so it has no channel to push an
//! exclusion through even though it could resolve one. Widening `RecallOpts`
//! with an `exclude_session_id` field upstream in `tinycortex` is the change
//! that unblocks the rest of this hoist; at that point this module keeps the
//! resolution and the call sites populate the field.

/// Resolve the self-echo exclusion for a recall from ambient host state.
///
/// Returns the current chat thread id when this call runs inside a live agent
/// turn, and `None` everywhere else. Callers pass the result straight into
/// [`UnifiedMemory::recall_excluding_session`].
///
/// [`UnifiedMemory::recall_excluding_session`]:
/// crate::store::UnifiedMemory::recall_excluding_session
pub(crate) fn current_self_echo_exclusion() -> Option<String> {
    let exclusion = crate::thread_context::current_thread_id();
    if let Some(ref session_id) = exclusion {
        tracing::debug!(
            exclude_session_id = %session_id,
            "[memory:recall_policy] resolved same-session self-echo exclusion from ambient turn"
        );
    } else {
        tracing::trace!(
            "[memory:recall_policy] no ambient chat turn; recall runs without a self-echo exclusion"
        );
    }
    exclusion
}

#[cfg(test)]
#[path = "recall_policy_tests.rs"]
mod tests;
