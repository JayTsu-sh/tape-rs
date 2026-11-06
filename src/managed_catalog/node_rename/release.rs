//! Bounded claim release while the original Publishing pool hold remains held.
use super::*;

pub(super) async fn require_s4_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
    intent: &NodeRenameVolumeIntent,
) -> Result<NodeRenameObservation> {
    if closure::at(tr, intent).await?.is_some() {
        return Err(CatalogError::Incomplete);
    }
    let saved = observation::observation_at(tr, intent)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let binding = intent.plan.recipe().record.facts.binding()?;
    let (scan, current) =
        active_volume::observed_at(tr, &manifest.task.library_serial, intent.volume()).await?;
    if scan != binding.scan
        || current != saved.current_index()
        || active_volume::history_at(
            tr,
            &manifest.task.library_serial,
            intent.volume(),
            current.generation,
        )
        .await?
        .0 != active_volume::IndexKind::NodeRename(intent.id())
    {
        return Err(invalid_attempt());
    }
    Ok(saved)
}

impl ManagedCatalog {
    /// Release at most one original ready page. A completed retry returns the
    /// retained progress; neither a foreign nor a missing claim is cleared.
    pub async fn release_node_rename_publication_page(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &mut NodeRenameAttempt,
    ) -> Result<bool> {
        let original = attempt.clone();
        let global = &original.global;
        let manifest = &global.manifest;
        let tr = self.db.create_trx()?;
        writing::require_held_original_at(&tr, route, owner, manifest).await?;
        let expected = current_group_at(&tr, manifest, &global.header)
            .await?
            .ok_or_else(invalid_attempt)?;
        if record_id(&expected.id)? != original.id()
            || expected.fingerprint()? != original.fingerprint()
            || expected.phase != NodeRenameAttemptPhase::Publishing
            || expected.owner > owner
        {
            return Err(invalid_attempt());
        }
        let index = expected
            .completion
            .as_ref()
            .ok_or_else(invalid_attempt)?
            .released_pages;
        drop(tr);
        if index == expected.pages {
            attempt.record = expected;
            return Ok(false);
        }
        let record = self
            .db
            .run(|tr, _| {
                let expected = expected.clone();
                async move {
                    requests::retry_result(
                        async {
                            writing::require_held_original_at(&tr, route, owner, manifest).await?;
                            original_global_at(&tr, manifest, &global.header).await?;
                            let current = current_group_at(&tr, manifest, &global.header)
                                .await?
                                .ok_or_else(invalid_attempt)?;
                            if current.phase != NodeRenameAttemptPhase::Publishing
                                || current.fingerprint()? != expected.fingerprint()?
                                || current.progress_revision != expected.progress_revision
                                || current.completion.as_ref().map(|c| (c.revision, c.chain))
                                    != expected.completion.as_ref().map(|c| (c.revision, c.chain))
                            {
                                return Err(invalid_attempt());
                            }
                            let done = current.completion.as_ref().ok_or_else(invalid_attempt)?;
                            if done.released_pages > index {
                                return Ok(current);
                            }
                            if current != expected {
                                return Err(invalid_attempt());
                            }
                            let page = saved_ready_at(&tr, &manifest.task, &current, index).await?;
                            let mut chain = done.released_chain;
                            let mut keys = Vec::with_capacity(page.intents.len());
                            for reference in &page.intents {
                                let intent = rebase::current_intent_at(
                                    &tr,
                                    manifest,
                                    &global.header,
                                    &current,
                                    reference,
                                    index,
                                    true,
                                )
                                .await?;
                                let saved = require_s4_at(&tr, manifest, &intent).await?;
                                chain = sealing::add_observation(chain, reference, &saved)?;
                                keys.push(node_attempts::volume_key(intent.volume()));
                            }
                            let mut next = current;
                            let completion =
                                next.completion.as_mut().ok_or_else(invalid_attempt)?;
                            completion.released_pages = index + 1;
                            completion.released_chain = chain;
                            let next = next.seal()?;
                            let encoded = record_bytes(&next)?;
                            let target = group_key(&manifest.task, record_id(&next.id)?);
                            // The pool hold remains. Every fallible operation precedes clears.
                            for key in keys {
                                tr.clear(&key);
                            }
                            tr.set(&target, &encoded);
                            Ok(next)
                        }
                        .await,
                    )
                }
            })
            .await??;
        attempt.record = record;
        Ok(true)
    }
}
