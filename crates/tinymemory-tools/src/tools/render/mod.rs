//! Turning engine responses into the compact JSON a model reads.
//!
//! Results carry what a model can act on and nothing else: a hit is its id,
//! kind, text, score (rounded to four places), a learning's confidence, and a
//! metadata subset (the source kind and id, `file_path`, `url`, `thread_id`,
//! `tags`, `observed_at`). The namespace is never rendered; it is the host's.
//! Absent optional fields are left out rather than written as `null`.

use chrono::SecondsFormat;
use serde_json::{Map, Value, json};
use tinymemory_api::{
    Citation, ExplorePage, FetchPage, ForgetReport, Hit, ItemId, ListPage, MemoryMeta,
    RecallAnswer, StoreReceipt,
};

const EVIDENCE_TOKENS: usize = 2_500;
const PARTLY_SEARCHED: &str = "\n\n(Memory was only partly searched";

/// `memory_recall`'s result: `{answer, citations: [...]}`.
pub(crate) fn recall(answer: &RecallAnswer) -> Value {
    json!({
        "answer": answer.answer,
        "citations": answer.citations.iter().map(citation).collect::<Vec<_>>(),
    })
}

/// `memory_fetch`'s result: `{hits: [...], next_cursor?}`.
pub(crate) fn fetch(page: &FetchPage) -> Value {
    paged("hits", &page.hits, page.next_cursor.as_deref())
}

/// The single-name tool's fetch text, with the engine cursor returned beside it.
pub(crate) fn fetch_page_text(page: &FetchPage) -> String {
    let mut output = match page.hits.len() {
        0 => "Nothing in memory matches.".to_string(),
        1 => "1 match.".to_string(),
        count => format!("Top {count} matches, best first."),
    };
    for hit in &page.hits {
        output.push_str("\n\n");
        if let Some(at) = hit.meta.observed_at {
            output.push_str(&format!("[{}] ", at.format("%Y-%m-%d %H:%M UTC")));
        }
        match hit.kind {
            tinymemory_api::ItemKind::Learning => output.push_str("learning: "),
            tinymemory_api::ItemKind::Document => output.push_str("document: "),
            tinymemory_api::ItemKind::Conversation => {}
        }
        output.push_str(hit.text.trim());
        output.push_str(&format!("\n(id {})", hit.id));
    }
    output
}

/// Caps evidence at about 2,500 tokens without splitting an entry's lines.
///
/// The byte threshold matches the host tool's four-characters-per-token
/// estimate. An oversized first line is retained whole, the partially
/// searched closing note is preserved, and the rest stays reachable through
/// fetch.
pub(crate) fn capped_evidence(evidence: &str) -> String {
    const SEPARATOR: &str = "\n\n---\n\n";
    let budget = EVIDENCE_TOKENS * 4;
    if evidence.len() <= budget {
        return evidence.to_string();
    }
    let (body, closing) = match evidence.rfind(PARTLY_SEARCHED) {
        Some(at) => (&evidence[..at], &evidence[at..]),
        None => (evidence, ""),
    };
    let entries: Vec<&str> = body.split(SEPARATOR).collect();
    let mut output = String::new();
    let mut whole = 0;
    let mut cut_short = false;
    for entry in &entries {
        let joint = if output.is_empty() { "" } else { SEPARATOR };
        if output.len() + joint.len() + entry.len() <= budget {
            output.push_str(joint);
            output.push_str(entry);
            whole += 1;
            continue;
        }
        let mut lines = 0;
        for line in entry.split('\n') {
            let piece = (if lines == 0 { joint.len() } else { 1 }) + line.len();
            if output.len() + piece > budget && !output.is_empty() {
                break;
            }
            output.push_str(if lines == 0 { joint } else { "\n" });
            output.push_str(line);
            lines += 1;
        }
        if lines == entry.split('\n').count() {
            whole += 1;
        } else if lines > 0 {
            output.push_str("\n[…]");
            cut_short = true;
        }
        break;
    }
    let left = entries.len() - whole - usize::from(cut_short);
    if left > 0 || cut_short {
        let shortened = if cut_short {
            ", the last one shortened,"
        } else {
            ""
        };
        output.push_str(&format!(
            "\n\n({left} more memory entries left out{shortened} to keep this under about \
             {EVIDENCE_TOKENS} tokens; search again with more specific words to see others.)"
        ));
    }
    output.push_str(closing);
    output
}

/// `memory_list`'s result: `{items: [...], next_cursor?}`.
pub(crate) fn list(page: &ListPage) -> Value {
    paged("items", &page.items, page.next_cursor.as_deref())
}

/// `memory_get`'s result: `{items: [...], missing: [ids]}`.
pub(crate) fn get(found: &[Hit], missing: &[ItemId]) -> Value {
    json!({
        "items": found.iter().map(hit).collect::<Vec<_>>(),
        "missing": missing,
    })
}

/// `memory_explore`'s result: the facet, its buckets and the counts.
pub(crate) fn explore(page: &ExplorePage) -> Value {
    json!({
        "facet": page.facet.as_str(),
        "buckets": page.buckets,
        "total": page.total,
        "missing": page.missing,
        "more_buckets": page.more_buckets,
        "truncated": page.truncated,
    })
}

/// `memory_store`'s result: `{id, replayed}`.
pub(crate) fn store(receipt: &StoreReceipt) -> Value {
    json!({ "id": receipt.id, "replayed": receipt.replayed })
}

/// `memory_forget`'s result: the [`ForgetReport`] fields plus the ids that
/// were skipped because they named nothing in reach.
pub(crate) fn forget(report: &ForgetReport, skipped: &[ItemId]) -> Value {
    json!({ "forgotten": report.forgotten, "skipped": skipped })
}

/// One hit.
pub(crate) fn hit(hit: &Hit) -> Value {
    let mut out = Map::new();
    out.insert("id".to_string(), json!(hit.id));
    out.insert("kind".to_string(), json!(hit.kind.as_str()));
    out.insert("text".to_string(), json!(hit.text));
    out.insert("score".to_string(), score(hit.score));
    if let Some(confidence) = hit.confidence {
        out.insert("confidence".to_string(), score(confidence));
    }
    out.insert("meta".to_string(), meta(&hit.meta));
    Value::Object(out)
}

fn citation(citation: &Citation) -> Value {
    let mut out = Map::new();
    out.insert("id".to_string(), json!(citation.id));
    out.insert("kind".to_string(), json!(citation.kind.as_str()));
    out.insert("snippet".to_string(), json!(citation.snippet));
    if let Some(value) = citation.score {
        out.insert("score".to_string(), score(value));
    }
    out.insert("meta".to_string(), meta(&citation.meta));
    Value::Object(out)
}

fn paged(key: &str, hits: &[Hit], next_cursor: Option<&str>) -> Value {
    let mut out = Map::new();
    out.insert(
        key.to_string(),
        Value::Array(hits.iter().map(hit).collect()),
    );
    if let Some(cursor) = next_cursor {
        out.insert("next_cursor".to_string(), json!(cursor));
    }
    Value::Object(out)
}

/// The metadata subset a model sees.
fn meta(meta: &MemoryMeta) -> Value {
    let mut source = Map::new();
    source.insert("kind".to_string(), json!(meta.source.kind.as_str()));
    if let Some(id) = &meta.source.id {
        source.insert("id".to_string(), json!(id));
    }
    let mut out = Map::new();
    out.insert("source".to_string(), Value::Object(source));
    let optional = [
        ("file_path", &meta.file_path),
        ("url", &meta.url),
        ("thread_id", &meta.thread_id),
    ];
    for (key, value) in optional {
        if let Some(value) = value {
            out.insert(key.to_string(), json!(value));
        }
    }
    if !meta.tags.is_empty() {
        out.insert("tags".to_string(), json!(meta.tags));
    }
    if let Some(at) = meta.observed_at {
        out.insert(
            "observed_at".to_string(),
            json!(at.to_rfc3339_opts(SecondsFormat::Secs, true)),
        );
    }
    Value::Object(out)
}

/// A score rounded to four decimal places, so `f32` noise does not reach the
/// model (`0.1` rather than `0.10000000149011612`).
fn score(value: f32) -> Value {
    json!((f64::from(value) * 10_000.0).round() / 10_000.0)
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
