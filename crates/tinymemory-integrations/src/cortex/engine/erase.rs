//! Erase: every registered kind scope in reach, deepest first.
//!
//! Only kind scopes are erased, never a node's own path: CortexDB erases a
//! scope's events for good but only redacts the scopes below it, which keep
//! their write keys for 24 hours (a re-sent write then replays and stores
//! nothing; measured on 0.10.5). A kind scope is a leaf (`…/agent:a/app:learnings`
//! and `…/agent:a/agent:b/app:learnings` are siblings), so erasing them
//! deletes every event and releases every key. They are still taken deepest
//! first, so a layout that ever put a scope below another would not strand
//! redacted events behind held keys.
//!
//! On the TinyHumans wire the whole tree (`whole_tree`: the root, its
//! descendants, every kind) erases in one `DELETE memory` that erases the
//! caller's entire hosted memory, every scope under its tenant (including any
//! another layout or client wrote there). A narrower request erases scope by
//! scope, as Direct does, through the backend's `memory/v1/erasures`
//! passthrough, which memory-api pins under the tenant's root (see
//! `log::erase`). A backend without that route answers `Unsupported`, so a
//! caller can fall back to `forget`.
//!
//! On the proxied wire the tenant is the call's `user:<id>` node, so an
//! erasure of that node with its descendants and every kind is the tenant's
//! whole memory: it purges the tenant in one `POST v1/tenant/purge`, which
//! also takes anything else stored under the tenant's root (blobs included).
//! A narrower request erases scope by scope by forgetting each scope's
//! events (see `log::erase`); the proxy exposes no `v1/erasures`.

use tinymemory_api::{EraseReport, EraseRequest, ItemKind};

use super::CortexEngine;
use crate::cortex::descriptor::CortexWire;
use crate::cortex::error::Result;

impl CortexEngine {
    /// See the module docs.
    pub(super) async fn erase_scopes(&self, req: EraseRequest) -> Result<EraseReport> {
        req.validate()?;
        let wire = self.log.client.wire();
        let everything = match wire {
            CortexWire::TinyHumans => is_whole_tree(&req),
            CortexWire::Proxied => is_whole_tenant(&req),
            CortexWire::Direct => false,
        };
        if everything {
            let erased_scopes = self.log.erase_all().await?;
            log::debug!("[cortex] erased the whole hosted memory ({erased_scopes} scopes)");
            return Ok(EraseReport {
                erased_scopes,
                receipts: Vec::new(),
            });
        }
        let kinds: Vec<ItemKind> = if req.kinds.is_empty() {
            ItemKind::ALL.to_vec()
        } else {
            req.kinds.clone()
        };
        let mut scopes = self.held_all(&req.reach, &kinds).await?;
        scopes.sort_by_key(|scope| std::cmp::Reverse(scope.path.matches('/').count()));
        let mut report = EraseReport::default();
        for scope in scopes {
            let erased = self.log.erase(&scope.path).await?;
            log::debug!("[cortex] erased {} ({erased:?})", scope.path);
            report.erased_scopes = report.erased_scopes.saturating_add(erased.scopes);
            report.receipts.extend(erased.ids);
        }
        Ok(report)
    }
}

/// Whether `req` erases a proxied tenant's whole memory: its `user:<id>`
/// node (the tenant, see `tenant`), its descendants, every kind.
fn is_whole_tenant(req: &EraseRequest) -> bool {
    req.reach.at.depth() == 1
        && super::tenant::tenant_of(&req.reach.at).is_some()
        && req.reach.descendants
        && req.kinds.is_empty()
}

/// Whether `req` erases everything: the root, its descendants, every kind.
/// [`EraseRequest::validate`] has already refused it without `whole_tree`.
fn is_whole_tree(req: &EraseRequest) -> bool {
    req.whole_tree && req.reach.at.is_root() && req.reach.descendants && req.kinds.is_empty()
}
