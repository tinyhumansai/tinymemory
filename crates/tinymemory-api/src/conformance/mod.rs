//! The behavioural suite every TinyMemory engine must pass, and a reference
//! in-memory engine to calibrate it.
//!
//! [`run`] stores, lists, fetches, recalls and forgets through any
//! [`MemoryEngine`](crate::MemoryEngine) and reports the first behaviour that breaks
//! the contract. It covers store/list round trips for each item kind, replay
//! idempotency, fetch filtering by every metadata field in every declared
//! mode, forget by id and by filter, refusal of an empty forget,
//! `Unsupported` for undeclared modes, and recall citations that resolve
//! through `list`. It writes only under a workspace unique to the run and
//! forgets it afterwards, so it can run against an engine that holds data.
//!
//! [`run_isolation`] checks two engines that serve two users of one backing
//! store: neither user's list, get, fetch, recall, facet, forget or store may
//! reach the other's items. A host that offers more than one user on a shared
//! engine must pass it.
//!
//! [`ReferenceEngine`] is the calibration subject: obvious by inspection, so a
//! failure against it means the assertion is wrong, not the engine.
//!
//! # Example
//!
//! ```
//! use tinymemory_api::conformance::{ReferenceEngine, run};
//!
//! # let runtime = tokio::runtime::Builder::new_current_thread().build()?;
//! # runtime.block_on(async {
//! let engine = ReferenceEngine::new();
//! run(&engine).await?;
//! assert!(engine.is_empty(), "the suite cleans up after itself");
//! # Ok::<(), tinymemory_api::conformance::Error>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod error;
pub mod reference;
mod suite;

pub use error::{Error, Result};
pub use reference::{CONSOLIDATED_TAG, REFERENCE_ENGINE_ID, ReferenceEngine};
pub use suite::{run, run_isolation};
