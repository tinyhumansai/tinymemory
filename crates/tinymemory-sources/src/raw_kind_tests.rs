//! Tests for stable raw archive directory names.

use super::RawKind;

#[test]
fn every_raw_kind_has_a_distinct_plural_directory() {
    let cases = [
        (RawKind::Email, "emails"),
        (RawKind::Chat, "chats"),
        (RawKind::Document, "documents"),
        (RawKind::Contact, "contacts"),
        (RawKind::Post, "posts"),
        (RawKind::Commit, "commits"),
        (RawKind::Issue, "issues"),
        (RawKind::PullRequest, "prs"),
    ];
    let mut directories = std::collections::HashSet::new();
    for (kind, expected) in cases {
        assert_eq!(kind.as_dir(), expected);
        assert!(
            directories.insert(kind.as_dir()),
            "duplicate directory {expected}"
        );
    }
}
