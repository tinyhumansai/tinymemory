//! Retrying the items a finished import skipped because the engine refused
//! them, and resuming such a retry after an interruption. A retry is the
//! user's action: when it stops it says so and waits for Retry again.

use std::collections::HashSet;

use tinymemory_api::Error as EngineError;

use super::run::{is_insufficient_credits, skips_item};
use super::state::legacy_id;
use super::{Begun, ImportError, ImportFile, ImportPhase, Importer, open_legacy};

impl Importer {
    /// Starts a retry of the items the finished import skipped. Items that now
    /// store leave the list; ones refused again stay, with the new reason.
    ///
    /// # Errors
    ///
    /// [`ImportError::NothingToRetry`] unless the import is `Done` (or a retry
    /// was interrupted) with failed items listed.
    pub fn begin_retry(&self) -> Result<Begun, ImportError> {
        let file = self.store.load();
        // A retry the app quit during is still a retry the user may press
        // again.
        let interrupted = file.state.phase == ImportPhase::Running && file.retrying;
        if !(file.state.phase == ImportPhase::Done || interrupted) || file.failed.is_empty() {
            return Err(ImportError::NothingToRetry);
        }
        Ok(self.mark_retrying(file))
    }

    /// Resumes a retry the app quit during, from the items still listed, with
    /// no check beyond what the host already made.
    pub fn begin_retry_resume(&self) -> Begun {
        self.mark_retrying(self.store.load())
    }

    fn mark_retrying(&self, mut file: ImportFile) -> Begun {
        file.state.phase = ImportPhase::Running;
        file.state.error = None;
        file.retrying = true;
        self.save(&file);
        log::info!(
            "[memory:import] retrying failed items, failed={}",
            file.failed.len()
        );
        Begun {
            state: file.state.clone(),
            file,
        }
    }

    /// Why a retry of failed items stopped, and that pressing Retry again
    /// continues it.
    fn retry_stop_message(&self, error: &EngineError) -> String {
        if error.is_transient() {
            return "the memory service is unavailable; press Retry again once it is back"
                .to_string();
        }
        if is_insufficient_credits(error) {
            return "not enough credits to retry these items; top up, then press Retry again"
                .to_string();
        }
        format!(
            "the retry stopped: {}; press Retry again",
            (self.describe)(error)
        )
    }

    /// Drives a begun retry to its end. The pause check is asked before every
    /// item; a pause leaves the retry `Running` with what is still listed.
    pub async fn run_retry(&self, begun: Begun) {
        let mut file = begun.file;
        // An item with no legacy id cannot be found again, so it is never
        // matched (every item the importer yields has one).
        let wanted: HashSet<String> = file
            .failed
            .iter()
            .map(|failed| failed.id.clone())
            .filter(|id| !id.is_empty())
            .collect();
        let items = open_legacy(&self.workspace_dir)
            .map_err(|error| error.to_string())
            .and_then(|workspace| {
                workspace
                    .items()
                    .filter_map(|imported| match imported {
                        Ok(imported) if wanted.contains(&legacy_id(&imported.item)) => {
                            Some(Ok(imported.item))
                        }
                        Ok(_) => None,
                        Err(error) => Some(Err(error.to_string())),
                    })
                    .collect::<Result<Vec<_>, String>>()
            });
        let mut failure = None;
        match items {
            Err(error) => {
                failure = Some(format!(
                    "reading the legacy store failed: {error}; press Retry again"
                ));
            }
            Ok(items) => {
                for item in items {
                    if self.paused_now() {
                        log::info!("[memory:import] background paused; retry left to resume");
                        self.save(&file);
                        return;
                    }
                    let id = legacy_id(&item);
                    match self.with_retries(|| self.engine.store(item.clone())).await {
                        Ok(_) => {
                            file.failed.retain(|failed| failed.id != id);
                            file.state.imported += 1;
                            file.state.failed = file.failed.len() as u64;
                            // Persisted per item, so a quit mid-retry keeps
                            // what was stored off the list.
                            self.save(&file);
                        }
                        Err(error) if skips_item(&error) => {
                            let again = self.failed_item(&item, &error);
                            if let Some(failed) = file.failed.iter_mut().find(|f| f.id == id) {
                                *failed = again;
                            }
                            self.save(&file);
                        }
                        Err(error) => {
                            failure = Some(self.retry_stop_message(&error));
                            break;
                        }
                    }
                }
            }
        }
        file.state.failed = file.failed.len() as u64;
        // The import itself is finished either way, so it stays `Done`: a
        // retry stopped by the engine or the account keeps the items still to
        // retry and says why.
        file.state.phase = ImportPhase::Done;
        file.state.error = failure;
        file.retrying = false;
        log::info!(
            "[memory:import] retry of failed items finished, still_failed={}",
            file.failed.len()
        );
        self.save(&file);
    }
}
