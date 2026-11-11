//! Scrubbing a [`StoreItem`] before it reaches an engine.
//!
//! Every free text an item carries is run through [`sanitize_text_with`]: a
//! document's title and text body, each conversation turn, a learning's
//! statement and evidence, and the metadata URL (query strings carry tokens).
//! Identifiers in the metadata — paths, repository, commit, thread and agent
//! ids — are left alone: they are what filters match on, and rewriting them
//! would make an item unfindable. A [`DocumentBody::Uri`] is left alone too;
//! sources resolve it to text before store, and that text is scrubbed then.

use tinymemory_api::{DocumentBody, StoreItem};

use tinyredact::{Policy, SanitizationReport, Sanitized, sanitize_text_with};

/// Scrubs every text `item` carries under the default (strictest) [`Policy`].
#[must_use]
pub fn scrub_item(item: StoreItem) -> Sanitized<StoreItem> {
    scrub_item_with(item, Policy::default())
}

/// Scrubs every text `item` carries under `policy`.
#[must_use]
pub fn scrub_item_with(mut item: StoreItem, policy: Policy) -> Sanitized<StoreItem> {
    let mut report = SanitizationReport::default();
    let mut clean = |text: &mut String| {
        let scrubbed = sanitize_text_with(text, policy);
        report = report.merge(scrubbed.report);
        *text = scrubbed.value;
    };
    match &mut item {
        StoreItem::Document { title, body, .. } => {
            if let Some(title) = title {
                clean(title);
            }
            if let DocumentBody::Text(text) = body {
                clean(text);
            }
        }
        StoreItem::Conversation { turns, .. } => {
            for turn in turns {
                clean(&mut turn.text);
            }
        }
        StoreItem::Learning { text, evidence, .. } => {
            clean(text);
            if let Some(evidence) = evidence {
                clean(evidence);
            }
        }
    }
    if let Some(url) = &mut item.meta_mut().url {
        clean(url);
    }
    Sanitized {
        value: item,
        report,
    }
}

#[cfg(test)]
#[path = "item_tests.rs"]
mod tests;
