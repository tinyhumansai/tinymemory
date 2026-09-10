//! Driver admission: deciding, from configuration alone, which memory driver a
//! host may bind and what class it binds as.
//!
//! ## Why this lives in the crate and not in the host
//!
//! Admission is the one part of binding that is genuinely engine-neutral. It
//! answers "is this driver id real, and is it allowed to answer for memory" —
//! a question with the same correct answer for every host that embeds this
//! contract. The host keeps everything downstream of the decision: constructing
//! the provider, caching it per workspace, wrapping it in a policy guard, and
//! converting [`DriverClass`] into whatever generic subsystem vocabulary the
//! host uses for its other subsystems.
//!
//! ## The two rules worth stating out loud
//!
//! **A built-in id's class is fixed.** [`DriverRegistry::builtin`] reserves
//! `null` as [`DriverClass::Null`] and `tinycortex` as
//! [`DriverClass::Embedded`], and an explicit `class` line may *confirm* a
//! reserved id's class but never override it. Without that rule, a config
//! naming `driver = "null"` with `class = "embedded"` would build the real
//! engine, advertise every family, and persist memory under the id documented
//! as `/dev/null`; the inverse would label a store-nothing provider
//! `tinycortex`. Either way the bound engine is mislabelled.
//!
//! **An unknown id is refused, not guessed.** A driver needs no per-driver
//! config entry — the embedded default's options live elsewhere — but only a
//! reserved id is admitted implicitly. Anything else is a typo, or an external
//! backend that forgot its entry, and admitting it would silently run the
//! default engine under an invented driver id.
//!
//! ## Refusal is not failure
//!
//! [`DriverRegistry::admit`] returns [`FallbackReason`] rather than an error
//! type, because the caller is expected to *fall back and stay bound*, loudly,
//! rather than leave the memory slot empty. The reason string is operator-facing
//! — logged, published, rendered in status — so it must never interpolate a
//! credential reference or an endpoint from the driver's config entry. Callers
//! pass only [`DriverEntry`], which carries neither.

use std::collections::BTreeMap;
use std::fmt;

use tinymemory_api::host::MemoryHostConfig;

mod class;

pub use class::{DriverClass, DriverClassParseError};

#[cfg(test)]
#[path = "test.rs"]
mod test;

/// The driver id reserved for the null placeholder, re-exported from the
/// contract so hosts and adapters agree on the spelling.
pub use tinymemory_api::null::NULL_DRIVER_ID;

/// The reserved driver ids, re-exported from the contract crate.
///
/// They moved to [`tinymemory_api::drivers`] so an adapter can spell the id it
/// binds under without depending on this crate — that dependency is what made
/// this crate unable to depend on the adapters in turn, and so blocked #18
/// §D1's per-engine features. Re-exported here because
/// `tinymemory::registry::TINYCORTEX_DRIVER_ID` is a path both the adapters and
/// downstream hosts already use.
///
/// `NAMESPACE_DRIVER_ID` moved with them rather than staying a `const` here:
/// it is a driver id like the other four, and splitting the set across two
/// crates would mean the next one lands in whichever place its author happened
/// to be reading.
pub use tinymemory_api::drivers::{
    AGENTMEMORY_DRIVER_ID, COGNEE_DRIVER_ID, CORTEX_DRIVER_ID, MEM0_DRIVER_ID, NAMESPACE_DRIVER_ID,
    SUPERMEMORY_DRIVER_ID, TINYCORTEX_DRIVER_ID,
};

/// The trust state a driver entry must carry for an external class to bind.
pub const TRUSTED: &str = "trusted";

/// Why a bind fell back to the placeholder driver.
///
/// `reason` is operator-facing: it is logged, published, and rendered in status.
/// It is built only from the driver id and the shape of the configuration, never
/// from a credential reference or an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackReason {
    /// The driver id that was asked for.
    pub configured_driver: String,
    /// Why it was refused.
    pub reason: String,
}

/// A refusal is an error, so `?` can propagate it.
///
/// It carried `Display` from the start but not this, which meant a host writing
/// the obvious `registry.admit(..)?` in a function returning `Box<dyn Error>` or
/// `anyhow::Error` got a type error instead. Nothing about the type changes —
/// this is the trait that makes the existing message usable where refusals
/// actually travel. Found by writing `examples/basic.rs` (issue #18 §E7), which
/// is the argument for having a compiled example at all.
impl std::error::Error for FallbackReason {}

impl fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "driver '{}' refused: {}",
            self.configured_driver, self.reason
        )
    }
}

/// A driver that was admitted, and the class it binds as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    /// The id that bound. Equal to the configured id — admission never renames
    /// a driver; a fallback is signalled by returning [`FallbackReason`].
    pub id: String,
    /// How the driver is reached.
    pub class: DriverClass,
}

/// A host's per-driver configuration entry, reduced to the two fields admission
/// actually reads.
///
/// Deliberately borrowed and deliberately narrow: a host's real entry type also
/// carries an endpoint and a credential reference, and neither may reach a
/// refusal message. Passing a projection rather than the whole entry makes that
/// structural instead of a rule someone has to remember.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriverEntry<'a> {
    /// The `class` line, if the entry has one.
    pub class: Option<&'a str>,
    /// The entry's trust state. Only consulted for an external class.
    pub trust_state: &'a str,
}

/// The configuration paths quoted back to the operator in refusal messages.
///
/// The crate does not know what a host's config file looks like, but a refusal
/// that cannot name the block to edit is much less useful. The host supplies
/// its own spellings; the wording around them is fixed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigLabels<'a> {
    /// The memory subsystem block, e.g. `[subsystems.memory]`.
    pub section: &'a str,
    /// The driver table, e.g. `[subsystems.memory.drivers]`.
    pub drivers: &'a str,
    /// One driver's entry, e.g. `[subsystems.memory.drivers.<id>]`.
    pub driver_entry: &'a str,
}

impl Default for ConfigLabels<'static> {
    fn default() -> Self {
        Self {
            section: "[subsystems.memory]",
            drivers: "[subsystems.memory.drivers]",
            driver_entry: "[subsystems.memory.drivers.<id>]",
        }
    }
}

/// The set of driver ids whose class is fixed by the crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverRegistry {
    reserved: BTreeMap<String, DriverClass>,
}

impl Default for DriverRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

impl DriverRegistry {
    /// The registry every host starts from: the null placeholder, the two
    /// embedded engines — TinyCortex and this workspace's own `namespace`
    /// store — and the supported native HTTP engines.
    #[must_use]
    pub fn builtin() -> Self {
        let mut reserved = BTreeMap::new();
        reserved.insert(NULL_DRIVER_ID.to_string(), DriverClass::Null);
        reserved.insert(TINYCORTEX_DRIVER_ID.to_string(), DriverClass::Embedded);
        reserved.insert(NAMESPACE_DRIVER_ID.to_string(), DriverClass::Embedded);
        reserved.insert(SUPERMEMORY_DRIVER_ID.to_string(), DriverClass::External);
        reserved.insert(MEM0_DRIVER_ID.to_string(), DriverClass::External);
        reserved.insert(COGNEE_DRIVER_ID.to_string(), DriverClass::External);
        reserved.insert(CORTEX_DRIVER_ID.to_string(), DriverClass::External);
        reserved.insert(AGENTMEMORY_DRIVER_ID.to_string(), DriverClass::External);
        Self { reserved }
    }

    /// A registry reserving nothing. Every id then needs an explicit `class`.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            reserved: BTreeMap::new(),
        }
    }

    /// Reserve an additional driver id at a fixed class.
    ///
    /// For a host that bundles an adapter this crate does not know about. The
    /// same confirm-never-override rule then applies to it.
    #[must_use]
    pub fn with_reserved(mut self, id: impl Into<String>, class: DriverClass) -> Self {
        self.reserved.entry(id.into()).or_insert(class);
        self
    }

    /// The class `id` is fixed to, or `None` if the id is not reserved.
    #[must_use]
    pub fn reserved_class(&self, id: &str) -> Option<DriverClass> {
        self.reserved.get(id).copied()
    }

    /// Decide whether the configured driver may bind.
    ///
    /// Pure — no I/O, no globals — so the fail-closed trust rule is testable
    /// without booting anything.
    ///
    /// `entry` is the host's `drivers.<id>` entry, or `None` when the config has
    /// no entry for this id.
    ///
    /// # Errors
    ///
    /// Returns the [`FallbackReason`] to record and publish when the configured
    /// driver is refused. Callers are expected to fall back rather than fail:
    /// the subsystem must stay bound, loudly.
    pub fn admit(
        &self,
        driver: &str,
        entry: Option<DriverEntry<'_>>,
        labels: ConfigLabels<'_>,
    ) -> Result<Admission, FallbackReason> {
        let id = driver.trim();
        if id.is_empty() {
            return Err(FallbackReason {
                configured_driver: String::new(),
                reason: format!("{} driver is empty", labels.section),
            });
        }

        let refuse = |reason: &str| FallbackReason {
            configured_driver: id.to_string(),
            reason: reason.to_string(),
        };

        // A driver needs no entry: the embedded default's options live in the
        // host's own config blocks. But only a reserved id is admitted
        // implicitly — see the module docs.
        let Some(entry) = entry else {
            if self.reserved_class(id) == Some(DriverClass::External) {
                return Err(refuse(&format!(
                    "no {} entry; external drivers require endpoint, credential, and trust configuration",
                    labels.driver_entry
                )));
            }
            return self.implicit(id, &refuse, &format!("no {} entry", labels.driver_entry));
        };

        let admission = match entry.class {
            None => self.implicit(
                id,
                &refuse,
                &format!("{} has no class line", labels.driver_entry),
            )?,
            Some(raw) => {
                // Render the parse error rather than discarding it: it carries the
                // offending value, and a config typo is the only way to get
                // here, so naming it is the difference between a refusal an
                // operator can act on and one they cannot.
                let class = DriverClass::parse(raw).map_err(|error| refuse(&error.to_string()))?;
                // A reserved id names a fixed implementation, so an explicit
                // `class` line may confirm it but never override it.
                if let Some(fixed) = self.reserved_class(id) {
                    if class != fixed {
                        return Err(refuse(&format!(
                            "driver id \"{id}\" is built in and is always class \
                             \"{}\"; remove the conflicting class = \"{raw}\" line",
                            fixed.as_str()
                        )));
                    }
                }
                Admission {
                    id: id.to_string(),
                    class,
                }
            }
        };

        if admission.class == DriverClass::External {
            // Fail closed: trust must be explicitly raised before an
            // out-of-process driver is allowed to answer for memory.
            if entry.trust_state != TRUSTED {
                return Err(refuse(&format!(
                    "external driver is untrusted: set trust_state = \"{TRUSTED}\" \
                     under {} to allow this binding",
                    labels.drivers
                )));
            }
        }

        Ok(admission)
    }

    /// Selects and admits the memory driver this host's configuration names.
    ///
    /// The half of driver binding that was specified but never wired: the
    /// registry could answer "is this driver id real and allowed", and nothing
    /// asked it. This reads the id from the host's own configuration and puts
    /// it through [`Self::admit`], so configuration decides the engine instead
    /// of a factory hardcoding one (issue #18 §A5).
    ///
    /// It reads [`MemoryHostConfig::memory_driver`], **not**
    /// `memory_provider` — despite the name, the latter is a `provider:model`
    /// routing string choosing which language model does summarisation, which
    /// is a different axis from which store the memory lives in.
    ///
    /// A configuration that names no driver gets [`TINYCORTEX_DRIVER_ID`], the
    /// reserved embedded default. That is what keeps an unconfigured host
    /// booting: an embedded id is admitted without a `drivers` entry, while an
    /// external one is refused without endpoint, credential and trust
    /// configuration.
    ///
    /// This resolves the *decision*, not the instance. Constructing the
    /// provider, caching it per workspace, and wrapping it in a policy guard
    /// stay with the host — see the module docs for why.
    ///
    /// # Errors
    ///
    /// Returns the [`FallbackReason`] to record and publish when the configured
    /// driver is refused, exactly as [`Self::admit`] does.
    pub fn select(
        &self,
        config: &dyn MemoryHostConfig,
        entry: Option<DriverEntry<'_>>,
        labels: ConfigLabels<'_>,
    ) -> Result<Admission, FallbackReason> {
        let driver = config.memory_driver().unwrap_or(TINYCORTEX_DRIVER_ID);
        self.admit(driver, entry, labels)
    }

    /// The class an id implies when nothing says otherwise.
    ///
    /// `context` names which part of the config was missing; the refusal echoes
    /// it so the operator knows whether to add an entry or a `class` line.
    fn implicit(
        &self,
        id: &str,
        refuse: &impl Fn(&str) -> FallbackReason,
        context: &str,
    ) -> Result<Admission, FallbackReason> {
        match self.reserved_class(id) {
            Some(class) => Ok(Admission {
                id: id.to_string(),
                class,
            }),
            None => Err(refuse(&format!(
                "unknown driver id \"{id}\": {context}, and the id is neither the \
                 embedded default nor \"null\""
            ))),
        }
    }
}
