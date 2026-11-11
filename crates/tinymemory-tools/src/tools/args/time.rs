//! Reading a model's `refers_to`: the local days a question is about.

use chrono::NaiveDate;
use tinymemory_api::{Error, Result, TimeHint};

use super::Args;
use crate::tools::ToolScope;

/// The [`TimeHint`] at `key`, read in the scope's zone, or `None` when the
/// model passed none. `to` may be left out for a single day.
///
/// # Errors
///
/// [`tinymemory_api::Error::InvalidRequest`] naming the field, for a value
/// that is not an object, a date that is not `YYYY-MM-DD`, or `from > to`.
pub(crate) fn time_hint(args: &Args<'_>, key: &str, scope: &ToolScope) -> Result<Option<TimeHint>> {
    let Some(range) = args.object(key, "refers_to.", &["from", "to"])? else {
        return Ok(None);
    };
    let day = |field: &str| -> Result<Option<NaiveDate>> {
        range
            .string(field)?
            .map(|raw| {
                NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d")
                    .map_err(|_| range.field_error(field, "must be a date as YYYY-MM-DD"))
            })
            .transpose()
    };
    let Some(from) = day("from")? else {
        return Err(range.field_error("from", "is required"));
    };
    let to = day("to")?.unwrap_or(from);
    if to < from {
        return Err(range.field_error("to", "must not be before `from`"));
    }
    // The range is fine, so a refusal here is the host's zone, not the
    // model's dates: say so instead of sending the model after `to`.
    crate::ops::time_hint(
        Some(crate::ops::DateRange { from, to: Some(to) }),
        scope.zone.as_deref(),
    )
    .map_err(|_| {
        Error::Config(format!(
            "the host's time zone {:?} is not an IANA zone",
            scope.zone.as_deref().unwrap_or_default()
        ))
    })
}

#[cfg(test)]
#[path = "time_tests.rs"]
mod tests;
