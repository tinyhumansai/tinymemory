//! Tests for the surrounding module.

use super::*;
use tempfile::TempDir;
use tinymemory_api::host::NoopEmbedding;

#[test]
fn sanitize_namespace_defaults_and_scrubs() {
    assert_eq!(UnifiedMemory::sanitize_namespace(""), GLOBAL_NAMESPACE);
    assert_eq!(UnifiedMemory::sanitize_namespace("   "), GLOBAL_NAMESPACE);
    assert_eq!(
        UnifiedMemory::sanitize_namespace("team alpha/#1"),
        "team_alpha/_1"
    );
    assert_eq!(UnifiedMemory::sanitize_namespace("a-b_c/ok"), "a-b_c/ok");
}

/// #5164: the PII step lives in this one funnel so every namespace path
/// (write, read, recall/search, graph, delete, on-disk dir) derives the same
/// address. Strict-gated — scanner-built namespaces keep their identity.
#[test]
fn sanitize_namespace_canonicalizes_pii_and_preserves_scanner_namespaces() {
    let canonical = UnifiedMemory::sanitize_namespace("cliente-RFC-VECJ880326XK4");
    assert!(
        !canonical.contains("VECJ880326XK4"),
        "the national ID must not become the storage address, got: {canonical}"
    );
    assert!(
        canonical.contains("REDACTED_PII"),
        "expected a redaction placeholder, got: {canonical}"
    );
    // Idempotent, so read paths can canonicalize unconditionally.
    assert_eq!(UnifiedMemory::sanitize_namespace(&canonical), canonical);

    for namespace in ["whatsapp-web:12025551234@c.us", "skill-gmail", "global"] {
        assert_eq!(
            UnifiedMemory::sanitize_namespace(namespace),
            namespace.replace(['@', ':', '.'], "_"),
            "scanner-built namespace must only get the character scrub: {namespace}"
        );
    }
}

/// A namespace beginning with `/` must not escape the workspace:
/// `Path::join` with an absolute path DISCARDS the base, so
/// `memory_dir/namespaces/` would vanish and `clear_namespace`'s
/// `remove_dir_all` would run against an arbitrary absolute path.
#[test]
fn a_namespace_cannot_escape_the_workspace() {
    for hostile in [
        "/Users/me/Documents",
        "//tmp/x",
        "///etc",
        "/",
        "a/../../etc",
        "../../etc",
    ] {
        let sanitized = UnifiedMemory::sanitize_namespace(hostile);
        assert!(
            !sanitized.starts_with('/'),
            "{hostile:?} sanitized to {sanitized:?}, which is absolute"
        );
        let dir = std::path::Path::new("/w/memory")
            .join("namespaces")
            .join(&sanitized);
        assert!(
            dir.starts_with("/w/memory/namespaces"),
            "{hostile:?} escaped to {}",
            dir.display()
        );
    }
}

#[test]
fn namespace_dir_uses_sanitized_namespace() {
    let tmp = TempDir::new().unwrap();
    let memory = UnifiedMemory::new(tmp.path(), Arc::new(NoopEmbedding), None).unwrap();
    let dir = memory.namespace_dir("team alpha/#1");
    assert_eq!(
        dir,
        tmp.path()
            .join("memory")
            .join("namespaces")
            .join("team_alpha/_1")
    );
}

#[test]
fn new_with_memory_dir_creates_separate_db() {
    let tmp = TempDir::new().unwrap();
    let mem1 = UnifiedMemory::new(tmp.path(), Arc::new(NoopEmbedding), None).unwrap();
    let mem2 =
        UnifiedMemory::new_with_memory_dir(tmp.path(), "memory-1", Arc::new(NoopEmbedding), None)
            .unwrap();
    assert_ne!(mem1.db_path(), mem2.db_path());
    assert!(
        mem1.db_path().ends_with("memory/memory.db"),
        "expected mem1 db under memory/memory.db, got {:?}",
        mem1.db_path()
    );
    assert!(
        mem2.db_path().ends_with("memory-1/memory.db"),
        "expected mem2 db under memory-1/memory.db, got {:?}",
        mem2.db_path()
    );
    assert!(mem1.db_path().exists(), "mem1 db file must exist on disk");
    assert!(mem2.db_path().exists(), "mem2 db file must exist on disk");
}

// ── Additive-migration error narrowing ──────────────────────────────
//
// Before `apply_additive_migration` existed these four boot-path
// `ALTER TABLE`s matched `Err(_)` and logged at `trace`, so a genuinely
// failing statement was indistinguishable from "column already exists".

fn scratch_conn() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE TABLE t (a TEXT);").unwrap();
    conn
}

#[test]
fn additive_migration_applies_a_new_column() {
    let conn = scratch_conn();
    assert_eq!(
        apply_additive_migration(&conn, "ALTER TABLE t ADD COLUMN b TEXT", "test").unwrap(),
        AdditiveMigration::Applied
    );
}

#[test]
fn additive_migration_swallows_duplicate_column() {
    let conn = scratch_conn();
    apply_additive_migration(&conn, "ALTER TABLE t ADD COLUMN b TEXT", "test").unwrap();
    assert_eq!(
        apply_additive_migration(&conn, "ALTER TABLE t ADD COLUMN b TEXT", "test").unwrap(),
        AdditiveMigration::AlreadyPresent
    );
}

#[test]
fn additive_migration_swallows_missing_table() {
    let conn = scratch_conn();
    assert_eq!(
        apply_additive_migration(&conn, "ALTER TABLE nope ADD COLUMN b TEXT", "test").unwrap(),
        AdditiveMigration::TableAbsent
    );
}

#[test]
fn additive_migration_surfaces_a_genuine_failure() {
    let conn = scratch_conn();
    // Not a duplicate column and not a missing table: a malformed
    // statement. Swallowing this would leave the store silently missing a
    // column that recall depends on, with only a trace-level breadcrumb.
    let err = apply_additive_migration(&conn, "ALTER TABLE t ADD COLUMN", "test")
        .expect_err("a real ALTER TABLE failure must surface, not be swallowed as idempotent");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("additive migration failed"),
        "error must name the failing migration, got: {rendered}"
    );
}

#[test]
fn additive_migration_surfaces_a_readonly_database() {
    // A read-only DB is the real-world shape of this defect: every ALTER
    // fails, the old code logged each at trace, and the store came up
    // missing columns that recall depends on.
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("ro.db");
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (a TEXT);").unwrap();
    }
    let conn =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(
        apply_additive_migration(&conn, "ALTER TABLE t ADD COLUMN b TEXT", "test").is_err(),
        "a read-only database must fail the migration, not look idempotent"
    );
}

#[test]
fn connection_has_busy_timeout_set() {
    let tmp = TempDir::new().unwrap();
    let memory = UnifiedMemory::new(tmp.path(), Arc::new(NoopEmbedding), None).unwrap();
    let conn = memory.conn.lock();
    // SQLite reports busy_timeout as a PRAGMA; 0 means no timeout.
    let timeout: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert!(
        timeout > 0,
        "busy_timeout must be non-zero to absorb write contention, got {timeout}"
    );
}
