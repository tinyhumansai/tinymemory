//! # Memory Trait Implementation
//!
//! This module implements the core `Memory` trait for the `UnifiedMemory`
//! struct. This allows `UnifiedMemory` to be used as a generic memory backend
//! within the OpenHuman system.
//!
//! Callers pass an explicit `namespace` on `store`/`get`/`forget` and via
//! `RecallOpts` on `recall`. When a `namespace` is omitted on `recall`/`list`,
//! the implementation falls back to `GLOBAL_NAMESPACE` (legacy behavior), which
//! Phase B/C will tighten once the memory tools pass namespace explicitly.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use parking_lot::Mutex;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::store::namespace_store::fts5;
use crate::store::types::{NamespaceDocumentInput, GLOBAL_NAMESPACE};
use crate::traits::{
    Memory, MemoryCategory, MemoryEntry, MemoryTaint, NamespaceSummary, RecallOpts,
};
use anyhow::Context;

use super::namespace_store::UnifiedMemory;

/// Convert a UNIX timestamp (f64) to RFC3339 string.
fn timestamp_to_rfc3339(ts: f64) -> String {
    let secs = ts.trunc() as i64;
    let nanos = ((ts.fract()) * 1_000_000_000.0).round() as u32;
    Utc.timestamp_opt(secs, nanos.min(999_999_999))
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| format!("{ts}"))
}

/// Normalize a namespace value: trim whitespace and fall back to
/// `GLOBAL_NAMESPACE` for `None` or blank/whitespace-only inputs. This ensures
/// that `recall`/`list` calls derived from user or RPC input never silently
/// receive an empty string that misses the global namespace.
fn normalize_namespace(namespace: Option<&str>) -> &str {
    namespace
        .map(str::trim)
        .filter(|ns| !ns.is_empty())
        .unwrap_or(GLOBAL_NAMESPACE)
}

/// Helper to convert a raw string category from the database into a `MemoryCategory`.
///
/// The store persists a category via its `Display` form, and the current
/// TinyCortex format renders `Custom(name)` as `custom:{name}` (so `Custom("core")`
/// stays distinct from `Core`). Parse back through `FromStr` — the true inverse of
/// `Display` — so the `custom:` prefix is stripped symmetrically. Wrapping the raw
/// string in `Custom(_)` instead (the previous behaviour) double-prefixed on
/// read-back once the wire format gained the prefix. An empty stored value has no
/// `FromStr` mapping, so it falls back to an empty `Custom` (matching the prior
/// catch-all for that degenerate case).
fn memory_category_from_stored(raw: &str) -> MemoryCategory {
    raw.parse().unwrap_or_else(|error| {
        tracing::debug!(
            category_chars = raw.chars().count(),
            reason = %error,
            "[memory_store] invalid stored category; preserving as custom"
        );
        MemoryCategory::Custom(raw.to_string())
    })
}

impl UnifiedMemory {
    /// Ranked recall with the same-session self-echo exclusion supplied
    /// **explicitly** by the caller.
    ///
    /// This is the engine body: given a query, a limit, [`RecallOpts`], and an
    /// optional session id to exclude, it produces the ranked result. It reads
    /// no ambient host state, so it is driveable from a test, a CLI, or a
    /// future embedding host without an agent harness in the picture.
    ///
    /// `exclude_session_id` drops documents tagged with that session before
    /// ranking (not after), so `limit` is never consumed by rows the caller
    /// asked not to see. `None` applies no exclusion at all.
    ///
    /// The host policy that decides *what* to exclude lives in
    /// `crate::store::recall_policy`; the [`Memory::recall`]
    /// impl below is the thin adapter that joins the two.
    pub async fn recall_excluding_session(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
        exclude_session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let namespace = normalize_namespace(opts.namespace);

        if let Some(excluded) = exclude_session_id {
            tracing::debug!(
                "[memory-trait] recall applying same-session exclusion namespace={namespace} \
                 exclude_session_id={excluded}"
            );
        }
        let ranked = self
            .query_namespace_ranked_excluding_session(
                namespace,
                query,
                limit as u32,
                exclude_session_id,
            )
            .await
            .map_err(anyhow::Error::msg)?;

        let min_score = opts.min_score.unwrap_or(f64::NEG_INFINITY);
        let mut out: Vec<MemoryEntry> = ranked
            .into_iter()
            .enumerate()
            .filter(|(_, r)| r.score >= min_score)
            .map(|(idx, r)| MemoryEntry {
                id: format!("{namespace}:{idx}"),
                key: r.key,
                content: r.content,
                namespace: Some(namespace.to_string()),
                category: memory_category_from_stored(&r.category),
                timestamp: Utc::now().to_rfc3339(),
                session_id: None,
                score: Some(r.score),
                // Surface the real taint persisted on `memory_docs` so the
                // subconscious gate can decide whether to escalate the
                // turn origin to `SubconsciousTainted` when this entry
                // lands in a tick's context window.
                taint: r.taint,
            })
            .collect();

        if let Some(ref cat) = opts.category {
            let want = cat.to_string();
            out.retain(|e| e.category.to_string() == want);
        }

        if let Some(sid) = opts.session_id {
            // Synchronous SQL behind the connection mutex — run it on the
            // blocking pool rather than an executor thread. A join failure is
            // folded into the same non-fatal arm as a query failure below.
            let fetched = {
                let conn = Arc::clone(&self.conn);
                let session = sid.to_owned();
                tokio::task::spawn_blocking(move || fts5::episodic_session_entries(&conn, &session))
                    .await
                    .context("join episodic session entries")
                    .and_then(|entries| entries)
            };
            let episodic_entries = match fetched {
                Ok(entries) => {
                    tracing::debug!(
                        "[memory-trait] loaded {} episodic entries for session={sid}",
                        entries.len()
                    );
                    entries
                }
                Err(e) => {
                    tracing::warn!(
                        "[memory-trait] failed to load episodic entries for session={sid}: {e}"
                    );
                    Vec::new()
                }
            };

            let query_lower = query.to_lowercase();
            let query_terms: Vec<&str> = query_lower.split_whitespace().collect();
            for entry in episodic_entries {
                let content_lower = entry.content.to_lowercase();
                let matched_count = query_terms
                    .iter()
                    .filter(|term| content_lower.contains(*term))
                    .count();
                if matched_count == 0 {
                    continue;
                }
                let match_score = matched_count as f64 / query_terms.len().max(1) as f64;
                if match_score < min_score {
                    continue;
                }
                let ts_rfc3339 = timestamp_to_rfc3339(entry.timestamp);

                out.push(MemoryEntry {
                    id: format!("episodic:{}", entry.id.unwrap_or(0)),
                    key: format!("{}:{}", entry.session_id, entry.role),
                    content: entry.content,
                    namespace: Some(namespace.to_string()),
                    category: MemoryCategory::Conversation,
                    timestamp: ts_rfc3339,
                    session_id: Some(entry.session_id),
                    score: Some(match_score),
                    taint: crate::MemoryTaint::Internal,
                });
            }
        }

        // ── Cross-session episodic recall (#1505) ────────────────────────
        //
        // When the caller asks for cross-session memory, pull FTS5-ranked
        // hits from every other session in the same workspace. Workspace
        // isolation is enforced by the SQLite DB path itself (one DB per
        // workspace == one DB per user) so this can never leak across
        // users. The current `session_id` (if any) is excluded so the
        // caller doesn't double-count its own chat history — those rows
        // already came in via the same-session path above.
        if opts.cross_session {
            let exclude = opts.session_id;
            // Same blocking-pool hop as the same-session fetch above.
            let fetched = {
                let conn = Arc::clone(&self.conn);
                let query = query.to_owned();
                let exclude = exclude.map(str::to_owned);
                tokio::task::spawn_blocking(move || {
                    fts5::episodic_cross_session_search(&conn, &query, limit, exclude.as_deref())
                })
                .await
                .context("join cross-session episodic search")
                .and_then(|entries| entries)
            };
            let cross_entries = match fetched {
                Ok(entries) => {
                    tracing::debug!(
                            "[memory-trait] cross-session episodic recall returned {} entries (exclude={:?})",
                            entries.len(),
                            exclude
                        );
                    entries
                }
                Err(e) => {
                    tracing::warn!(
                        "[memory-trait] cross-session episodic recall failed (non-fatal): {e}"
                    );
                    Vec::new()
                }
            };

            // Normalise FTS5 rank into a [0..1] keyword-style score by
            // reusing the same matched-terms heuristic as the same-session
            // branch. This keeps the score scale consistent across hits so
            // the downstream sort doesn't preferentially up-rank one branch
            // over the other.
            let query_lower = query.to_lowercase();
            let query_terms: Vec<&str> = query_lower.split_whitespace().collect();
            for entry in cross_entries {
                let content_lower = entry.content.to_lowercase();
                let matched_count = query_terms
                    .iter()
                    .filter(|term| content_lower.contains(*term))
                    .count();
                if matched_count == 0 {
                    // FTS5 surfaced a porter-stemmed match with zero
                    // literal query-term overlap. Drop it — the previous
                    // `0.1_f64.max(min_score)` floor defeated the
                    // downstream `score >= min_relevance_score` gate
                    // (when min_score==0.4 the floor also became 0.4),
                    // so those rows always survived. Skip outright.
                    continue;
                }
                let match_score = matched_count as f64 / query_terms.len().max(1) as f64;
                if match_score < min_score {
                    continue;
                }
                let ts_rfc3339 = timestamp_to_rfc3339(entry.timestamp);
                out.push(MemoryEntry {
                    id: format!("episodic-cross:{}", entry.id.unwrap_or(0)),
                    key: format!("{}:{}", entry.session_id, entry.role),
                    content: entry.content,
                    namespace: Some(namespace.to_string()),
                    category: MemoryCategory::Conversation,
                    timestamp: ts_rfc3339,
                    session_id: Some(entry.session_id),
                    score: Some(match_score),
                    taint: crate::MemoryTaint::Internal,
                });
            }
        }

        if opts.session_id.is_some() || opts.cross_session {
            out.sort_by(|a, b| {
                b.score
                    .unwrap_or(0.0)
                    .partial_cmp(&a.score.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            out.truncate(limit);
        }

        Ok(out)
    }
}

// ── Blocking SQL bodies ──────────────────────────────────────────────────────
//
// The connection is a `parking_lot::Mutex<rusqlite::Connection>`: every SQL
// call is synchronous and holds the lock for its duration, so running one on
// an executor thread stalls every task scheduled there. Each `Memory` method
// below owns its parameters, hops to `spawn_blocking`, and runs its body here;
// the bodies are associated fns (not `&self` methods) because the closure must
// be `'static` and cannot borrow the store.

/// One `memory_docs` row as `get` selects it:
/// `(document_id, key, content, updated_at, category, taint, session_id)`.
type MemoryDocRow = (String, String, String, f64, String, String, Option<String>);

impl UnifiedMemory {
    fn get_blocking(
        conn: &Arc<Mutex<Connection>>,
        ns: &str,
        key: &str,
    ) -> anyhow::Result<Option<MemoryEntry>> {
        let conn = conn.lock();
        // `session_id` is selected here for the same reason `list` selects it:
        // it is a column on this row, and a `get` that dropped it made the two
        // readers disagree about one record. The contract's round-trip
        // assertion catches exactly that (`tinymemory_conformance`), and it was
        // invisible until #18 §A3 let this store be bound as a driver at all.
        let row: Option<MemoryDocRow> = conn
            .query_row(
                "SELECT document_id, key, content, updated_at, category, taint, session_id
                 FROM memory_docs WHERE namespace = ?1 AND key = ?2 LIMIT 1",
                params![ns, key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;
        Ok(row.map(
            |(id, key, content, updated_at, category, taint_str, session_id)| MemoryEntry {
                id,
                key,
                content,
                namespace: Some(ns.to_string()),
                category: memory_category_from_stored(&category),
                timestamp: timestamp_to_rfc3339(updated_at),
                session_id,
                score: None,
                taint: crate::MemoryTaint::from_db_str(&taint_str),
            },
        ))
    }

    fn list_blocking(
        conn: &Arc<Mutex<Connection>>,
        ns: &str,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let conn = conn.lock();
        let mut stmt = conn.prepare(
            "SELECT document_id, key, content, category, session_id, updated_at, taint
             FROM memory_docs WHERE namespace = ?1 ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map(params![ns], |row| {
            let stored_category: String = row.get(3)?;
            Ok(MemoryEntry {
                id: row.get(0)?,
                key: row.get(1)?,
                content: row.get(2)?,
                namespace: Some(ns.to_string()),
                category: memory_category_from_stored(&stored_category),
                session_id: row.get(4)?,
                timestamp: timestamp_to_rfc3339(row.get(5)?),
                score: None,
                taint: crate::MemoryTaint::from_db_str(&row.get::<_, String>(6)?),
            })
        })?;
        let mut entries = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        if let Some(category) = category {
            entries.retain(|entry| &entry.category == category);
        }
        if let Some(session_id) = session_id {
            entries.retain(|entry| entry.session_id.as_deref() == Some(session_id));
        }
        Ok(entries)
    }

    fn forget_lookup_blocking(
        conn: &Arc<Mutex<Connection>>,
        ns: &str,
        key: &str,
    ) -> anyhow::Result<Option<String>> {
        let conn = conn.lock();
        Ok(conn
            .query_row(
                "SELECT document_id FROM memory_docs WHERE namespace = ?1 AND key = ?2 LIMIT 1",
                params![ns, key],
                |row| row.get(0),
            )
            .optional()?)
    }

    fn namespace_summaries_blocking(
        conn: &Arc<Mutex<Connection>>,
    ) -> anyhow::Result<Vec<NamespaceSummary>> {
        let conn = conn.lock();
        let mut stmt = conn.prepare(
            "SELECT namespace, COUNT(*) AS n, MAX(updated_at) AS last
             FROM memory_docs
             GROUP BY namespace
             ORDER BY namespace",
        )?;
        let rows = stmt.query_map([], |row| {
            let ns: String = row.get(0)?;
            let count: i64 = row.get(1)?;
            let last: Option<f64> = row.get(2)?;
            Ok((ns, count, last))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (ns, count, last) = r?;
            out.push(NamespaceSummary {
                namespace: ns,
                count: usize::try_from(count).unwrap_or(0),
                last_updated: last.map(timestamp_to_rfc3339),
            });
        }
        Ok(out)
    }

    fn count_blocking(conn: &Arc<Mutex<Connection>>) -> anyhow::Result<usize> {
        let conn = conn.lock();
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM memory_docs", [], |row| row.get(0))?;
        usize::try_from(count).context("negative count")
    }
}

#[async_trait]
impl Memory for UnifiedMemory {
    fn name(&self) -> &str {
        "namespace"
    }

    async fn store(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
    ) -> anyhow::Result<()> {
        // The default `store` entry point is user-driven; ingest paths
        // come in via `store_with_taint`.
        self.store_with_taint(
            namespace,
            key,
            content,
            category,
            session_id,
            MemoryTaint::Internal,
        )
        .await
    }

    async fn store_with_taint(
        &self,
        namespace: &str,
        key: &str,
        content: &str,
        category: MemoryCategory,
        session_id: Option<&str>,
        taint: MemoryTaint,
    ) -> anyhow::Result<()> {
        let ns = if namespace.trim().is_empty() {
            GLOBAL_NAMESPACE.to_string()
        } else {
            namespace.to_string()
        };
        self.upsert_document(NamespaceDocumentInput {
            namespace: ns,
            key: key.to_string(),
            title: key.to_string(),
            content: content.to_string(),
            source_type: "chat".to_string(),
            priority: "medium".to_string(),
            tags: Vec::new(),
            metadata: json!({}),
            category: category.to_string(),
            session_id: session_id.map(str::to_string),
            document_id: None,
            taint,
        })
        .await
        .map(|_| ())
        .map_err(anyhow::Error::msg)
    }

    async fn recall(
        &self,
        query: &str,
        limit: usize,
        opts: RecallOpts<'_>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        // Host policy seam: the exclusion the engine applies is resolved
        // here, at the trait adapter, and handed down as a parameter. The
        // engine itself (`recall_excluding_session`) reads no ambient state.
        // See `store::recall_policy` for why the resolution is still ambient
        // and what would let it be pushed further up.
        let exclude_session_id = super::recall_policy::current_self_echo_exclusion();
        self.recall_excluding_session(query, limit, opts, exclude_session_id.as_deref())
            .await
    }

    async fn recall_relevant_by_vector(
        &self,
        namespace: &str,
        query: &str,
        limit: usize,
        min_vector_similarity: f64,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let hits = self
            .query_namespace_hits(namespace, query, limit as u32)
            .await
            .map_err(anyhow::Error::msg)?;
        Ok(hits
            .into_iter()
            .filter(|h| h.score_breakdown.vector_similarity >= min_vector_similarity)
            .filter(|h| !h.content.trim().is_empty())
            .map(|h| (h.key, h.content))
            .collect())
    }

    async fn get(&self, namespace: &str, key: &str) -> anyhow::Result<Option<MemoryEntry>> {
        // Address the row the way `store` wrote it: `upsert_document` stores
        // `sanitize_namespace(namespace)` and `canonical_document_key(key)`, so
        // looking up the raw caller values misses whenever either transform
        // changed anything — the caller then reads the row as absent and stores
        // it again, which is the retry loop behind #5164.
        let ns = UnifiedMemory::sanitize_namespace(namespace);
        let key = crate::store::safety::canonical_document_key(key);
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || Self::get_blocking(&conn, &ns, &key))
            .await
            .context("join Memory::get")?
    }

    async fn list(
        &self,
        namespace: Option<&str>,
        category: Option<&MemoryCategory>,
        session_id: Option<&str>,
    ) -> anyhow::Result<Vec<MemoryEntry>> {
        let ns = UnifiedMemory::sanitize_namespace(normalize_namespace(namespace));
        let category = category.cloned();
        let session_id = session_id.map(str::to_owned);
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            Self::list_blocking(&conn, &ns, category.as_ref(), session_id.as_deref())
        })
        .await
        .context("join Memory::list")?
    }

    async fn forget(&self, namespace: &str, key: &str) -> anyhow::Result<bool> {
        // Same write/read symmetry as `get` above (#5164): a `forget` that
        // addresses the raw caller identifiers can never delete a row whose
        // namespace or key was canonicalized on the way in.
        let ns = UnifiedMemory::sanitize_namespace(namespace);
        let key = crate::store::safety::canonical_document_key(key);
        let row: Option<String> = {
            let conn = Arc::clone(&self.conn);
            let ns = ns.clone();
            tokio::task::spawn_blocking(move || Self::forget_lookup_blocking(&conn, &ns, &key))
                .await
                .context("join Memory::forget")??
        };
        let Some(document_id) = row else {
            return Ok(false);
        };
        // `delete_document` awaits internally (graph upkeep, sidecar removal),
        // so only the synchronous lookup above runs on the blocking pool.
        self.delete_document(&ns, &document_id)
            .await
            .map_err(anyhow::Error::msg)?;
        Ok(true)
    }

    async fn namespace_summaries(&self) -> anyhow::Result<Vec<NamespaceSummary>> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || Self::namespace_summaries_blocking(&conn))
            .await
            .context("join Memory::namespace_summaries")?
    }

    async fn count(&self) -> anyhow::Result<usize> {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || Self::count_blocking(&conn))
            .await
            .context("join Memory::count")?
    }

    async fn health_check(&self) -> bool {
        self.workspace_dir.exists() && self.db_path.exists()
    }
}

#[cfg(test)]
#[path = "memory_trait_tests.rs"]
mod tests;
