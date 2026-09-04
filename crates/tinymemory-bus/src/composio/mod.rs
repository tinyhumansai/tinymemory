//! The Composio sync vocabulary: what a provider run produces, what it
//! remembers between runs, and what a user is willing to let it do.
//!
//! Composio is the connector layer the memory stack syncs through — Gmail,
//! Slack, Notion, GitHub, Linear, ClickUp and the catalog-only toolkits behind
//! them. Each toolkit's *provider* fetches a profile, pulls items, normalises
//! tasks and reports what it did. This module owns the shapes those runs
//! exchange; the providers themselves, the HTTP client, the registry and the
//! persistence live in the engine crate, where their dependencies belong.
//!
//! # Why the vocabulary is here and the providers are not
//!
//! The split is the same one [`crate::goals`] makes, and for the same reason.
//! A host reads these shapes: it renders a [`runs::SyncOutcome`] in the sync
//! status panel, files a [`tasks::NormalizedTask`] onto the agent's todo board,
//! gates a tool call on a [`scopes::UserScopePref`], and reports a
//! [`state::SyncState`]'s remaining daily budget. It does not run a provider —
//! the module does that, behind the bus. So the *values* have to be nameable
//! from the contract the host already links, while the code that produces them
//! must not be: a provider needs `reqwest`, an async runtime and the chunk
//! store, and none of those may enter this crate.
//!
//! The alternative — a parallel set of host-side structs — is the failure this
//! whole crate exists to prevent. A `SyncOutcome` decoded from the module would
//! not be *the* `SyncOutcome`, and every field added on one side would be a
//! silent decode gap on the other with nothing to catch it. One definition,
//! here, at the bottom.
//!
//! # What stayed in the engine crate, and why
//!
//! Read this before concluding something is missing:
//!
//! - **`ProviderContext`** — holds an `Arc<Config>` and dispatches Composio
//!   actions through the host seam. Behaviour, not a payload.
//! - **`SyncStateStore`** and [`state::SyncState`]'s `load` / `save` — a
//!   key/value I/O seam and its two async methods. This crate publishes no
//!   traits (see [`crate`]); the engine crate carries the trait and offers the
//!   two methods as an extension trait over the type defined here.
//! - **`user_scopes::{load, save}`** — the same, one namespace over: the
//!   preference *shape* is here, reading and writing it is not.
//! - **`profile::{persist_provider_profile, load_connected_identities,
//!   delete_connected_identity_facets, is_self_identity}`** — every one of
//!   them reaches the profile facet store.
//! - **`profile_md`** — rewrites managed blocks in the host's `PROFILE.md`.
//!   Filesystem mutation against a host-owned file; it is host policy that
//!   happens to be written in the memory stack, not a wire type.
//! - **the provider registry** — a process-global `HashMap` of trait objects
//!   that reach `reqwest` and the chunk store.
//!
//! The curated catalogs were on that list and are **not** any more: they are
//! several thousand `&'static str` action slugs with no dependency at all, and
//! the host is their heaviest reader. See [`catalogs`] for why they moved.

pub mod catalogs;
pub mod profile;
pub mod runs;
pub mod scopes;
pub mod state;
pub mod tasks;

pub use profile::{
    canonicalize, normalize_connection_identifier, render_connected_identities_section,
    ConnectedIdentity, IdentityKind, ProviderUserProfile,
};
pub use runs::{ComposioUsage, ComposioUsageHandle, SyncOutcome, SyncReason};
pub use scopes::{
    agent_ready_toolkits, classify_unknown, find_curated, toolkit_from_slug, CuratedTool,
    ToolScope, UserScopePref,
};
pub use state::{
    extract_item_id, DailyBudget, SyncState, DEFAULT_DAILY_REQUEST_LIMIT, KV_NAMESPACE,
    STATE_NAMESPACE,
};
pub use tasks::{GithubFetchMode, NormalizedTask, TaskContainer, TaskFetchFilter, TaskKind};

pub use catalogs::{
    catalog_for_toolkit, curated_scope_for, has_native_provider, is_action_visible_with_pref,
    native_provider_sync_interval_secs, parse_sync_interval_override, sync_interval_env_var,
    toolkit_description, toolkit_has_scope, toolkit_result_notes, CAPABILITY_TOOLKITS,
    NATIVE_PROVIDERS,
};
