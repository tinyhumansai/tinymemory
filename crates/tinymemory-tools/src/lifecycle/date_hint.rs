//! Runtime-neutral date hints for turns whose text refers to a calendar date.

use chrono::NaiveDate;
use futures::future::BoxFuture;
use tinymemory_api::TimeHint;

use crate::ops::{DateRange, time_hint};

/// Prompt instructions for the model that extracts a local date range.
pub const INSTRUCTION: &str = "Return the calendar date range (local dates) that the user's message refers to, as JSON only: {\"from\":\"YYYY-MM-DD\",\"to\":\"YYYY-MM-DD\"}, or {\"from\":null,\"to\":null} when it refers to no date. Resolve relative words (yesterday, last week, a weekday, a month) against the current date. A single day has from == to. If a word can mean either the past or the future (e.g. Hindi/Hinglish 'kal', 'parso'), return a window covering both readings.";

/// A host callback that asks its configured model for a raw date-range answer.
///
/// The host supplies current-date context in `instruction` using `zone`; the
/// library owns the shared instruction and parses the returned JSON.
pub trait DateHintModel: Send + Sync {
    /// Returns the model's raw answer, or `None` when the call failed.
    fn complete(
        &self,
        instruction: String,
        user_text: String,
        zone: String,
    ) -> BoxFuture<'static, Option<String>>;
}

/// Parses a model answer into the engine's date-ranking hint.
///
/// Prose and code fences around the first JSON object are tolerated. Invalid,
/// reversed, or timezone-invalid ranges return `None`.
#[must_use]
pub fn parse_range(answer: &str, zone: Option<&str>) -> Option<TimeHint> {
    #[derive(serde::Deserialize)]
    struct Range {
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
    }

    let json = answer.get(answer.find('{')?..=answer.rfind('}')?)?;
    let range: Range = serde_json::from_str(json).ok()?;
    let from = range.from?;
    time_hint(Some(DateRange { from, to: range.to }), zone)
        .ok()
        .flatten()
}
