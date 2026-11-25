//! Immutable complete Full-history scheduling hints. These records never
//! replace media reads, publish file copies, or authorize source initialization.
use super::*;
use crate::ltfs::recovery::{
    CapturedIndexSource, FULL_HISTORY_ENTRY_BYTES, FullHistoryDirectory, FullHistoryHintBuilder,
    FullHistoryManifest, IndexScratchBudget,
};
use crate::ltfs::volume::CommittedIndexConstruct;

const ENTRIES_PER_CHUNK: u64 = 384;
const CHUNKS_PER_TRANSACTION: u64 = 4;

// The interposer runs only after a real committed short transaction or a
// completed read. It can drop an already-durable acknowledgement in tests;
// production builds contain no callback, alternate authority or retry path.
#[cfg(test)]
mod test_interposer {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    type Future = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;
    pub(super) type Hook = dyn Fn(&'static str, u64, u64) -> Future + Send + Sync;
    type Hooks = HashMap<Uuid, (Uuid, Arc<Hook>)>;
    static HOOKS: OnceLock<Mutex<Hooks>> = OnceLock::new();

    fn hooks() -> &'static Mutex<Hooks> {
        HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    struct Installed {
        namespace: Uuid,
        identity: Uuid,
    }
    impl Drop for Installed {
        fn drop(&mut self) {
            let mut hooks = hooks().lock().unwrap();
            if hooks
                .get(&self.namespace)
                .is_some_and(|(id, _)| *id == self.identity)
            {
                hooks.remove(&self.namespace);
            }
        }
    }

    pub(super) fn install(namespace: Uuid, hook: Arc<Hook>) -> impl Drop {
        let identity = Uuid::new_v4();
        assert!(
            hooks()
                .lock()
                .unwrap()
                .insert(namespace, (identity, hook))
                .is_none()
        );
        Installed {
            namespace,
            identity,
        }
    }

    pub(super) async fn after(
        namespace: Uuid,
        point: &'static str,
        first: u64,
        end: u64,
    ) -> Result<()> {
        let hook = hooks()
            .lock()
            .unwrap()
            .get(&namespace)
            .map(|(_, hook)| hook.clone());
        if let Some(hook) = hook {
            hook(point, first, end).await?;
        }
        Ok(())
    }
}

fn cache(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::Cache(format!("完整 Full 历史位置提示: {error}"))
}

fn prefix(namespace: Uuid) -> Vec<u8> {
    let mut key = crate::managed_catalog::key(crate::managed_catalog::FULL_HISTORY_HINT_KIND);
    key.extend_from_slice(namespace.as_bytes());
    key
}

fn head_key(namespace: Uuid, head: ActiveIndexIdentity) -> Vec<u8> {
    let mut key = prefix(namespace);
    key.push(b'h');
    let mut encoded = Vec::new();
    active_volume::encode_index(&mut encoded, head);
    key.extend_from_slice(&Sha256::digest(encoded));
    key
}

fn object_prefix(namespace: Uuid, manifest: FullHistoryManifest) -> Vec<u8> {
    let mut key = prefix(namespace);
    key.push(b'o');
    key.extend_from_slice(&manifest.fingerprint());
    key
}

fn complete_key(namespace: Uuid, manifest: FullHistoryManifest) -> Vec<u8> {
    let mut key = object_prefix(namespace, manifest);
    key.push(b'm');
    key
}

fn chunk_key(namespace: Uuid, manifest: FullHistoryManifest, chunk: u64) -> Vec<u8> {
    let mut key = object_prefix(namespace, manifest);
    key.push(b'c');
    key.extend_from_slice(&chunk.to_be_bytes());
    key
}

fn committed(head: ActiveIndexIdentity) -> CommittedIndexConstruct {
    CommittedIndexConstruct {
        volume_uuid: head.volume_uuid,
        generation: head.generation,
        location: head.location,
        sha256: head.sha256,
        incremental: head.incremental,
    }
}

async fn require_current_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<ActiveIndexIdentity> {
    let parent = record_at(tr, reservation, owner).await?;
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    if parent["phase"] == "reformatting" {
        return Err(CatalogError::Invalid("格式化阶段不能使用旧源历史位置提示"));
    }
    source_state_at(tr, reservation).await.map(|(_, head)| head)
}

fn optional_hint_error(error: crate::error::TapeError) -> Result<Option<FullHistoryDirectory>> {
    match error {
        crate::error::TapeError::LocalCapacity { .. }
        | crate::error::TapeError::RecoveryRestricted { .. }
        | crate::error::TapeError::Ltfs(_) => Ok(None),
        other => Err(cache(other)),
    }
}

impl ManagedCatalog {
    #[cfg(test)]
    pub(crate) fn install_reclaim_history_hint_test_hook(
        &self,
        namespace: Uuid,
        hook: std::sync::Arc<test_interposer::Hook>,
    ) -> impl Drop {
        test_interposer::install(namespace, hook)
    }

    /// Publish only locations emitted by an actual complete source capture.
    /// Every transaction validates the original parent/owner/assignment/head.
    /// Retry effects are immutable FDB writes; tape commands never run here.
    pub(crate) async fn publish_reclaim_full_history_hint(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        source: &CapturedIndexSource,
    ) -> Result<bool> {
        let Some(directory) = source.full_history_hint() else {
            return Ok(false);
        };
        let manifest = directory.manifest();
        if manifest.head != source.construct()
            || manifest.dp_eod != source.construct_end()
            || !matches!(
                source.scope(),
                crate::ltfs::recovery::IndexSourceScope::Current {
                    history_truncated: false,
                    ..
                }
            )
        {
            return Err(CatalogError::Invalid("历史位置提示未绑定当前完整源捕获"));
        }
        let namespace = reservation.namespace();
        let mut after = None;
        let mut next_chunk = 0u64;
        let mut entries = 0u64;
        let mut digest = Sha256::new();
        loop {
            // This is local disk IO before the transaction, with a fixed upper
            // bound of four 26,496-byte chunks, independent of history length.
            let page = directory
                .entry_page(after, (ENTRIES_PER_CHUNK * CHUNKS_PER_TRANSACTION) as usize)
                .map_err(cache)?;
            if page.is_empty() {
                break;
            }
            let first_chunk = next_chunk;
            let mut chunks = Vec::with_capacity(CHUNKS_PER_TRANSACTION as usize);
            for part in page.chunks(ENTRIES_PER_CHUNK as usize) {
                let mut bytes = Vec::with_capacity(part.len() * FULL_HISTORY_ENTRY_BYTES);
                for entry in part {
                    let encoded = entry.to_bytes();
                    digest.update(encoded);
                    bytes.extend_from_slice(&encoded);
                }
                chunks.push(bytes);
            }
            self.db
                .run(|tr, _| {
                    let chunks = &chunks;
                    Box::pin(async move {
                        tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                            512_000,
                        ))?;
                        requests::retry_result(
                            async {
                                if committed(require_current_at(&tr, reservation, owner).await?)
                                    != manifest.head
                                {
                                    return Err(CatalogError::Invalid(
                                        "历史提示发布时原源卷头改变",
                                    ));
                                }
                                for (offset, bytes) in chunks.iter().enumerate() {
                                    let key =
                                        chunk_key(namespace, manifest, first_chunk + offset as u64);
                                    if let Some(saved) = tr.get(&key, false).await? {
                                        if saved.as_ref() != bytes {
                                            return Err(CatalogError::Corrupt(
                                                "不可变历史位置分片重试冲突",
                                            ));
                                        }
                                    } else {
                                        tr.set(&key, bytes);
                                    }
                                }
                                #[cfg(test)]
                                assert!(tr.get_approximate_size().await? < 512_000);
                                Ok(())
                            }
                            .await,
                        )
                    })
                })
                .await??;
            #[cfg(test)]
            test_interposer::after(
                namespace,
                "chunks-committed",
                first_chunk,
                first_chunk + chunks.len() as u64,
            )
            .await?;
            entries = entries
                .checked_add(page.len() as u64)
                .ok_or(CatalogError::Invalid("历史位置条目计数溢出"))?;
            next_chunk += chunks.len() as u64;
            after = page.last().map(|entry| entry.start_block);
        }
        if entries != manifest.entries
            || <[u8; 32]>::from(digest.finalize()) != manifest.entries_sha256
        {
            return Err(CatalogError::Corrupt("源捕获历史位置在分片期间改变"));
        }
        let raw = manifest.to_bytes();
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let head = require_current_at(&tr, reservation, owner).await?;
                            if committed(head) != manifest.head {
                                return Err(CatalogError::Invalid("历史提示完成时原源卷头改变"));
                            }
                            let complete = complete_key(namespace, manifest);
                            if let Some(saved) = tr.get(&complete, false).await? {
                                if saved.as_ref() != raw {
                                    return Err(CatalogError::Corrupt("不可变历史位置清单冲突"));
                                }
                            } else {
                                tr.set(&complete, &raw);
                            }
                            // Atomic small visibility pointer, independent of R/source
                            // authority. Namespace holds retain all immutable chunks.
                            tr.set(&head_key(namespace, head), &raw);
                            Ok(())
                        }
                        .await,
                    )
                })
            })
            .await??;
        #[cfg(test)]
        test_interposer::after(namespace, "complete-committed", 0, next_chunk).await?;
        Ok(true)
    }

    /// Load an optional scheduling candidate into bounded private scratch.
    /// Missing/invalid hints fall back to actual discovery. FDB/owner failures
    /// remain failures and cannot be turned into a local-catalog success.
    pub(crate) async fn load_reclaim_full_history_hint(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        scratch: &IndexScratchBudget,
    ) -> Result<Option<FullHistoryDirectory>> {
        let namespace = reservation.namespace();
        let tr = self.db.create_trx()?;
        let head = require_current_at(&tr, reservation, owner).await?;
        if head.incremental {
            return Ok(None);
        }
        let pointer = head_key(namespace, head);
        let Some(raw) = tr.get(&pointer, false).await? else {
            return Ok(None);
        };
        let manifest = match FullHistoryManifest::from_bytes(&raw) {
            Ok(manifest) if manifest.head == committed(head) => manifest,
            _ => return Ok(None),
        };
        if tr
            .get(&complete_key(namespace, manifest), false)
            .await?
            .as_deref()
            != Some(raw.as_ref())
        {
            return Ok(None);
        }
        drop(tr);
        let mut builder = match FullHistoryHintBuilder::new(manifest, scratch) {
            Ok(builder) => builder,
            Err(error) => return optional_hint_error(error),
        };
        let total = manifest.entries.div_ceil(ENTRIES_PER_CHUNK);
        let mut first = 0;
        while first < total {
            let end = (first + CHUNKS_PER_TRANSACTION).min(total);
            let chunks = self
                .db
                .run(|tr, _| {
                    let pointer = &pointer;
                    let raw = &raw;
                    Box::pin(async move {
                        requests::retry_result(
                            async {
                                if require_current_at(&tr, reservation, owner).await? != head {
                                    return Err(CatalogError::Invalid(
                                        "历史提示装入时原源卷头改变",
                                    ));
                                }
                                if tr.get(pointer, false).await?.as_deref() != Some(raw.as_ref())
                                    || tr
                                        .get(&complete_key(namespace, manifest), false)
                                        .await?
                                        .as_deref()
                                        != Some(raw.as_ref())
                                {
                                    return Ok(None);
                                }
                                let mut chunks =
                                    Vec::with_capacity(CHUNKS_PER_TRANSACTION as usize);
                                for index in first..end {
                                    let Some(bytes) = tr
                                        .get(&chunk_key(namespace, manifest, index), false)
                                        .await?
                                    else {
                                        return Ok(None);
                                    };
                                    let entries = (manifest.entries - index * ENTRIES_PER_CHUNK)
                                        .min(ENTRIES_PER_CHUNK);
                                    if bytes.len() != entries as usize * FULL_HISTORY_ENTRY_BYTES {
                                        return Ok(None);
                                    }
                                    chunks.push(bytes.to_vec());
                                }
                                Ok(Some(chunks))
                            }
                            .await,
                        )
                    })
                })
                .await??;
            #[cfg(test)]
            test_interposer::after(namespace, "load-batch-read", first, end).await?;
            let Some(chunks) = chunks else {
                return Ok(None);
            };
            // SQLite/codec work stays outside the short FDB transaction.
            for bytes in chunks {
                if let Err(error) = builder.append_entries(&bytes) {
                    return optional_hint_error(error);
                }
            }
            first = end;
        }
        let directory = match builder.finish() {
            Ok(directory) => directory,
            Err(error) => return optional_hint_error(error),
        };
        let tr = self.db.create_trx()?;
        if require_current_at(&tr, reservation, owner).await? != head {
            return Err(CatalogError::Invalid("历史提示结清时原源卷头改变"));
        }
        if tr.get(&pointer, false).await?.as_deref() != Some(raw.as_ref())
            || tr
                .get(&complete_key(namespace, manifest), false)
                .await?
                .as_deref()
                != Some(raw.as_ref())
        {
            return Ok(None);
        }
        drop(tr);
        #[cfg(test)]
        test_interposer::after(namespace, "load-finished", 0, total).await?;
        Ok(Some(directory))
    }
}
