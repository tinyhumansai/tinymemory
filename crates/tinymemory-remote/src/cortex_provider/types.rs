//! Private CortexDB experience request inputs.

use serde_json::Value;
use tinymemory_api::types::MemoryTaint;

pub(super) struct ExperienceInput<'a> {
    pub(super) namespace: &'a str,
    pub(super) modality: &'a str,
    pub(super) role: Option<&'a str>,
    pub(super) key: &'a str,
    pub(super) body: &'a str,
    pub(super) session_id: Option<&'a str>,
    pub(super) taint: MemoryTaint,
    pub(super) payload: Value,
    pub(super) idempotency_seed: &'a str,
    pub(super) observed_at: Option<String>,
    pub(super) labels: Vec<String>,
}
