//! Test-only workspace-scoped provider context behavior.

use super::*;

impl ProviderContext {
    pub fn memory_client(&self) -> Option<crate::store::MemoryClientRef> {
        crate::store::MemoryClient::from_workspace_dir(self.config.workspace_dir().clone())
            .ok()
            .map(std::sync::Arc::new)
    }
}
