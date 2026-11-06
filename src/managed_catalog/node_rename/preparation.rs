//! Bounded original preparation discovery. Hints never grant media authority.
use super::*;

fn page_range(task: &NodeMutationTask) -> (Vec<u8>, Vec<u8>) {
    let mut begin = prefix(task);
    begin.push(4);
    let mut end = prefix(task);
    end.push(5);
    (begin, end)
}
async fn has_attempt_state(tr: &Transaction, task: &NodeMutationTask) -> Result<bool> {
    let mut begin = prefix(task);
    begin.push(5);
    let mut end = prefix(task);
    end.push(10);
    Ok(!tr
        .get_range(
            &RangeOption {
                begin: KeySelector::first_greater_or_equal(begin),
                end: KeySelector::first_greater_or_equal(end),
                limit: Some(1),
                ..Default::default()
            },
            1,
            false,
        )
        .await?
        .is_empty())
}
pub(in crate::managed_catalog) async fn require_unsealed_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
) -> Result<()> {
    if tr.get(&global_key(&manifest.task), false).await?.is_some()
        || has_attempt_state(tr, &manifest.task).await?
    {
        return Err(bad_participants());
    }
    Ok(())
}
fn require_page_scope(
    page: &ParticipantPage,
    manifest: &NodeRenameManifest,
    owner: u64,
    index: u64,
) -> Result<()> {
    if page.creator > owner
        || page.facts.index != index
        || page.facts.logical != manifest.request.fingerprint()
        || page.facts.manifest != manifest.header.summary.sha256
    {
        return Err(bad_participants());
    }
    Ok(())
}
async fn page_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
    owner: u64,
    index: u64,
) -> Result<ParticipantPage> {
    let raw = tr
        .get(&participant_key(&manifest.task, index), false)
        .await?
        .ok_or_else(bad_participants)?;
    let page = ParticipantPage::decode(&raw)?;
    require_page_scope(&page, manifest, owner, index)?;
    Ok(page)
}
pub(in crate::managed_catalog) async fn require_no_saved_participants(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
) -> Result<()> {
    let (begin, _) = page_range(&manifest.task);
    if tr.get(&global_key(&manifest.task), false).await?.is_some()
        || requests::prefix_has_rows(tr, &begin).await?
        || has_attempt_state(tr, &manifest.task).await?
    {
        return Err(bad_participants());
    }
    Ok(())
}
pub(in crate::managed_catalog) async fn require_fresh_page_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
    owner: u64,
    index: u64,
    previous: [u8; 32],
) -> Result<()> {
    require_unsealed_at(tr, manifest).await?;
    let (_, end) = page_range(&manifest.task);
    let later = tr
        .get_range(
            &RangeOption {
                begin: KeySelector::first_greater_than(participant_key(&manifest.task, index)),
                end: KeySelector::first_greater_or_equal(end),
                limit: Some(1),
                ..Default::default()
            },
            1,
            false,
        )
        .await?;
    if !later.is_empty() {
        return Err(bad_participants());
    }
    if index > 0 && page_at(tr, manifest, owner, index - 1).await?.chain()? != previous {
        return Err(bad_participants());
    }
    Ok(())
}
/// Refuse to mint a replacement for an original referenced descriptor. An
/// unsealed future page may still acquire its first recipe after earlier pages.
pub(in crate::managed_catalog) async fn require_unreferenced_recipe_at(
    tr: &Transaction,
    manifest: &NodeRenameManifest,
    owner: u64,
    volume: Uuid,
) -> Result<()> {
    let header = tr
        .get(&global_key(&manifest.task), false)
        .await?
        .map(|raw| GlobalHeader::decode(&raw, manifest))
        .transpose()?;
    if header.as_ref().is_some_and(|h| h.creator > owner)
        || header.is_none() && has_attempt_state(tr, &manifest.task).await?
    {
        return Err(bad_participants());
    }
    let (begin, end) = page_range(&manifest.task);
    let rows = tr
        .get_range(
            &RangeOption {
                begin: KeySelector::first_greater_or_equal(begin.clone()),
                end: KeySelector::first_greater_or_equal(end),
                limit: Some(1),
                reverse: true,
                ..Default::default()
            },
            1,
            false,
        )
        .await?;
    let Some(last) = rows.first() else {
        return if header.is_some() {
            Err(bad_participants())
        } else {
            Ok(())
        };
    };
    if last.key().len() != begin.len() + 8 || !last.key().starts_with(&begin) {
        return Err(bad_participants());
    }
    let index = u64::from_be_bytes(
        last.key()[begin.len()..]
            .try_into()
            .expect("fixed page index"),
    );
    let page = ParticipantPage::decode(last.value())?;
    require_page_scope(&page, manifest, owner, index)?;
    if let Some(header) = &header
        && page.chain()? != header.summary.sha256
    {
        return Err(bad_participants());
    }
    let maximum = manifest
        .header
        .summary
        .files
        .saturating_add(1)
        .div_ceil(MEMBERS_PER_PAGE as u64);
    if index >= maximum || header.as_ref().is_some_and(|h| h.pages != index + 1) {
        return Err(bad_participants());
    }
    let (mut low, mut high) = (0, index.checked_add(1).ok_or_else(bad_participants)?);
    while low < high {
        let middle = low + (high - low) / 2;
        let page = page_at(tr, manifest, owner, middle).await?;
        let first = page
            .facts
            .members
            .first()
            .ok_or_else(bad_participants)?
            .volume;
        let last = page
            .facts
            .members
            .last()
            .ok_or_else(bad_participants)?
            .volume;
        if volume < first {
            high = middle;
        } else if volume > last {
            low = middle + 1;
        } else {
            if page.facts.members.iter().any(|m| m.volume == volume) {
                return Err(bad_participants());
            }
            return Ok(());
        }
    }
    Ok(())
}

pub(super) fn candidate_page(
    capture: &NodeRenameParticipantsCapture,
    projection: &NodeRenameProjectionSpool,
) -> Result<(NodeRenameParticipantsCapture, Vec<NodeRenameParticipant>)> {
    let mut next = capture.clone();
    let mut members = Vec::new();
    if capture.complete {
        return Ok((next, members));
    }
    while members.len() < MEMBERS_PER_PAGE {
        if next.pending.is_empty() && !next.source_end {
            let page = projection.volumes_page(next.cursor.as_deref(), MEMBERS_PER_PAGE)?;
            next.cursor = page.next;
            next.source_end = next.cursor.is_none();
            next.pending = page.items.into();
        }
        let Some(member) = next_member(
            next.pending.front(),
            &mut next.target,
            &mut next.anchor_pending,
        ) else {
            next.complete = true;
            break;
        };
        if next
            .pending
            .front()
            .is_some_and(|source| source.volume == member.volume)
        {
            next.pending.pop_front();
        }
        members.push(member);
    }
    Ok((next, members))
}
impl ManagedCatalog {
    /// At most 32 original volume IDs; neither admission nor an I/O permit.
    pub async fn node_rename_next_participant_volumes(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        capture: &NodeRenameParticipantsCapture,
        projection: &mut NodeRenameProjectionSpool,
    ) -> Result<Vec<Uuid>> {
        let manifest = &capture.manifest;
        projection.require_manifest(&manifest.request, &manifest.header.summary)?;
        let tr = self.db.create_trx()?;
        require_original(&tr, route, owner, manifest).await?;
        let (_, members) = candidate_page(capture, projection)?;
        Ok(members.into_iter().map(|member| member.volume).collect())
    }
}
