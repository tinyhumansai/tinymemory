//! Beginning and driving an import.

use tinymemory_api::{Error as EngineError, StoreItem, WaitFor, WriteOptions};

use super::state::legacy_id;
use super::{
    Begun, CHECKPOINT_EVERY, FailedItem, ImportError, ImportFile, ImportPhase, Importer,
    STORE_BATCH, count_legacy, open_legacy,
};
use crate::import::{Checkpoint, ImportedItem};

/// The reason a run stopped on an engine that stayed unavailable through every
/// retry; it is resumed on its own.
const UNAVAILABLE_REASON: &str =
    "the memory service is unavailable; the import resumes on its own within a few minutes";

/// Whether `error` is the hosted backend's "not enough credits" refusal.
#[must_use]
pub fn is_insufficient_credits(error: &EngineError) -> bool {
    matches!(error, EngineError::InsufficientCredits(_))
}

/// Whether a store failure is about the item (skip it and go on) rather than
/// the engine or the account (stop, so a resume retries from here).
///
/// Only a refusal of the item itself is skippable: malformed or too large
/// (`InvalidRequest`), not storable here (`Unsupported`, `NotFound`), or a
/// replay key already bound to a different body (`Conflict`). Everything else
/// would fail the next item the same way: a rejected credential, an exhausted
/// credit balance, an unreachable or overloaded engine. Skipping those would
/// advance the checkpoint past items that were never stored and finish `Done`
/// with nothing imported.
#[must_use]
pub fn skips_item(error: &EngineError) -> bool {
    matches!(
        error,
        EngineError::InvalidRequest(_)
            | EngineError::Unsupported(_)
            | EngineError::NotFound(_)
            | EngineError::Conflict(_)
    )
}

/// What storing one batch of legacy items did.
#[derive(Debug, Default)]
struct BatchOutcome {
    /// Items stored (or replayed).
    stored: u64,
    /// The checkpoint after the last item handled, stored or skipped.
    checkpoint: Option<Checkpoint>,
    /// Why the import must stop.
    fatal: Option<String>,
    /// Whether `fatal` is transient: the engine stayed unavailable through
    /// every retry, so a background job resumes the run.
    transient: bool,
    /// Whether `fatal` is the account's credits running out.
    credits: bool,
    /// Items the engine refused, skipped so the rest could go on.
    failed: Vec<FailedItem>,
}

impl Importer {
    pub(crate) fn save(&self, file: &ImportFile) {
        if let Err(error) = self.store.save(file) {
            log::warn!("[memory:import] writing import state failed: {error}");
        }
    }

    pub(crate) fn failed_item(&self, item: &StoreItem, error: &EngineError) -> FailedItem {
        FailedItem {
            id: legacy_id(item),
            reason: (self.describe)(error),
        }
    }

    pub(crate) fn paused_now(&self) -> bool {
        self.paused.as_ref().is_some_and(|paused| paused())
    }

    /// Runs `call` again after each of the policy's delays while it fails with
    /// a transient error, and returns its last result.
    pub(crate) async fn with_retries<T, F, Fut>(&self, mut call: F) -> tinymemory_api::Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = tinymemory_api::Result<T>>,
    {
        let mut result = call().await;
        for delay in &self.policy.delays {
            match &result {
                Err(error) if error.is_transient() => {
                    log::debug!(
                        "[memory:import] engine unavailable; retrying in {} ms: {error}",
                        delay.as_millis()
                    );
                    self.pacer.sleep(*delay).await;
                    result = call().await;
                }
                _ => break,
            }
        }
        result
    }

    pub(crate) fn fatal_message(&self, error: &EngineError) -> String {
        if error.is_transient() {
            return UNAVAILABLE_REASON.to_string();
        }
        if is_insufficient_credits(error) {
            return "not enough credits to import your memory; top up, then resume the import \
                    to continue where it stopped"
                .to_string();
        }
        (self.describe)(error)
    }

    /// Checks consent, sizes the legacy store, marks the state `Running` and
    /// returns the run to hand to [`Importer::run`]. A `Done` import starts
    /// over; an interrupted one resumes from its checkpoint.
    ///
    /// Blocking: it reads the legacy store.
    ///
    /// # Errors
    ///
    /// [`ImportError::ConsentRequired`] without consent and
    /// [`ImportError::NoLegacyStore`] when the workspace holds no v1 store.
    pub fn begin(&self) -> Result<Begun, ImportError> {
        if !(self.consent)() {
            return Err(ImportError::ConsentRequired);
        }
        let mut file = self.store.load();
        if file.state.phase == ImportPhase::Done {
            file = ImportFile::default();
        }
        // A resumed import already knows its total: only check the store is
        // still there, instead of reading all of it again before the run
        // reads it once more.
        let resuming = !file.checkpoint.is_start() && file.state.total > 0;
        let total = if resuming {
            open_legacy(&self.workspace_dir).ok().map(|_| None)
        } else {
            count_legacy(&self.workspace_dir).map(|counts| Some(counts.total()))
        };
        let Some(total) = total else {
            return Err(ImportError::NoLegacyStore);
        };
        file.state.phase = ImportPhase::Running;
        file.paused_for_credits = false;
        if let Some(total) = total {
            file.state.total = total;
        }
        file.state.error = None;
        self.save(&file);
        log::info!("[memory:import] import started, total={}", file.state.total);
        Ok(Begun {
            state: file.state.clone(),
            file,
        })
    }

    /// Stores `batch` in one bulk call. If the engine refuses the batch
    /// because of an item (see [`skips_item`]), the items are stored one at a
    /// time instead so only the bad ones are skipped. Any other failure stops
    /// the import without advancing the checkpoint past what was stored.
    async fn store_batch(&self, batch: Vec<ImportedItem>, wait: WaitFor) -> BatchOutcome {
        let Some(last) = batch.last() else {
            return BatchOutcome::default();
        };
        let last_checkpoint = last.checkpoint.clone();
        let items: Vec<_> = batch.iter().map(|imported| imported.item.clone()).collect();
        let options = WriteOptions { wait };
        match self
            .with_retries(|| self.engine.store_many_with(items.clone(), options))
            .await
        {
            Ok(receipts) => {
                log::debug!("[memory:import] batch stored, count={}", receipts.len());
                return BatchOutcome {
                    stored: receipts.len() as u64,
                    checkpoint: Some(last_checkpoint),
                    ..BatchOutcome::default()
                };
            }
            Err(error) if !skips_item(&error) => {
                return BatchOutcome {
                    transient: error.is_transient(),
                    credits: is_insufficient_credits(&error),
                    fatal: Some(self.fatal_message(&error)),
                    ..BatchOutcome::default()
                };
            }
            Err(error) => {
                log::debug!("[memory:import] batch refused; storing one by one: {error}");
            }
        }
        let mut outcome = BatchOutcome::default();
        for imported in batch {
            match self
                .with_retries(|| self.engine.store(imported.item.clone()))
                .await
            {
                Ok(_) => outcome.stored += 1,
                Err(error) if !skips_item(&error) => {
                    outcome.transient = error.is_transient();
                    outcome.credits = is_insufficient_credits(&error);
                    outcome.fatal = Some(self.fatal_message(&error));
                    return outcome;
                }
                Err(error) => {
                    log::debug!("[memory:import] item skipped: {error}");
                    outcome
                        .failed
                        .push(self.failed_item(&imported.item, &error));
                }
            }
            outcome.checkpoint = Some(imported.checkpoint);
        }
        outcome
    }

    /// Drives a begun import to its end and persists the outcome: `Done`, an
    /// `Error` the user resumes, a transient stop left `Running` for a
    /// background tick, or (when paused) `Running` untouched.
    ///
    /// The legacy store is read a batch at a time between awaits, so the
    /// host runs this where a short blocking read is acceptable.
    pub async fn run(&self, begun: Begun) {
        let mut file = begun.file;
        let mut failure = None;
        let workspace = match open_legacy(&self.workspace_dir) {
            Ok(workspace) => Some(workspace),
            Err(error) => {
                failure = Some(format!("reading the legacy store failed: {error}"));
                None
            }
        };
        let mut read_from = file.checkpoint.clone();
        let mut since_checkpoint = 0u64;
        let mut transient = false;
        let mut credits = false;
        let mut pausing = false;
        let mut last: Vec<ImportedItem> = Vec::new();
        while let Some(workspace) = workspace.as_ref().filter(|_| failure.is_none()) {
            let mut batch: Vec<ImportedItem> = Vec::with_capacity(STORE_BATCH);
            for imported in workspace.items_from(&read_from).take(STORE_BATCH) {
                match imported {
                    Ok(imported) => batch.push(imported),
                    Err(error) => {
                        failure = Some(format!("reading the legacy store failed: {error}"));
                        batch.clear();
                        break;
                    }
                }
            }
            if batch.is_empty() {
                break;
            }
            let exhausted = batch.len() < STORE_BATCH;
            if let Some(newest) = batch.last() {
                read_from = newest.checkpoint.clone();
            }
            if self.paused_now() {
                pausing = true;
                break;
            }
            // Accepted: nothing reads the import back until it ends, so a
            // batch does not wait to be listed; the last one is waited for
            // below.
            last.clone_from(&batch);
            let outcome = self.store_batch(batch, WaitFor::Accepted).await;
            file.state.imported += outcome.stored;
            // One entry per legacy id; a later refusal replaces the reason.
            for failed in outcome.failed {
                // Every item the importer yields has a legacy id; one without
                // could never be found again to retry, so it is not listed.
                if failed.id.is_empty() {
                    log::warn!(
                        "[memory:import] a refused item has no legacy id; not kept for retry"
                    );
                    continue;
                }
                match file.failed.iter_mut().find(|known| known.id == failed.id) {
                    Some(known) => *known = failed,
                    None => file.failed.push(failed),
                }
            }
            file.state.failed = file.failed.len() as u64;
            if let Some(checkpoint) = outcome.checkpoint {
                file.checkpoint = checkpoint;
            }
            since_checkpoint += outcome.stored;
            if since_checkpoint >= CHECKPOINT_EVERY {
                self.save(&file);
                since_checkpoint = 0;
            }
            if let Some(error) = outcome.fatal {
                failure = Some(error);
                transient = outcome.transient;
                credits = outcome.credits;
                break;
            }
            if exhausted {
                break;
            }
        }
        drop(workspace);
        // Every batch was stored accepted. Before the import says it is done
        // (and organizing reads it back), wait once for the last batch to be
        // listed: storing it again visible is a replay that returns when it
        // is, retried like any batch. The wait is best-effort: every item is
        // already accepted, so a listing still behind finishes the import
        // rather than leaving it to resume into the same wait again.
        if failure.is_none() && !pausing && !last.is_empty() {
            let settled = self.store_batch(last, WaitFor::Visible).await;
            file.listed_unconfirmed = settled.fatal.is_some();
            if let Some(error) = settled.fatal {
                log::warn!(
                    "[memory:import] the last batch is not listed yet; finishing anyway: {error}"
                );
            }
        }
        if pausing {
            // Left `Running` with its checkpoint: the next unpaused tick
            // resumes it.
            log::info!(
                "[memory:import] background paused; import left to resume, imported={}",
                file.state.imported
            );
            self.save(&file);
            return;
        }
        match failure {
            // Left `Running` with its checkpoint and the reason: the next
            // background tick resumes it, as it does an import the app quit.
            Some(error) if transient => {
                log::info!("[memory:import] engine unavailable; import left to resume: {error}");
                file.state.error = Some(error);
            }
            Some(error) => {
                file.paused_for_credits = credits;
                log::warn!(
                    "[memory:import] import stopped, imported={}",
                    file.state.imported
                );
                file.state.phase = ImportPhase::Error;
                file.state.error = Some(error);
            }
            None => {
                log::info!(
                    "[memory:import] import finished, imported={}",
                    file.state.imported
                );
                file.state.phase = ImportPhase::Done;
                file.state.error = None;
            }
        }
        self.save(&file);
    }

    /// Records that an automatic resume of an interrupted run could not start,
    /// so it stops instead of looping: the progress and the checkpoint stay as
    /// persisted and the user's Resume continues from there. An interrupted
    /// retry leaves the import `Done` with its failed items, so Retry stays
    /// available.
    pub fn record_resume_failure(&self, error: &str) {
        super::record_resume_failure(self.store.as_ref(), error);
    }
}
