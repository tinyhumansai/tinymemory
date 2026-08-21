//! Email thread → markdown, shared shape with the engine's canonicaliser
//! (#18 §B1).
//!
//! The gmail pipeline stores one markdown document per message; the engine's
//! ingest path canonicalises full threads with the same header block and
//! body-cleaning rules. The exact output format is load-bearing twice over:
//! the chunker splits at `---\nFrom:` boundaries, and the engine writes the
//! same shape from its copy — `thread_markdown_format_is_pinned` holds the
//! two to one form.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use crate::email_clean;

/// One message of a thread, in the canonicaliser's input shape.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmailMessage {
    /// Sender, as the provider renders it.
    pub from: String,
    /// Direct recipients.
    #[serde(default)]
    pub to: Vec<String>,
    /// Carbon-copy recipients.
    #[serde(default)]
    pub cc: Vec<String>,
    /// Subject line.
    pub subject: String,
    #[serde(
        default = "chrono_now",
        serialize_with = "chrono::serde::ts_milliseconds::serialize",
        deserialize_with = "deserialize_flexible_timestamp"
    )]
    /// When the message was sent. Accepts epoch-ms or RFC 3339 on the wire.
    pub sent_at: DateTime<Utc>,
    /// Body text, best rendering the provider offers.
    pub body: String,
    /// Opaque pointer back to the raw source record.
    #[serde(default)]
    pub source_ref: Option<String>,
    /// `List-Unsubscribe` header, when present.
    #[serde(default)]
    pub list_unsubscribe: Option<String>,
}

/// A thread of messages from one provider.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EmailThread {
    /// Provider slug, e.g. `gmail`.
    pub provider: String,
    /// The thread's subject.
    pub thread_subject: String,
    /// Messages, any order; rendering sorts oldest-first.
    pub messages: Vec<EmailMessage>,
}

fn chrono_now() -> DateTime<Utc> {
    Utc::now()
}

/// The thread as canonical markdown: one `---\nFrom:` block per message,
/// oldest first, bodies through [`email_clean::clean_body`]. `None` for an
/// empty thread.
pub fn thread_markdown(thread: EmailThread) -> Option<String> {
    if thread.messages.is_empty() {
        return None;
    }
    let mut messages = thread.messages;
    messages.sort_by_key(|m| m.sent_at);

    let mut md = String::new();
    // No leading `# Email thread — ...` header. Provider / subject info belongs
    // in the MD front-matter. The chunker splits this output at `---\nFrom:`
    // boundaries so each message becomes one chunk.
    for msg in &messages {
        md.push_str("---\n");
        md.push_str(&format!("From: {}\n", email_clean::md_escape(&msg.from)));
        if !msg.to.is_empty() {
            md.push_str(&format!(
                "To: {}\n",
                email_clean::md_escape(&msg.to.join(", "))
            ));
        }
        if !msg.cc.is_empty() {
            md.push_str(&format!(
                "Cc: {}\n",
                email_clean::md_escape(&msg.cc.join(", "))
            ));
        }
        md.push_str(&format!(
            "Subject: {}\n",
            email_clean::md_escape(&msg.subject)
        ));
        md.push_str(&format!("Date: {}\n", msg.sent_at.to_rfc3339()));

        if let Some(unsub) = &msg.list_unsubscribe {
            md.push_str(&format!(
                "List-Unsubscribe: {}\n",
                email_clean::md_escape(unsub)
            ));
        }
        md.push('\n');
        let cleaned = email_clean::clean_body(msg.body.trim());
        if cleaned.is_empty() {
            md.push('\n');
        } else {
            let safe_body = cleaned
                .lines()
                .map(|line| {
                    if line.trim_end() == "---" {
                        format!("\\{line}")
                    } else {
                        line.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            md.push_str(&safe_body);
        }
        md.push_str("\n\n");
    }
    Some(md)
}

/// Deserialise a `DateTime<Utc>` from either:
/// - a JSON integer = epoch **milliseconds** (legacy callers — back-compat),
/// - a JSON string = RFC 3339 / ISO-8601 (e.g. `"2026-05-17T19:30:00Z"`), or
///   a decimal string containing epoch milliseconds.
///
/// On an unparseable string a serde error is returned (no silent default).
/// Shared across chat, email, and document canonicalisers.
///
fn deserialize_flexible_timestamp<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawTs {
        Millis(i64),
        Text(String),
        Null,
    }

    fn epoch_millis<E: serde::de::Error>(ms: i64) -> Result<DateTime<Utc>, E> {
        // Contemporary epoch seconds are ten digits while epoch milliseconds
        // are thirteen. Reject the ambiguous near-epoch range so a seconds
        // value cannot silently poison ordering and staleness calculations.
        const MIN_PLAUSIBLE_EPOCH_MILLIS: u64 = 100_000_000_000;
        if ms.unsigned_abs() < MIN_PLAUSIBLE_EPOCH_MILLIS {
            return Err(E::custom(format!(
                "epoch-ms value {ms} is too small; pass milliseconds, not seconds"
            )));
        }
        chrono::TimeZone::timestamp_millis_opt(&Utc, ms)
            .single()
            .ok_or_else(|| E::custom(format!("invalid epoch-ms: {ms}")))
    }

    let raw = RawTs::deserialize(deserializer)?;
    match raw {
        RawTs::Null => Ok(Utc::now()),
        RawTs::Millis(ms) => epoch_millis(ms),
        RawTs::Text(s) => {
            if let Ok(dt) = DateTime::parse_from_rfc3339(&s) {
                return Ok(dt.with_timezone(&Utc));
            }
            if let Ok(ms) = s.parse::<i64>() {
                return epoch_millis(ms);
            }
            Err(serde::de::Error::custom(format!(
                "cannot parse '{s}' as RFC 3339 or epoch-ms"
            )))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "email_markdown_tests.rs"]
mod tests;
