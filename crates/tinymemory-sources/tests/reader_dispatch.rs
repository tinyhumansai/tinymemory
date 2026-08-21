//! Public reader-dispatch policy tests.

use tinymemory_sources::{
    readers::{is_locally_readable, reader_for},
    SourceKind,
};

#[test]
fn timer_dispatch_constructs_only_readers_that_never_need_network() {
    for kind in [SourceKind::Folder, SourceKind::Conversation] {
        assert!(is_locally_readable(&kind));
        assert_eq!(reader_for(&kind).map(|reader| reader.kind()), Some(kind));
    }

    for kind in [
        SourceKind::Composio,
        SourceKind::GithubRepo,
        SourceKind::TwitterQuery,
        SourceKind::RssFeed,
        SourceKind::WebPage,
    ] {
        assert!(!is_locally_readable(&kind));
        assert!(reader_for(&kind).is_none());
    }
}
