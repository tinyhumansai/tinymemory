//! The local filesystem implementation of the shared-tree claim port.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::Digest;

use super::error::{Error, Result};
use super::job::LegacyClaim;

/// A hard-link claim marker for a legacy tree shared by local accounts.
///
/// The marker is stored at
/// `<app_dir>/memory/legacy_claims/<sha256(normalized-endpoint)>.json`.
/// Hard-link creation makes the first account the winner without replacing a
/// claim another account already made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsLegacyClaim {
    marker: PathBuf,
    owner: String,
    endpoint: String,
}

impl FsLegacyClaim {
    /// A claim for `owner` on the tree served by `endpoint`.
    #[must_use]
    pub fn new(app_dir: &Path, endpoint: &str, owner: &str) -> Self {
        let endpoint = normalize(endpoint);
        let digest: String = sha2::Sha256::digest(endpoint.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            marker: app_dir
                .join("memory")
                .join("legacy_claims")
                .join(format!("{digest}.json")),
            owner: owner.to_string(),
            endpoint,
        }
    }

    /// The marker path shared by spellings of the same normalized endpoint.
    #[must_use]
    pub fn marker_path(&self) -> &Path {
        &self.marker
    }

    /// The claiming account's actor id.
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The normalized endpoint used for this marker.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn recorded_owner(&self) -> Result<Option<String>> {
        match std::fs::read(&self.marker) {
            Ok(bytes) => serde_json::from_slice::<Marker>(&bytes)
                .map(|marker| Some(marker.owner))
                .map_err(|error| unreadable(&error)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(unreadable(&error)),
        }
    }

    fn take_with_directory_sync(
        &self,
        sync_directory: impl Fn(&Path) -> std::io::Result<()>,
    ) -> Result<bool> {
        if let Some(owner) = self.recorded_owner()? {
            if owner != self.owner {
                return Ok(false);
            }
            self.sync_parent_directory(&sync_directory)?;
            return Ok(true);
        }
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.marker.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let temp =
                tempfile::NamedTempFile::new_in(self.marker.parent().unwrap_or(Path::new(".")))?;
            let marker = Marker {
                endpoint: self.endpoint.clone(),
                owner: self.owner.clone(),
            };
            serde_json::to_writer(temp.as_file(), &marker).map_err(std::io::Error::other)?;
            temp.as_file().sync_all()?;
            // Fails when the marker exists: never replaces another account's
            // claim.
            std::fs::hard_link(temp.path(), &self.marker)
        };
        match write() {
            Ok(()) => {
                self.sync_parent_directory(&sync_directory)?;
                log::info!("[memory:layout_migration] legacy tree claimed");
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if self
                    .recorded_owner()?
                    .is_some_and(|owner| owner == self.owner)
                {
                    self.sync_parent_directory(&sync_directory)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            Err(error) => Err(Error::State(format!(
                "legacy tree claim not saved: {error}"
            ))),
        }
    }

    fn sync_parent_directory(
        &self,
        sync_directory: &impl Fn(&Path) -> std::io::Result<()>,
    ) -> Result<()> {
        let parent = self.marker.parent().unwrap_or(Path::new("."));
        sync_directory(parent).map_err(|error| {
            Error::State(format!("legacy tree claim directory not synced: {error}"))
        })
    }
}

impl LegacyClaim for FsLegacyClaim {
    fn held_by_other(&self) -> Result<bool> {
        Ok(self
            .recorded_owner()?
            .is_some_and(|owner| owner != self.owner))
    }

    fn take(&self) -> Result<bool> {
        self.take_with_directory_sync(sync_directory)
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

// Opening directories as files is not portable to Windows. The marker file
// itself is still synced before linking there, preserving existing behavior.
#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[derive(Serialize, Deserialize)]
struct Marker {
    endpoint: String,
    owner: String,
}

/// One spelling per endpoint: trimmed, scheme and host lowercased, default
/// ports and trailing slashes dropped.
fn normalize(endpoint: &str) -> String {
    let endpoint = endpoint.trim();
    match url::Url::parse(endpoint) {
        Ok(url) => url.as_str().trim_end_matches('/').to_string(),
        Err(_) => endpoint.trim_end_matches('/').to_ascii_lowercase(),
    }
}

fn unreadable(error: &dyn std::fmt::Display) -> Error {
    Error::State(format!("legacy tree claim unreadable: {error}"))
}

#[cfg(test)]
#[path = "fs_claim_tests.rs"]
mod tests;
