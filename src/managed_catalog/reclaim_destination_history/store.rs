//! One current chain, one proposal and at most one retired chain per scope.
//! Every FDB operation revalidates the private catalog context. Local directory
//! work happens outside transactions and stays within one scratch reservation.
use super::*;
use serde::{Deserialize, Serialize};

fn state_key(prefix: &[u8]) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.push(b's');
    key
}
fn node_key(prefix: &[u8], root: Root) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.push(b'n');
    key.extend_from_slice(&root.id);
    key
}
fn chunk_key(prefix: &[u8], root: Root, index: u64) -> Vec<u8> {
    let mut key = node_key(prefix, root);
    key.push(b'c');
    key.extend_from_slice(&index.to_be_bytes());
    key
}
fn pin_prefix(prefix: &[u8]) -> Vec<u8> {
    let mut key = prefix.to_vec();
    key.push(b'p');
    key
}
fn pin_key(prefix: &[u8], nonce: Uuid) -> Vec<u8> {
    let mut key = pin_prefix(prefix);
    key.extend_from_slice(nonce.as_bytes());
    key
}
fn parent_pin_prefix(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = parent_prefix(reservation);
    key.push(b'r');
    key
}
fn parent_pin_key(reservation: &ReclaimReservation, owner: u64, nonce: Uuid) -> Vec<u8> {
    let mut key = parent_pin_prefix(reservation);
    key.extend_from_slice(&owner.to_be_bytes());
    key.extend_from_slice(nonce.as_bytes());
    key
}
async fn state_at(tr: &Transaction, prefix: &[u8]) -> Result<State> {
    match tr.get(&state_key(prefix), false).await? {
        Some(raw) => State::decode(&raw),
        None => Ok(State::default()),
    }
}
fn write_state(tr: &Transaction, prefix: &[u8], state: &State) -> Result<()> {
    tr.set(&state_key(prefix), &state.encode()?);
    Ok(())
}
async fn node_at(tr: &Transaction, prefix: &[u8], root: Root) -> Result<Node> {
    let raw = tr
        .get(&node_key(prefix, root), false)
        .await?
        .ok_or(invalid())?;
    Node::decode(&raw)?.require_root(root)
}
async fn hint_state_at(tr: &Transaction, prefix: &[u8]) -> Result<Option<State>> {
    let raw = tr.get(&state_key(prefix), false).await?;
    Ok(match raw {
        Some(raw) => State::decode(&raw).ok(),
        None => Some(State::default()),
    })
}
async fn hint_node_at(tr: &Transaction, prefix: &[u8], root: Root) -> Result<Option<Node>> {
    let Some(raw) = tr.get(&node_key(prefix, root), false).await? else {
        return Ok(None);
    };
    Ok(Node::decode(&raw)
        .and_then(|node| node.require_root(root))
        .ok())
}
fn scratch_hint<T>(result: crate::error::Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(
            crate::error::TapeError::LocalCapacity { .. }
            | crate::error::TapeError::Ltfs(_)
            | crate::error::TapeError::RecoveryRestricted { .. },
        ) => Ok(None),
        Err(error) => Err(cache(error)),
    }
}
async fn immutable_at(tr: &Transaction, key: &[u8], bytes: &[u8]) -> Result<()> {
    match tr.get(key, false).await? {
        Some(raw) if raw.as_ref() != bytes => Err(invalid()),
        Some(_) => Ok(()),
        None => {
            tr.set(key, bytes);
            Ok(())
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pin {
    owner: u64,
    root: Root,
}
async fn parent_owner_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<()> {
    if record_at(tr, reservation, owner).await?["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(())
}

impl ManagedCatalog {
    async fn pin_destination_history(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        context: &DestinationHistoryContext,
        nonce: Uuid,
    ) -> Result<Option<(Root, Node)>> {
        let prefix = context.prefix(reservation);
        self.db
            .run(|tr, _| {
                let prefix = &prefix;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            context.require_at(&tr, reservation, owner).await?;
                            let Some(mut state) = hint_state_at(&tr, prefix).await? else {
                                return Ok(None);
                            };
                            let key = pin_key(prefix, nonce);
                            let index = parent_pin_key(reservation, owner, nonce);
                            // A commit-unknown retry resumes exactly the already-pinned root,
                            // even if Current acquired another immutable Append meanwhile.
                            if let Some(raw) = tr.get(&key, false).await? {
                                let pin: Pin =
                                    serde_json::from_slice(&raw).map_err(|_| invalid())?;
                                if pin.owner != owner
                                    || tr.get(&index, false).await?.as_deref()
                                        != Some(prefix.as_slice())
                                {
                                    return Err(invalid());
                                }
                                let node = node_at(&tr, prefix, pin.root).await?;
                                return Ok(Some((pin.root, node)));
                            }
                            if state.retired.as_ref().is_some_and(|root| root.collecting)
                                || state.pins >= 32
                            {
                                return Ok(None);
                            }
                            let Some(root) = state.current else {
                                return Ok(None);
                            };
                            let Some(node) = hint_node_at(&tr, prefix, root).await? else {
                                return Ok(None);
                            };
                            let pin = Pin { owner, root };
                            let bytes = serde_json::to_vec(&pin).map_err(cache)?;
                            state.pins = state.pins.checked_add(1).ok_or(invalid())?;
                            let parent = parent_pins_key(reservation);
                            let total = counter_at(&tr, &parent)
                                .await?
                                .checked_add(1)
                                .ok_or(invalid())?;
                            immutable_at(&tr, &index, prefix).await?;
                            tr.set(&key, &bytes);
                            write_counter(&tr, &parent, total);
                            write_state(&tr, prefix, &state)?;
                            Ok(Some((root, node)))
                        }
                        .await,
                    )
                })
            })
            .await?
    }
    // Release needs current parent/owner, but not the original head or task
    // phase: both may legitimately advance after the last loaded page.
    async fn unpin_destination_history(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        prefix: &[u8],
        nonce: Uuid,
    ) -> Result<()> {
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_owner_at(&tr, reservation, owner).await?;
                            let key = pin_key(prefix, nonce);
                            let Some(raw) = tr.get(&key, false).await? else {
                                return Ok(());
                            };
                            let pin: Pin = serde_json::from_slice(&raw).map_err(|_| invalid())?;
                            if pin.owner != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            let mut state = state_at(&tr, prefix).await?;
                            state.pins = state.pins.checked_sub(1).ok_or(invalid())?;
                            let parent = parent_pins_key(reservation);
                            let total = counter_at(&tr, &parent)
                                .await?
                                .checked_sub(1)
                                .ok_or(invalid())?;
                            let index = parent_pin_key(reservation, owner, nonce);
                            if tr.get(&index, false).await?.as_deref() != Some(prefix) {
                                return Err(invalid());
                            }
                            tr.clear(&index);
                            tr.clear(&key);
                            write_counter(&tr, &parent, total);
                            write_state(&tr, prefix, &state)?;
                            Ok(())
                        }
                        .await,
                    )
                })
            })
            .await?
    }
    pub(crate) async fn load_reclaim_destination_history_hint(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        context: &DestinationHistoryContext,
        budget: &IndexScratchBudget,
    ) -> Result<Option<FullHistoryDirectory>> {
        let nonce = Uuid::new_v4();
        let prefix = context.prefix(reservation);
        let Some((root, node)) = self
            .pin_destination_history(reservation, owner, context, nonce)
            .await?
        else {
            return Ok(None);
        };
        let result = self
            .load_destination_history_pinned(reservation, owner, context, root, node, budget)
            .await;
        self.unpin_destination_history(reservation, owner, &prefix, nonce)
            .await?;
        result
    }
    async fn load_destination_history_pinned(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        context: &DestinationHistoryContext,
        root: Root,
        mut node: Node,
        budget: &IndexScratchBudget,
    ) -> Result<Option<FullHistoryDirectory>> {
        let prefix = context.prefix(reservation);
        let manifest = node.manifest();
        let Some(mut builder) = scratch_hint(FullHistoryHintBuilder::new(manifest, budget))? else {
            return Ok(None);
        };
        let mut remaining = manifest.entries;
        let mut expected = root;
        loop {
            if node.require_root(expected).is_err() {
                return Ok(None);
            }
            match node {
                Node::Append { parent, entry, .. } => {
                    if remaining < 2 {
                        return Ok(None);
                    }
                    if scratch_hint(builder.append_entries(&entry.to_bytes()))?.is_none() {
                        return Ok(None);
                    }
                    let tr = self.db.create_trx()?;
                    context.require_at(&tr, reservation, owner).await?;
                    let Some(parent_node) = hint_node_at(&tr, &prefix, parent).await? else {
                        return Ok(None);
                    };
                    if node.require_parent(parent_node).is_err() {
                        return Ok(None);
                    }
                    remaining -= 1;
                    node = parent_node;
                    expected = parent;
                }
                Node::Base(base) => {
                    if base.entries != remaining {
                        return Ok(None);
                    }
                    let chunks = base.entries.div_ceil(ENTRIES_PER_CHUNK);
                    let mut first = 0;
                    while first < chunks {
                        let end = (first + TRANSACTION_PAGE).min(chunks);
                        let tr = self.db.create_trx()?;
                        context.require_at(&tr, reservation, owner).await?;
                        if hint_node_at(&tr, &prefix, expected).await?.is_none() {
                            return Ok(None);
                        }
                        let mut pages = Vec::with_capacity(TRANSACTION_PAGE as usize);
                        for index in first..end {
                            let Some(raw) =
                                tr.get(&chunk_key(&prefix, expected, index), false).await?
                            else {
                                return Ok(None);
                            };
                            let count = (base.entries - index * ENTRIES_PER_CHUNK)
                                .min(ENTRIES_PER_CHUNK)
                                as usize;
                            if raw.len() != count * FULL_HISTORY_ENTRY_BYTES {
                                return Ok(None);
                            }
                            pages.push(raw.to_vec());
                        }
                        drop(tr);
                        for page in pages {
                            if scratch_hint(builder.append_entries(&page))?.is_none() {
                                return Ok(None);
                            }
                        }
                        first = end;
                    }
                    break;
                }
            }
        }
        let tr = self.db.create_trx()?;
        context.require_at(&tr, reservation, owner).await?;
        if hint_node_at(&tr, &prefix, root).await?.is_none() {
            return Ok(None);
        }
        drop(tr);
        scratch_hint(builder.finish())
    }
    pub(crate) async fn publish_reclaim_destination_history_hint(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        context: &DestinationHistoryContext,
        captured: &CapturedIndexSource,
    ) -> Result<bool> {
        let Some(directory) = captured.full_history_hint() else {
            return Ok(false);
        };
        context.require_capture(captured)?;
        let prefix = context.prefix(reservation);
        let manifest = directory.manifest();
        let tr = self.db.create_trx()?;
        context.require_at(&tr, reservation, owner).await?;
        let Some(state) = hint_state_at(&tr, &prefix).await? else {
            return Ok(false);
        };
        let current = state.current;
        let old = match current {
            Some(root) => {
                let Some(node) = hint_node_at(&tr, &prefix, root).await? else {
                    return Ok(false);
                };
                Some(node)
            }
            None => None,
        };
        drop(tr);
        if old.is_some_and(|node| node.manifest() == manifest) {
            return Ok(true);
        }
        // A corrupt/stale scheduling hint can name a later history. Reusing
        // one of its ancestor Base IDs would create shared GC ownership. Keep
        // actual discovery authoritative and skip optional persistence instead.
        if old.is_some_and(|node| {
            let previous = node.manifest();
            manifest.entries <= previous.entries
                || manifest.head.location.start_block <= previous.head.location.start_block
                || manifest.head.generation < previous.head.generation
                || manifest.head.volume_uuid != previous.head.volume_uuid
                || manifest.block_size != previous.block_size
                || manifest.index_partition != previous.index_partition
                || manifest.data_partition != previous.data_partition
        }) {
            return Ok(false);
        }
        let mut proposal = Node::Base(manifest);
        if let (Some(root), Some(old)) = (current, old) {
            let old_manifest = old.manifest();
            if old_manifest.entries.checked_add(1) == Some(manifest.entries) {
                // Compare every actual prefix byte against the old semantic
                // digest. Header.previous alone is insufficient for append.
                let mut digest = Sha256::new();
                let mut count = 0;
                let mut last = None;
                directory
                    .for_each_entry(|entry| {
                        if count < old_manifest.entries {
                            digest.update(entry.to_bytes());
                        } else {
                            last = Some(entry);
                        }
                        count += 1;
                        Ok(())
                    })
                    .map_err(cache)?;
                if count == manifest.entries
                    && <[u8; 32]>::from(digest.finalize()) == old_manifest.entries_sha256
                {
                    if let Some(entry) = last {
                        let append = Node::Append {
                            parent: root,
                            manifest,
                            entry,
                        };
                        if append.require_parent(old).is_ok() {
                            proposal = append;
                        }
                    }
                }
            }
        }
        let root = proposal.root();
        let bytes = proposal.encode();
        let admitted = self
            .db
            .run(|tr, _| {
                let prefix = &prefix;
                let bytes = &bytes;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            context.require_at(&tr, reservation, owner).await?;
                            let mut state = state_at(&tr, prefix).await?;
                            if state.current == Some(root) {
                                node_at(&tr, prefix, root).await?;
                                return Ok(true);
                            }
                            if state.current != current {
                                return Ok(false);
                            }
                            if let Some(stage) = &state.staging {
                                if stage.node != *bytes || stage.expected_current != current {
                                    return Ok(false);
                                }
                            } else {
                                if matches!(proposal, Node::Base(_)) && state.retired.is_some() {
                                    return Ok(false);
                                }
                                state.staging = Some(Staging {
                                    node: bytes.clone(),
                                    expected_current: current,
                                    next_chunk: 0,
                                });
                                write_state(&tr, prefix, &state)?;
                            }
                            Ok(true)
                        }
                        .await,
                    )
                })
            })
            .await??;
        if !admitted {
            return Ok(false);
        }
        if matches!(proposal, Node::Base(_)) {
            let mut after = None;
            let mut index = 0;
            let mut count = 0;
            let mut digest = Sha256::new();
            loop {
                let page = directory
                    .entry_page(after, (ENTRIES_PER_CHUNK * TRANSACTION_PAGE) as usize)
                    .map_err(cache)?;
                if page.is_empty() {
                    break;
                }
                let first = index;
                let mut chunks = Vec::with_capacity(TRANSACTION_PAGE as usize);
                for part in page.chunks(ENTRIES_PER_CHUNK as usize) {
                    let mut raw = Vec::with_capacity(part.len() * FULL_HISTORY_ENTRY_BYTES);
                    for entry in part {
                        let bytes = entry.to_bytes();
                        digest.update(bytes);
                        raw.extend_from_slice(&bytes);
                        count += 1;
                    }
                    chunks.push(raw);
                }
                index += chunks.len() as u64;
                after = page.last().map(|entry| entry.start_block);
                self.db
                    .run(|tr, _| {
                        let prefix = &prefix;
                        let bytes = &bytes;
                        let chunks = &chunks;
                        Box::pin(async move {
                            requests::retry_result(
                                async {
                                    context.require_at(&tr, reservation, owner).await?;
                                    let mut state = state_at(&tr, prefix).await?;
                                    if state.current == Some(root) {
                                        node_at(&tr, prefix, root).await?;
                                        return Ok(());
                                    }
                                    let stage = state.staging.as_mut().ok_or(invalid())?;
                                    if stage.node != *bytes
                                        || stage.expected_current != current
                                        || state.current != current
                                        || stage.next_chunk < first
                                    {
                                        return Err(invalid());
                                    }
                                    for (offset, raw) in chunks.iter().enumerate() {
                                        immutable_at(
                                            &tr,
                                            &chunk_key(prefix, root, first + offset as u64),
                                            raw,
                                        )
                                        .await?;
                                    }
                                    stage.next_chunk = stage.next_chunk.max(index);
                                    write_state(&tr, prefix, &state)?;
                                    Ok(())
                                }
                                .await,
                            )
                        })
                    })
                    .await??;
            }
            if count != manifest.entries
                || <[u8; 32]>::from(digest.finalize()) != manifest.entries_sha256
            {
                return Err(invalid());
            }
        }
        self.db
            .run(|tr, _| {
                let prefix = &prefix;
                let bytes = &bytes;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            context.require_at(&tr, reservation, owner).await?;
                            let mut state = state_at(&tr, prefix).await?;
                            if state.current == Some(root) {
                                node_at(&tr, prefix, root).await?;
                                return Ok(true);
                            }
                            let stage = state.staging.as_ref().ok_or(invalid())?;
                            if state.current != current
                                || stage.expected_current != current
                                || stage.node != *bytes
                                || matches!(proposal, Node::Base(_))
                                    && stage.next_chunk
                                        != manifest.entries.div_ceil(ENTRIES_PER_CHUNK)
                            {
                                return Err(invalid());
                            }
                            if let Node::Append { parent, .. } = proposal {
                                if Some(parent) != current {
                                    return Err(invalid());
                                }
                                proposal.require_parent(node_at(&tr, prefix, parent).await?)?;
                            } else if let Some(old) = current {
                                if state.retired.is_some() {
                                    return Err(invalid());
                                }
                                let count = node_at(&tr, prefix, old).await?.manifest().entries;
                                state.retired = Some(Retired {
                                    root: old,
                                    entries_left: count,
                                    next_chunk: 0,
                                    collecting: false,
                                });
                            }
                            immutable_at(&tr, &node_key(prefix, root), bytes).await?;
                            state.current = Some(root);
                            state.staging = None;
                            write_state(&tr, prefix, &state)?;
                            Ok(true)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}

impl ManagedCatalog {
    /// Only an already fenced, strictly newer runtime owner can release old
    /// readers. No wall clock or expiration is used to infer reader death.
    pub(crate) async fn reap_reclaim_parent_history_pins(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<u64> {
        let index_prefix = parent_pin_prefix(reservation);
        let mut scope_start = parent_prefix(reservation);
        scope_start.extend_from_slice(b"s\x02");
        self.db
            .run(|tr, _| {
                let index_prefix = &index_prefix;
                let scope_start = &scope_start;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            parent_owner_at(&tr, reservation, owner).await?;
                            control::reclaim::isolation::require_normal_mutation_at(
                                &tr,
                                reservation,
                                owner,
                            )
                            .await?;
                            let range = foundationdb::RangeOption {
                                limit: Some(32),
                                ..foundationdb::RangeOption::from((
                                    index_prefix.clone(),
                                    prefix_end(index_prefix),
                                ))
                            };
                            let rows = tr.get_range(&range, 1, false).await?;
                            let mut count = 0;
                            for row in rows.iter() {
                                let key = row.key();
                                let scope = row.value();
                                if key.len() != index_prefix.len() + 24
                                    || scope.len() != scope_start.len() + 32
                                    || !scope.starts_with(scope_start)
                                {
                                    return Err(invalid());
                                }
                                let saved_owner = u64::from_be_bytes(
                                    key[index_prefix.len()..index_prefix.len() + 8]
                                        .try_into()
                                        .unwrap(),
                                );
                                if saved_owner >= owner {
                                    if saved_owner > owner {
                                        return Err(CatalogError::OwnerFenced);
                                    }
                                    break;
                                }
                                let nonce = Uuid::from_slice(&key[index_prefix.len() + 8..])
                                    .map_err(|_| invalid())?;
                                let local = pin_key(scope, nonce);
                                let raw = tr.get(&local, false).await?.ok_or(invalid())?;
                                let pin: Pin =
                                    serde_json::from_slice(&raw).map_err(|_| invalid())?;
                                if pin.owner != saved_owner {
                                    return Err(invalid());
                                }
                                let mut state = state_at(&tr, scope).await?;
                                state.pins = state.pins.checked_sub(1).ok_or(invalid())?;
                                tr.clear(&local);
                                tr.clear(key);
                                write_state(&tr, scope, &state)?;
                                count += 1;
                            }
                            if count > 0 {
                                let key = parent_pins_key(reservation);
                                let total = counter_at(&tr, &key)
                                    .await?
                                    .checked_sub(count)
                                    .ok_or(invalid())?;
                                write_counter(&tr, &key, total);
                            }
                            Ok(count)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
    /// One bounded GC transaction. New pins are excluded atomically until the
    /// retired chain is gone; a current Append never loses an ancestor.
    pub(crate) async fn collect_reclaim_destination_history(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        context: &DestinationHistoryContext,
    ) -> Result<bool> {
        let prefix = context.prefix(reservation);
        self.db
            .run(|tr, _| {
                let prefix = &prefix;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            context.require_at(&tr, reservation, owner).await?;
                            let mut state = state_at(&tr, prefix).await?;
                            let Some(mut retired) = state.retired.clone() else {
                                return Ok(true);
                            };
                            if state.pins != 0
                                || requests::prefix_has_rows(&tr, &pin_prefix(prefix)).await?
                            {
                                return Ok(false);
                            }
                            let current = state.current.ok_or(invalid())?;
                            if node_at(&tr, prefix, current).await?.manifest().entries
                                <= retired.entries_left
                            {
                                return Err(invalid());
                            }
                            retired.collecting = true;
                            let mut budget = TRANSACTION_PAGE;
                            while budget > 0 {
                                let node = node_at(&tr, prefix, retired.root).await?;
                                match node {
                                    Node::Append { parent, .. } => {
                                        let parent_node = node_at(&tr, prefix, parent).await?;
                                        node.require_parent(parent_node)?;
                                        if retired.entries_left != node.manifest().entries
                                            || retired.next_chunk != 0
                                        {
                                            return Err(invalid());
                                        }
                                        tr.clear(&node_key(prefix, retired.root));
                                        retired.root = parent;
                                        retired.entries_left -= 1;
                                        budget -= 1;
                                    }
                                    Node::Base(manifest) => {
                                        if retired.entries_left != manifest.entries {
                                            return Err(invalid());
                                        }
                                        let chunks = manifest.entries.div_ceil(ENTRIES_PER_CHUNK);
                                        if retired.next_chunk > chunks {
                                            return Err(invalid());
                                        }
                                        while retired.next_chunk < chunks && budget > 0 {
                                            tr.clear(&chunk_key(
                                                prefix,
                                                retired.root,
                                                retired.next_chunk,
                                            ));
                                            retired.next_chunk += 1;
                                            budget -= 1;
                                        }
                                        if retired.next_chunk == chunks {
                                            tr.clear(&node_key(prefix, retired.root));
                                            state.retired = None;
                                            write_state(&tr, prefix, &state)?;
                                            return Ok(true);
                                        }
                                        break;
                                    }
                                }
                            }
                            state.retired = Some(retired);
                            write_state(&tr, prefix, &state)?;
                            Ok(false)
                        }
                        .await,
                    )
                })
            })
            .await?
    }
}

/// Called only inside an original parent terminal/cancellation transaction,
/// before the parent identity is removed. All original business gates remain
/// with that transaction. Its atomic terminal change fences new pins/proposals.
pub(in super::super) async fn clear_parent_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<()> {
    parent_owner_at(tr, reservation, owner).await?;
    if counter_at(tr, &parent_pins_key(reservation)).await? != 0
        || requests::prefix_has_rows(tr, &parent_pin_prefix(reservation)).await?
    {
        return Err(CatalogError::PoolBusy);
    }
    let prefix = parent_prefix(reservation);
    tr.clear_range(&prefix, &prefix_end(&prefix));
    Ok(())
}
