//! `[subsystems.*]` config section — the uniform cross-subsystem driver-binding
//! shape defined in `docs/specs/kernel.md` §3.6 and `docs/specs/plan-memory.md` §4.5.
//!
//! GREENFIELD / ZERO BEHAVIOUR CHANGE: nothing reads this config yet. It exists
//! so `[subsystems.memory]` can be authored today and so `inference`,
//! `channels`, `sandbox`, … can slot in later as sibling fields on
//! [`SubsystemsConfig`] without reshaping this type.
//!
//! Shape (kernel.md §3.6 / plan-memory.md §4.5):
//!
//! ```toml
//! [subsystems.memory]
//! driver = "tinycortex"
//!
//! [subsystems.memory.hooks]
//! auto_recall = true
//! auto_capture = true
//! max_context_tokens = 2000
//! recall_max_chars = 1000
//! capture_max_chars = 500
//!
//! [subsystems.memory.drivers.supermemory]
//! class = "external"
//! transport = "http"
//! endpoint = "https://api.supermemory.ai"
//! credential_ref = "keychain:supermemory"
//! trust_state = "untrusted"
//! ```

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Top-level `[subsystems]` config block. Currently carries only `memory`;
/// future subsystems (`inference`, `channels`, `sandbox`, …) are added here
/// as sibling fields — see kernel.md §3.6.
#[derive(Debug, Clone, Serialize, Deserialize, Default, JsonSchema)]
#[serde(default)]
pub struct SubsystemsConfig {
    #[serde(default)]
    pub memory: MemorySubsystemConfig,
}

/// `[subsystems.memory]` — which driver is bound for the memory subsystem,
/// its hook budgets, and the per-driver option table.
///
/// `PartialEq`/`Eq` let `CoreContext::rebind_workspace` short-circuit a
/// no-op rebind by comparing the config it was handed against the one already
/// held — equality is value comparison only, so it never prints or leaks the
/// credential fields the way `Debug` would. `Hash` lets `binding`
/// key its per-workspace cache on the whole config, so a changed driver/hooks/
/// trust for an already-bound workspace yields a fresh binding rather than a
/// stale cache hit.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MemorySubsystemConfig {
    /// The bound driver id (e.g. `"tinycortex"`, `"supermemory"`, `"null"`).
    /// Must match a key under `drivers` when that driver needs options.
    #[serde(default = "default_memory_driver")]
    pub driver: String,

    #[serde(default)]
    pub hooks: MemoryHooksConfig,

    /// Per-driver option tables, keyed by driver id. The embedded default
    /// (`tinycortex`) needs no entry here — its options continue to live in
    /// the existing `[memory]` / `[memory_tree]` / `[[memory_sources]]`
    /// blocks (plan-memory.md §4.5: "no user-visible config break").
    #[serde(default)]
    pub drivers: BTreeMap<String, MemoryDriverConfig>,
}

fn default_memory_driver() -> String {
    "tinycortex".into()
}

impl Default for MemorySubsystemConfig {
    fn default() -> Self {
        Self {
            driver: default_memory_driver(),
            hooks: MemoryHooksConfig::default(),
            drivers: BTreeMap::new(),
        }
    }
}

/// Memory-hook budgets — the auto-recall / auto-capture behavior gating
/// values. Defaults reproduce today's (pre-`[subsystems]`) behavior exactly;
/// nothing reads these yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MemoryHooksConfig {
    #[serde(default = "default_true")]
    pub auto_recall: bool,
    #[serde(default = "default_true")]
    pub auto_capture: bool,
    #[serde(default = "default_max_context_tokens")]
    pub max_context_tokens: usize,
    #[serde(default = "default_recall_max_chars")]
    pub recall_max_chars: usize,
    #[serde(default = "default_capture_max_chars")]
    pub capture_max_chars: usize,
}

fn default_true() -> bool {
    true
}
fn default_max_context_tokens() -> usize {
    2000
}
fn default_recall_max_chars() -> usize {
    1000
}
fn default_capture_max_chars() -> usize {
    500
}

impl Default for MemoryHooksConfig {
    fn default() -> Self {
        Self {
            auto_recall: default_true(),
            auto_capture: default_true(),
            max_context_tokens: default_max_context_tokens(),
            recall_max_chars: default_recall_max_chars(),
            capture_max_chars: default_capture_max_chars(),
        }
    }
}

/// One entry under `[subsystems.memory.drivers.<id>]`. Describes an
/// external/embedded driver binding — class, transport, endpoint, and a
/// *reference* to a credential resolved via the keychain (never an inline
/// secret; plan-memory.md §4.5, kernel.md §3.6).
///
/// `trust_state` is fail-closed `"untrusted"` per kernel.md §3.4: an external
/// driver must have its trust explicitly raised before bind succeeds.
///
/// MUST NOT derive `Debug` — see the manual impl below. `credential_ref` is a
/// secret handle and plan-memory.md §7 Tier-3 conformance requires "credential never
/// in `Debug`/error output", mirroring `storage_memory::MemoryConfig`'s
/// manual redacting `Debug` impl for `agentmemory_secret`.
///
/// `PartialEq`/`Eq` are safe to derive: they compare values for equality and
/// never render them, so `credential_ref` stays out of any output.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct MemoryDriverConfig {
    /// Driver class: `"embedded"` | `"external"` | `"null"`. See kernel.md §3.1.
    #[serde(default)]
    pub class: Option<String>,

    /// Wire transport for external drivers, e.g. `"http"`. See plan-memory.md §4.2.
    #[serde(default)]
    pub transport: Option<String>,

    /// Base endpoint URL for external/http drivers.
    #[serde(default)]
    pub endpoint: Option<String>,

    /// A *reference* to a credential (e.g. `"keychain:supermemory"`),
    /// resolved kernel-side through the existing keychain — never an inline
    /// secret. Redacted in `Debug`/error output; see the manual `Debug` impl.
    #[serde(default)]
    pub credential_ref: Option<String>,

    /// Fail-closed trust state for this driver binding. Defaults to
    /// `"untrusted"`; must be explicitly raised before an external driver's
    /// bind succeeds (kernel.md §3.4).
    #[serde(default = "default_trust_state")]
    pub trust_state: String,
}

fn default_trust_state() -> String {
    "untrusted".into()
}

impl Default for MemoryDriverConfig {
    fn default() -> Self {
        Self {
            class: None,
            transport: None,
            endpoint: None,
            credential_ref: None,
            trust_state: default_trust_state(),
        }
    }
}

// Manual `Debug` implementation that redacts `credential_ref`. Without this,
// any `format!("{cfg:?}")` / `tracing::debug!(?cfg, ...)` / panic message
// capturing a `MemoryDriverConfig` would dump the credential reference
// verbatim. The value itself (e.g. `"keychain:supermemory"`) is only a
// *reference*, not the secret — but plan-memory.md §7 Tier-3 conformance requires it
// never appear in Debug/error output regardless, so this mirrors
// `MemoryConfig`'s `agentmemory_secret` treatment exactly. NEVER derive
// `Debug` on this struct.
impl std::fmt::Debug for MemoryDriverConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryDriverConfig")
            .field("class", &self.class)
            .field("transport", &self.transport)
            .field("endpoint", &self.endpoint)
            .field(
                "credential_ref",
                &self.credential_ref.as_ref().map(|_| "<redacted>"),
            )
            .field("trust_state", &self.trust_state)
            .finish()
    }
}

#[cfg(test)]
#[path = "subsystems_tests.rs"]
mod tests;
