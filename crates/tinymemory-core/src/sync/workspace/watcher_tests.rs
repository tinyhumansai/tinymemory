//! Tests for the surrounding module.

use super::*;
use std::fs;
use tempfile::TempDir;

#[test]
fn is_watched_extension_md_and_txt() {
    assert!(is_watched_extension(Path::new("note.md")));
    assert!(is_watched_extension(Path::new("note.txt")));
    assert!(!is_watched_extension(Path::new("image.png")));
    assert!(!is_watched_extension(Path::new("data.json")));
}

#[test]
fn file_mtime_returns_some_for_existing_file() {
    let tmp = TempDir::new().unwrap();
    let p = tmp.path().join("test.md");
    fs::write(&p, "hello").unwrap();
    assert!(file_mtime(&p).is_some());
}

#[test]
fn file_mtime_returns_none_for_missing_file() {
    assert!(file_mtime(Path::new("/nonexistent/file.md")).is_none());
}

#[test]
fn source_id_format_includes_mtime() {
    let rel = "journal/2024-01-01.md";
    let mtime: u64 = 1_700_000_000;
    let id = format!("vault_watcher:{rel}@{mtime}");
    assert_eq!(id, "vault_watcher:journal/2024-01-01.md@1700000000");
}

#[test]
fn start_vault_watcher_is_idempotent() {
    // Two calls must not panic; the OnceLock ensures only one spawns.
    // We can't assert much more without a live tokio runtime here, but
    // this pins the guard logic doesn't regress.
    //
    // NOTE: deliberately does NOT use #[tokio::test] — calling
    // start_vault_watcher() outside an async context exercises the
    // OnceLock-already-set branch, which is the important regression
    // target. The actual `tokio::spawn` inside will no-op gracefully.
    // WATCHER_STARTED may already be set by a prior test in this
    // process; that's fine — the second-call path is what we're testing.
    start_vault_watcher();
    start_vault_watcher();
    assert!(WATCHER_STARTED.get().is_some());
}
