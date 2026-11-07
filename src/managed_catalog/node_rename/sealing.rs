//! Confirm every original participant before closing the group to further I/O.
use super::*;

pub(super) fn initial_chain(group: &GroupRecord) -> Result<[u8; 32]> {
    Ok(Sha256::new()
        .chain_update(b"tape-rs/node-rename-all-s4/1")
        .chain_update(group.fingerprint()?)
        .chain_update(group.progress_revision.to_be_bytes())
        .finalize()
        .into())
}
pub(super) fn add_observation(
    previous: [u8; 32],
    reference: &IntentRef,
    value: &NodeRenameObservation,
) -> Result<[u8; 32]> {
    if value.intent().record.original_reference()? != *reference {
        return Err(invalid_attempt());
    }
    Ok(Sha256::new()
        .chain_update(b"tape-rs/node-rename-all-s4-member/1")
        .chain_update(previous)
        .chain_update(record_bytes(reference)?)
        .chain_update(observation::encode_observation(value)?)
        .finalize()
        .into())
}
async fn require_snapshot_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    attempt: &NodeRenameAttempt,
    expected: &GroupRecord,
) -> Result<()> {
    let global = &attempt.global;
    let manifest = &global.manifest;
    let task = writing::require_held_original_at(tr, route, owner, manifest).await?;
    original_global_at(tr, manifest, &global.header).await?;
    if task.state != NodeMutationTaskState::Writing
        || current_group_at(tr, manifest, &global.header)
            .await?
            .as_ref()
            != Some(expected)
        || record_id(&expected.id)? != attempt.id()
        || expected.fingerprint()? != attempt.fingerprint()
        || expected.owner > owner
        || !matches!(
            expected.phase,
            NodeRenameAttemptPhase::Writing | NodeRenameAttemptPhase::Publishing
        )
    {
        return Err(invalid_attempt());
    }
    Ok(())
}

impl ManagedCatalog {
    /// Seal all current per-volume S4s of the original group. Each read is
    /// bounded by the original ready page; the final CAS is constant-sized.
    /// Publishing retains every claim and the pool hold until namespace commit.
    pub async fn seal_node_rename_s4s(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        attempt: &mut NodeRenameAttempt,
    ) -> Result<()> {
        let requested = attempt.clone();
        let global = &requested.global;
        let manifest = &global.manifest;
        let tr = self.db.create_trx()?;
        let expected = current_group_at(&tr, manifest, &global.header)
            .await?
            .ok_or_else(invalid_attempt)?;
        require_snapshot_at(&tr, route, owner, &requested, &expected).await?;
        drop(tr);
        let mut chain = initial_chain(&expected)?;
        let mut ready_chain = expected.initial_chain()?;
        let mut count = 0_u64;
        for index in 0..expected.pages {
            let tr = self.db.create_trx()?;
            require_snapshot_at(&tr, route, owner, &requested, &expected).await?;
            let page = saved_ready_at(&tr, &manifest.task, &expected, index).await?;
            if page.previous != ready_chain {
                return Err(invalid_attempt());
            }
            for reference in &page.intents {
                let intent = rebase::current_intent_at(
                    &tr,
                    manifest,
                    &global.header,
                    &expected,
                    reference,
                    index,
                    expected
                        .completion
                        .as_ref()
                        .is_none_or(|c| index >= c.released_pages),
                )
                .await?;
                if closure::at(&tr, &intent).await?.is_some() {
                    return Err(CatalogError::Incomplete);
                }
                let value = observation::observation_at(&tr, &intent)
                    .await?
                    .ok_or(CatalogError::Incomplete)?;
                let binding = intent.plan.recipe().record.facts.binding()?;
                let (scan, current) =
                    active_volume::observed_at(&tr, &manifest.task.library_serial, intent.volume())
                        .await?;
                if scan != binding.scan
                    || current != value.current_index()
                    || active_volume::history_at(
                        &tr,
                        &manifest.task.library_serial,
                        intent.volume(),
                        current.generation,
                    )
                    .await?
                    .0 != active_volume::IndexKind::NodeRename(intent.id())
                {
                    return Err(invalid_attempt());
                }
                chain = add_observation(chain, reference, &value)?;
                count = count.checked_add(1).ok_or_else(invalid_attempt)?;
            }
            ready_chain = page.chain()?;
        }
        if ready_chain != expected.ready_chain || count != expected.volumes {
            return Err(invalid_attempt());
        }
        let completion = CompletionSeal {
            revision: expected.progress_revision,
            chain,
            released_pages: expected.completion.as_ref().map_or(0, |c| c.released_pages),
            released_chain: expected
                .completion
                .as_ref()
                .map_or(initial_chain(&expected)?, |c| c.released_chain),
        };
        if expected
            .completion
            .as_ref()
            .is_some_and(|saved| *saved != completion)
        {
            return Err(invalid_attempt());
        }
        let record = self
            .db
            .run(|tr, _| {
                let expected = expected.clone();
                let completion = completion.clone();
                let attempt = &requested;
                async move {
                    requests::retry_result(
                        async {
                            let mut current = expected.clone();
                            current.phase = NodeRenameAttemptPhase::Publishing;
                            current.completion = Some(completion);
                            let current = current.seal()?;
                            // A committed reply may have been lost between transaction retries.
                            let saved = current_group_at(&tr, manifest, &global.header).await?;
                            let snapshot = if saved.as_ref() == Some(&current) {
                                &current
                            } else {
                                &expected
                            };
                            require_snapshot_at(&tr, route, owner, attempt, snapshot).await?;
                            revalidate_at(&tr, route, owner, &manifest.request).await?;
                            if saved.as_ref() == Some(&current) {
                                return Ok(current);
                            }
                            let encoded = record_bytes(&current)?;
                            let key = group_key(&manifest.task, record_id(&current.id)?);
                            tr.set(&key, &encoded);
                            Ok(current)
                        }
                        .await,
                    )
                }
            })
            .await??;
        attempt.record = record;
        Ok(())
    }
}
