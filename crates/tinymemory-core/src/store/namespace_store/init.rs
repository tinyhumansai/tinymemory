//! `UnifiedMemory` constructor + schema bootstrap.
//!
//! Creates the workspace directories, opens the SQLite connection in WAL mode,
//! materialises every table the unified store owns (docs, kv, graph, vector
//! chunks, episodic FTS5, segments, events, profile), and runs idempotent
//! legacy-namespace migrations. Also exposes path / namespace helpers shared
//! by the rest of the unified module.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use parking_lot::Mutex;
use rusqlite::Connection;

use crate::store::safety::canonical_identifier;
use crate::store::types::GLOBAL_NAMESPACE;
use tinymemory_api::host::EmbeddingProvider;

use super::UnifiedMemory;

/// What an idempotent additive `ALTER TABLE … ADD COLUMN` actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdditiveMigration {
    /// The column did not exist and was added.
    Applied,
    /// SQLite reported `duplicate column name` — an older DB already has it.
    AlreadyPresent,
    /// SQLite reported `no such table` — the table has not been created yet
    /// (the fresh-install case for the profile Phase-3 columns, which run
    /// *before* `PROFILE_INIT_SQL`).
    TableAbsent,
}

/// Classify a rusqlite error as one of the two benign, expected outcomes of an
/// idempotent additive migration, or `None` when it is a genuine failure.
///
/// SQLite reports both conditions as the generic `SQLITE_ERROR` (code 1) with
/// no distinguishing extended code, so the message is the only signal. Every
/// other error — a malformed statement, a read-only or corrupt database, disk
/// I/O — must surface rather than be swallowed as "column already exists".
fn classify_additive_migration_error(err: &rusqlite::Error) -> Option<AdditiveMigration> {
    let rusqlite::Error::SqliteFailure(_, Some(message)) = err else {
        return None;
    };
    let lowered = message.to_ascii_lowercase();
    if lowered.contains("duplicate column name") {
        Some(AdditiveMigration::AlreadyPresent)
    } else if lowered.contains("no such table") {
        Some(AdditiveMigration::TableAbsent)
    } else {
        None
    }
}

/// Run one idempotent additive migration, swallowing **only** the
/// duplicate-column and missing-table cases.
///
/// Before this existed, every `ALTER TABLE` on the boot path matched
/// `Err(_)` and logged at `trace`, so a genuinely broken statement or an
/// unwritable database was indistinguishable from a no-op re-run and the store
/// came up silently missing a column.
pub(crate) fn apply_additive_migration(
    conn: &Connection,
    sql: &str,
    scope: &str,
) -> anyhow::Result<AdditiveMigration> {
    match conn.execute(sql, []) {
        Ok(_) => {
            tracing::debug!("[{scope}:init] additive migration applied: {sql}");
            Ok(AdditiveMigration::Applied)
        }
        Err(e) => match classify_additive_migration_error(&e) {
            Some(outcome @ AdditiveMigration::AlreadyPresent) => {
                tracing::trace!("[{scope}:init] column already present, skipping: {sql}");
                Ok(outcome)
            }
            Some(outcome @ AdditiveMigration::TableAbsent) => {
                tracing::trace!("[{scope}:init] table not created yet, skipping: {sql}");
                Ok(outcome)
            }
            Some(AdditiveMigration::Applied) => unreachable!("Applied is not an error outcome"),
            None => {
                tracing::error!("[{scope}:init] additive migration FAILED: {sql}: {e}");
                Err(anyhow::Error::new(e)
                    .context(format!("[{scope}:init] additive migration failed: {sql}")))
            }
        },
    }
}

impl UnifiedMemory {
    /// Open (or create) the unified store rooted at `workspace_dir`.
    ///
    /// Delegates to [`Self::new_with_memory_dir`] using the default
    /// `"memory"` subdirectory name. Safe to call on every boot.
    pub fn new(
        workspace_dir: &Path,
        embedder: Arc<dyn EmbeddingProvider>,
        _open_timeout_secs: Option<u64>,
    ) -> anyhow::Result<Self> {
        Self::new_with_memory_dir(workspace_dir, "memory", embedder, _open_timeout_secs)
    }

    /// Open (or create) a unified store using an explicit memory subdirectory
    /// name under `workspace_dir`.
    ///
    /// This enables multiple independent stores inside the same workspace (e.g.
    /// per-personality databases) without path collisions. `memory_subdir` is
    /// joined directly to `workspace_dir`, so `"memory-1"` yields
    /// `workspace_dir/memory-1/memory.db`.
    ///
    /// Creates the on-disk layout, runs all `CREATE TABLE` statements, and
    /// applies idempotent legacy-namespace migrations. Safe to call on every
    /// boot.
    pub fn new_with_memory_dir(
        workspace_dir: &Path,
        memory_subdir: &str,
        embedder: Arc<dyn EmbeddingProvider>,
        _open_timeout_secs: Option<u64>,
    ) -> anyhow::Result<Self> {
        use std::path::Component;
        anyhow::ensure!(!memory_subdir.is_empty(), "memory_subdir must not be empty");
        let subdir_path = Path::new(memory_subdir);
        anyhow::ensure!(
            subdir_path.components().count() == 1
                && subdir_path
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "memory_subdir must be a single relative path component without traversal"
        );
        let memory_dir = workspace_dir.join(subdir_path);
        let namespaces_dir = memory_dir.join("namespaces");
        let vectors_dir = memory_dir.join("vectors");
        std::fs::create_dir_all(&namespaces_dir)?;
        std::fs::create_dir_all(&vectors_dir)?;

        let db_path = memory_dir.join("memory.db");
        let conn = Connection::open(&db_path)?;
        // Active storage layout for the core memory domain:
        // - memory_docs: namespace-scoped source documents and markdown metadata.
        // - vector_chunks: chunked document text plus optional local embedding bytes.
        // - graph_namespace: namespace graph edges used for relation-first retrieval.
        // - graph_global: cross-namespace graph edges used as fallback/shared memory.
        // - kv_namespace: namespace-scoped durable preferences, decisions, and state.
        // - kv_global: global durable key-value memories outside a namespace scope.
        // Absorb concurrent write contention under cargo-llvm-cov and any other
        // scenario where background workers hold the write lock while a second
        // connection attempts a write. Without a timeout the driver returns
        // SQLITE_BUSY immediately, which causes test flakes and runtime errors.
        conn.busy_timeout(std::time::Duration::from_secs(15))
            .context("configure unified memory busy_timeout")?;

        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;

             CREATE TABLE IF NOT EXISTS memory_docs (
               document_id TEXT PRIMARY KEY,
               namespace TEXT NOT NULL,
               key TEXT NOT NULL,
               title TEXT NOT NULL,
               content TEXT NOT NULL,
               source_type TEXT NOT NULL,
               priority TEXT NOT NULL,
               tags_json TEXT NOT NULL,
               metadata_json TEXT NOT NULL,
               category TEXT NOT NULL,
               session_id TEXT,
               created_at REAL NOT NULL,
               updated_at REAL NOT NULL,
               markdown_rel_path TEXT NOT NULL,
               taint TEXT NOT NULL DEFAULT 'internal',
               UNIQUE(namespace, key)
             );
             CREATE INDEX IF NOT EXISTS idx_memory_docs_ns_updated ON memory_docs(namespace, updated_at DESC);

             CREATE TABLE IF NOT EXISTS kv_global (
               key TEXT PRIMARY KEY,
               value_json TEXT NOT NULL,
               updated_at REAL NOT NULL
             );

             CREATE TABLE IF NOT EXISTS kv_namespace (
               namespace TEXT NOT NULL,
               key TEXT NOT NULL,
               value_json TEXT NOT NULL,
               updated_at REAL NOT NULL,
               PRIMARY KEY(namespace, key)
             );
             CREATE INDEX IF NOT EXISTS idx_kv_namespace_ns ON kv_namespace(namespace);

             CREATE TABLE IF NOT EXISTS graph_global (
               subject TEXT NOT NULL,
               predicate TEXT NOT NULL,
               object TEXT NOT NULL,
               attrs_json TEXT NOT NULL,
               updated_at REAL NOT NULL,
               PRIMARY KEY(subject, predicate, object)
             );
             CREATE INDEX IF NOT EXISTS idx_graph_global_subject ON graph_global(subject, predicate);

             CREATE TABLE IF NOT EXISTS graph_namespace (
               namespace TEXT NOT NULL,
               subject TEXT NOT NULL,
               predicate TEXT NOT NULL,
               object TEXT NOT NULL,
               attrs_json TEXT NOT NULL,
               updated_at REAL NOT NULL,
               PRIMARY KEY(namespace, subject, predicate, object)
             );
             CREATE INDEX IF NOT EXISTS idx_graph_namespace_ns ON graph_namespace(namespace);
             CREATE INDEX IF NOT EXISTS idx_graph_namespace_subject ON graph_namespace(namespace, subject, predicate);

             CREATE TABLE IF NOT EXISTS vector_chunks (
               namespace TEXT NOT NULL,
               document_id TEXT NOT NULL,
               chunk_id TEXT NOT NULL,
               text TEXT NOT NULL,
               embedding BLOB,
               metadata_json TEXT NOT NULL,
               created_at REAL NOT NULL,
               updated_at REAL NOT NULL,
               model_signature TEXT,
               dim INTEGER,
               PRIMARY KEY(namespace, chunk_id)
             );
             CREATE INDEX IF NOT EXISTS idx_vector_chunks_ns_doc ON vector_chunks(namespace, document_id);",
        )?;

        // Tag vector_chunks with the embedding model that produced each vector
        // on existing databases (idempotent). Fresh installs get these from the
        // CREATE TABLE above; older DBs need the ALTERs so recall can exclude
        // vectors generated by a different embedding model (cross-model cosine is
        // garbage) and skip dimension mismatches instead of silently scoring 0.
        for sql in [
            "ALTER TABLE vector_chunks ADD COLUMN model_signature TEXT",
            "ALTER TABLE vector_chunks ADD COLUMN dim INTEGER",
        ] {
            apply_additive_migration(&conn, sql, "vector_chunks")?;
        }

        // Backfill the `taint` column on existing `memory_docs` databases.
        // Fresh installs get this via the CREATE TABLE above; older DBs need
        // the ALTER so retrieval can carry the provenance signal up to the
        // subconscious gate. Idempotent: a duplicate-column error on
        // re-application is expected (logged at trace).
        apply_additive_migration(
            &conn,
            "ALTER TABLE memory_docs ADD COLUMN taint TEXT NOT NULL DEFAULT 'internal'",
            "memory_docs",
        )?;

        // Create FTS5 episodic tables (episodic_log, episodic_fts, and their
        // triggers) so the Archivist can call episodic_insert immediately after
        // the store is initialised.
        conn.execute_batch(super::fts5::EPISODIC_INIT_SQL)?;

        // Conversation segmentation tables.
        conn.execute_batch(super::segments::SEGMENTS_INIT_SQL)?;

        // Backfill the (start_seq, end_seq) columns on existing databases
        // — fresh installs get them from SEGMENTS_INIT_SQL above; older DBs
        // need the ALTER TABLEs. Idempotent: a duplicate-column error is
        // expected and logged at trace level.
        for sql in super::segments::SEGMENTS_MIGRATIONS_SQL {
            apply_additive_migration(&conn, sql, "segments")?;
        }

        // Event extraction tables.
        conn.execute_batch(super::events::EVENTS_INIT_SQL)?;

        // Phase 3 (#566): add new columns to existing databases BEFORE running
        // PROFILE_INIT_SQL. PROFILE_INIT_SQL includes indexes that reference
        // state/stability/user_state — those CREATE INDEX statements fail on
        // pre-Phase-3 databases unless the columns already exist.
        // On fresh installs the table doesn't exist yet so ALTER TABLE fails
        // silently here; PROFILE_INIT_SQL then creates it with all columns.
        {
            use super::profile::PHASE3_COLUMNS_SQL;
            for sql in PHASE3_COLUMNS_SQL.iter() {
                // `TableAbsent` is the expected fresh-install outcome here:
                // these run *before* `PROFILE_INIT_SQL` creates `user_profile`.
                apply_additive_migration(&conn, sql, "profile")?;
            }
        }

        // User profile accumulation table (CREATE TABLE IF NOT EXISTS + all indexes).
        // On existing databases the table creation is a no-op; the index creation
        // succeeds because PHASE3_COLUMNS_SQL above has already added the columns.
        conn.execute_batch(super::profile::PROFILE_INIT_SQL)?;

        // Phase 3 indexes: idempotently restore performance indexes removed in #1616.
        // New installs get these from PROFILE_INIT_SQL; existing DBs get them here,
        // after PHASE3_COLUMNS_SQL has ensured the columns exist.
        {
            use super::profile::PHASE3_INDEXES_SQL;
            for sql in PHASE3_INDEXES_SQL {
                match conn.execute_batch(sql) {
                    Ok(_) => tracing::debug!("[profile:init] index applied: {sql}"),
                    Err(e) => {
                        tracing::warn!(
                            "[profile:init] index creation failed (non-fatal): {sql}: {e}"
                        );
                    }
                }
            }
        }

        // Idempotent legacy-namespace migration.
        //
        // Older writes via MemoryStoreTool packed the intended namespace into
        // the key as `"{namespace}/{actual_key}"` and stored the row under the
        // GLOBAL_NAMESPACE. Split those rows now so the new trait surface can
        // rely on the `namespace` column.
        //
        // The anti-join guard prevents duplicate-split collisions if a
        // post-split row already exists (UNIQUE(namespace, key) would otherwise
        // fail). Safe to run on every boot.
        let migrated = conn.execute(
            "UPDATE memory_docs
             SET namespace = substr(key, 1, instr(key, '/') - 1),
                 key = substr(key, instr(key, '/') + 1)
             WHERE namespace = ?1
               AND instr(key, '/') > 0
               AND NOT EXISTS (
                 SELECT 1 FROM memory_docs m2
                 WHERE m2.namespace = substr(memory_docs.key, 1, instr(memory_docs.key, '/') - 1)
                   AND m2.key = substr(memory_docs.key, instr(memory_docs.key, '/') + 1)
               )",
            rusqlite::params![GLOBAL_NAMESPACE],
        )?;
        if migrated > 0 {
            log::info!(
                "[memory] migrated {migrated} legacy `ns/key` rows out of the `{GLOBAL_NAMESPACE}` namespace"
            );
        }

        // Companion migration: `vector_chunks` rows keyed by `document_id` still
        // point at `GLOBAL_NAMESPACE` after the `memory_docs` split above, so
        // namespace-scoped recall would miss them. Re-home each chunk to its
        // document's new namespace. Idempotent: after both migrations run, no
        // chunk under GLOBAL_NAMESPACE maps to a document in another namespace.
        let chunks_migrated = conn.execute(
            "UPDATE vector_chunks
             SET namespace = (
                 SELECT namespace FROM memory_docs
                 WHERE memory_docs.document_id = vector_chunks.document_id
             )
             WHERE namespace = ?1
               AND document_id IN (
                 SELECT document_id FROM memory_docs WHERE namespace != ?1
               )",
            rusqlite::params![GLOBAL_NAMESPACE],
        )?;
        if chunks_migrated > 0 {
            log::info!(
                "[memory] migrated {chunks_migrated} vector_chunks rows out of the `{GLOBAL_NAMESPACE}` namespace"
            );
        }

        Ok(Self {
            workspace_dir: workspace_dir.to_path_buf(),
            memory_dir,
            db_path,
            vectors_dir,
            conn: Arc::new(Mutex::new(conn)),
            embedder,
        })
    }

    /// Root workspace directory holding `memory/` and its subtrees.
    pub fn workspace_dir(&self) -> &Path {
        &self.workspace_dir
    }

    /// Filesystem path of the SQLite database file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Directory used for vector-related sidecar files.
    pub fn vectors_dir(&self) -> &Path {
        &self.vectors_dir
    }

    pub(crate) fn now_ts() -> f64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    /// Canonical storage form of a namespace: PII-bearing namespaces are
    /// canonicalized (#5164), then path-hostile characters collapse to `_`.
    ///
    /// The PII step lives here, in the one funnel every namespace path already
    /// goes through — writes, reads, recall/search (`query.rs`), graph relations
    /// (`graph.rs`), deletes, and the on-disk `namespaces/<ns>/` directory — so
    /// a canonicalized write stays addressable by its original namespace
    /// instead of looking like a missing row and driving the caller to retry.
    pub(crate) fn sanitize_namespace(namespace: &str) -> String {
        let trimmed = canonical_identifier(namespace.trim());
        if trimmed.is_empty() {
            return GLOBAL_NAMESPACE.to_string();
        }
        let sanitized: String = trimmed
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '/' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        // `/` is kept so a namespace can be hierarchical, but a LEADING one
        // makes the result an absolute path — and `Path::join` with an
        // absolute path discards the base entirely, so
        // `memory_dir/namespaces/` vanishes and the namespace addresses
        // anywhere on the filesystem. `clear_namespace` calls
        // `remove_dir_all` on that path. (`..` is already neutralised above:
        // `.` is not in the allow-list, so it becomes `_`.)
        let sanitized = sanitized.trim_start_matches('/').to_string();
        if sanitized.is_empty() {
            return GLOBAL_NAMESPACE.to_string();
        }
        sanitized
    }

    /// Resolved memory subdirectory for this store instance (e.g.
    /// `workspace_dir/memory` for the default store, or a custom subdir for
    /// personality-specific stores).
    pub fn memory_dir(&self) -> &Path {
        &self.memory_dir
    }

    pub(crate) fn namespace_dir(&self, namespace: &str) -> PathBuf {
        self.memory_dir
            .join("namespaces")
            .join(Self::sanitize_namespace(namespace))
    }
}

#[cfg(test)]
#[path = "init_tests.rs"]
mod tests;
