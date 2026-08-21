use super::*;

use crate::readers::SourceReader;

fn cached_reader(url: &str) -> RssReader {
    RssReader {
        cache: Mutex::new(Some(types::FeedCache {
            url: url.to_string(),
            fetched_at: Instant::now(),
            entries: vec![
                types::FeedEntry {
                    id: "plain".into(),
                    title: "Plain entry".into(),
                    body: "plain body".into(),
                    link: Some("https://example.com/plain".into()),
                    published: Some("2026-01-01T00:00:00Z".into()),
                    updated_at_ms: Some(1_767_225_600_000),
                },
                types::FeedEntry {
                    id: "html".into(),
                    title: "HTML entry".into(),
                    body: "<p>html body</p>".into(),
                    link: None,
                    published: None,
                    updated_at_ms: None,
                },
            ],
        })),
    }
}

fn rss_source(url: Option<&str>, max_items: Option<u32>) -> MemorySourceEntry {
    MemorySourceEntry {
        id: "feed".into(),
        label: "Feed".into(),
        kind: SourceKind::RssFeed,
        enabled: true,
        toolkit: None,
        connection_id: None,
        path: None,
        glob: None,
        url: url.map(str::to_string),
        branch: None,
        paths: Vec::new(),
        max_commits: None,
        max_issues: None,
        max_prs: None,
        query: None,
        since_days: None,
        max_items,
        selector: None,
        max_tokens_per_sync: None,
        max_cost_per_sync_usd: None,
        sync_depth_days: None,
    }
}

#[tokio::test]
async fn cached_feed_drives_list_and_read_without_network() {
    let url = "https://example.com/feed.xml";
    let reader = cached_reader(url);
    assert_eq!(reader.kind(), SourceKind::RssFeed);

    let listed = reader
        .list_items(&rss_source(Some(url), Some(1)), std::path::Path::new("."))
        .await
        .expect("cached list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "plain");
    assert_eq!(listed[0].updated_at_ms, Some(1_767_225_600_000));

    let plain = reader
        .read_item(
            &rss_source(Some(url), None),
            "plain",
            std::path::Path::new("."),
        )
        .await
        .expect("cached plaintext item");
    assert_eq!(plain.content_type, ContentType::Plaintext);
    assert_eq!(plain.metadata["link"], "https://example.com/plain");

    let html = reader
        .read_item(
            &rss_source(Some(url), None),
            "html",
            std::path::Path::new("."),
        )
        .await
        .expect("cached HTML item");
    assert_eq!(html.content_type, ContentType::Html);
}

#[tokio::test]
async fn rss_reader_reports_missing_configuration_and_items() {
    let reader = RssReader::new();
    let missing_url = rss_source(None, None);
    assert!(reader
        .list_items(&missing_url, std::path::Path::new("."))
        .await
        .is_err());
    assert!(reader
        .read_item(&missing_url, "anything", std::path::Path::new("."))
        .await
        .is_err());

    let url = "https://example.com/feed.xml";
    assert!(cached_reader(url)
        .read_item(
            &rss_source(Some(url), None),
            "missing",
            std::path::Path::new("."),
        )
        .await
        .is_err());
}

#[tokio::test]
async fn blocked_and_malformed_feed_urls_fail_closed_without_network() {
    for url in ["not a URL", "file:///etc/passwd", "http://127.0.0.1/feed"] {
        let error = RssReader::new()
            .list_items(&rss_source(Some(url), None), std::path::Path::new("."))
            .await
            .expect_err("unsafe URL must be refused");
        assert!(!error.to_string().is_empty());
    }
}

#[test]
fn parse_rss_extracts_items() {
    let xml = r#"<?xml version="1.0"?>
    <rss version="2.0">
    <channel>
        <title>Test Feed</title>
        <item>
            <title>First post</title>
            <link>https://example.com/1</link>
            <description>Body of first post</description>
        </item>
        <item>
            <title>Second post</title>
            <guid>guid-2</guid>
            <description>Body of second</description>
        </item>
    </channel>
    </rss>"#;

    let entries = parse_rss(xml).unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].title, "First post");
    assert_eq!(entries[0].id, "https://example.com/1");
    assert_eq!(entries[1].id, "guid-2");
}

#[test]
fn parse_atom_extracts_entries() {
    let xml = r#"<?xml version="1.0"?>
    <feed xmlns="http://www.w3.org/2005/Atom">
        <entry>
            <title>Atom entry</title>
            <id>urn:entry:1</id>
            <content>Content here</content>
            <link href="https://example.com/atom/1" />
        </entry>
    </feed>"#;

    let entries = parse_atom(xml).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].title, "Atom entry");
    assert_eq!(entries[0].id, "urn:entry:1");
    assert_eq!(
        entries[0].link.as_deref(),
        Some("https://example.com/atom/1")
    );
}

#[test]
fn parse_feed_detects_format() {
    let rss = "<rss><channel><item><title>T</title></item></channel></rss>";
    assert!(parse_feed_full(rss).is_ok());

    let atom = "<feed><entry><title>T</title><id>1</id></entry></feed>";
    assert!(parse_feed_full(atom).is_ok());

    assert!(parse_feed_full("<html></html>").is_err());
}

// ── Entry timestamps ───────────────────────────────────────────────

#[test]
fn parse_rss_emits_pubdate_timestamp() {
    let xml = r#"<rss version="2.0"><channel>
        <item>
            <title>Post</title>
            <guid>1</guid>
            <pubDate>Wed, 02 Oct 2002 13:00:00 GMT</pubDate>
        </item>
    </channel></rss>"#;

    let entries = parse_rss(xml).unwrap();
    // 2002-10-02T13:00:00Z in epoch milliseconds.
    assert_eq!(entries[0].updated_at_ms, Some(1_033_563_600_000));
}

#[test]
fn parse_atom_emits_updated_timestamp() {
    let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom">
        <entry>
            <title>Atom entry</title>
            <id>urn:entry:1</id>
            <updated>2026-03-01T12:00:00.000Z</updated>
        </entry>
    </feed>"#;

    let entries = parse_atom(xml).unwrap();
    // 2026-03-01T12:00:00Z in epoch milliseconds.
    assert_eq!(entries[0].updated_at_ms, Some(1_772_366_400_000));
}

#[test]
fn parse_item_without_timestamp_is_unknown() {
    let xml = "<rss><channel><item><title>T</title></item></channel></rss>";
    let entries = parse_rss(xml).unwrap();
    assert_eq!(entries[0].updated_at_ms, None);
}

#[test]
fn rss_timestamp_ms_rejects_garbage() {
    assert_eq!(rss_timestamp_ms("not a date"), None);
    assert_eq!(rss_timestamp_ms(""), None);
}

// ── CDATA unwrapping ────────────────────────────────────────────────

#[test]
fn extract_tag_unwraps_cdata() {
    // The common RSS shape `<description><![CDATA[<p>body</p>]]></description>`
    // must yield clean HTML, not the literal CDATA markers.
    let xml = "<description><![CDATA[<p>body</p>]]></description>";
    assert_eq!(
        extract_tag(xml, "description").as_deref(),
        Some("<p>body</p>")
    );
}

#[test]
fn extract_tag_does_not_entity_decode_inside_cdata() {
    // CDATA content is literal: `&lt;` must survive intact, not become `<`.
    let xml = "<description><![CDATA[Say &lt;tag&gt; literally]]></description>";
    assert_eq!(
        extract_tag(xml, "description").as_deref(),
        Some("Say &lt;tag&gt; literally")
    );
}

#[test]
fn extract_tag_entity_decodes_outside_cdata() {
    let xml = "<title>A &amp; B &lt;b&gt; bold</title>";
    assert_eq!(extract_tag(xml, "title").as_deref(), Some("A & B <b> bold"));
}

#[test]
fn extract_cdata_reuses_tag_extraction() {
    // `content:encoded` is typically CDATA-wrapped; extract_cdata must match
    // extract_tag on the same input.
    let xml = "<content:encoded><![CDATA[<p>full</p>]]></content:encoded>";
    assert_eq!(
        extract_cdata(xml, "content:encoded").as_deref(),
        Some("<p>full</p>")
    );
}

#[test]
fn parse_rss_description_with_cdata_is_clean() {
    let xml = r#"<rss version="2.0"><channel>
        <item>
            <title>Post</title>
            <guid>1</guid>
            <description><![CDATA[<p>Hello <b>world</b></p>]]></description>
        </item>
    </channel></rss>"#;

    let entries = parse_rss(xml).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, "<p>Hello <b>world</b></p>");
    assert!(!entries[0].body.contains("CDATA"));
}

#[test]
fn parse_rss_empty_description_falls_back_to_encoded_content() {
    // A present-but-empty `<description></description>` must not block the
    // `content:encoded` fallback — the item carries its body there instead.
    let xml = r#"<rss version="2.0"><channel>
        <item>
            <title>Post</title>
            <guid>1</guid>
            <description></description>
            <content:encoded><![CDATA[<p>Full body from content:encoded</p>]]></content:encoded>
        </item>
    </channel></rss>"#;

    let entries = parse_rss(xml).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, "<p>Full body from content:encoded</p>");
}

#[test]
fn parse_atom_empty_content_falls_back_to_summary() {
    // Mirrors the RSS `description`/`content:encoded` pair: an empty
    // `<content></content>` must fall through to a populated `<summary>`.
    let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom">
        <entry>
            <title>Atom entry</title>
            <id>urn:entry:1</id>
            <content></content>
            <summary>Summary body</summary>
        </entry>
    </feed>"#;

    let entries = parse_atom(xml).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].body, "Summary body");
}

// ── URL host redaction ──────────────────────────────────────────────

#[test]
fn url_host_redacts_userinfo() {
    // Credentials embedded in a source URL must never reach debug traces —
    // only the host is logged.
    assert_eq!(
        url_host("https://alice:secret@example.com/feed.xml"),
        "example.com"
    );
}

#[test]
fn url_host_drops_path_query_and_fragment() {
    assert_eq!(
        url_host("https://example.com/feed?token=abc#top"),
        "example.com"
    );
}

#[test]
fn url_host_fallback_strips_userinfo_without_scheme() {
    // Scheme-less values are unparseable by reqwest; the textual fallback
    // must still drop the `user:pass@` prefix and the path.
    assert_eq!(url_host("alice:secret@example.com/feed"), "example.com");
    assert_eq!(url_host("example.com/feed"), "example.com");
}

// ── Entity decoding ─────────────────────────────────────────────────

#[test]
fn decode_xml_entities_decodes_amp_last() {
    // `&amp;lt;` is the escaped form of `&lt;`; it must decode once to `&lt;`,
    // not twice to `<`.
    assert_eq!(decode_xml_entities("&amp;lt;"), "&lt;");
    assert_eq!(decode_xml_entities("&amp;amp;"), "&amp;");
}

#[test]
fn decode_xml_entities_handles_all_named() {
    assert_eq!(
        decode_xml_entities("&lt;b&gt; &quot;q&quot; &apos;a&apos; &amp; more"),
        "<b> \"q\" 'a' & more"
    );
}
