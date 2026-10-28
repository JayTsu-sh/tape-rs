//! Durable source identities for a private, audited import. One row per tape;
//! no file-sized state is retained while checking a large staged directory.

use crate::catalog::volume_extents::ExpectedIndex;
use crate::ltfs::index::IndexLocation;
use sha2::Digest;

use super::*;

pub(super) const SOURCES_DOMAIN: &[u8] = b"tape-rs/staged-import-sources/1";
/// A managed import with this marker contains per-source data identity and EE target path (row v5).
pub(super) const REQUIRED_VERSION: &[u8] = &[5];
const SUMMARY_LEN: usize = 1 + staged_import::MANIFEST_LEN + 8 + 32;
const STAGED_END: usize = 1 + staged_import::MANIFEST_LEN;
const VOLUMES_END: usize = STAGED_END + 8;
const SOURCE_PAGE: usize = 64;
type SourceMap = std::collections::HashMap<String, (Uuid, Uuid, u64)>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StagedSourceSummary {
    pub staged: StagedImportSummary,
    pub volumes: u64,
    pub sources_sha256: [u8; 32],
}

impl StagedSourceSummary {
    fn encode(self) -> [u8; SUMMARY_LEN] {
        let mut bytes = [0; SUMMARY_LEN];
        bytes[0] = 2;
        bytes[1..STAGED_END].copy_from_slice(&self.staged.encode());
        bytes[STAGED_END..VOLUMES_END].copy_from_slice(&self.volumes.to_be_bytes());
        bytes[VOLUMES_END..SUMMARY_LEN].copy_from_slice(&self.sources_sha256);
        bytes
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != SUMMARY_LEN || bytes[0] != 2 {
            return Err(CatalogError::Corrupt("暂存来源清单格式"));
        }
        let staged = StagedImportSummary::decode(&bytes[1..STAGED_END])?;
        if !staged.complete {
            return Err(CatalogError::Corrupt("暂存来源清单尚未完成"));
        }
        Ok(Self {
            staged,
            volumes: read_u64(&bytes[STAGED_END..VOLUMES_END]),
            sources_sha256: bytes[VOLUMES_END..SUMMARY_LEN]
                .try_into()
                .expect("validated length"),
        })
    }
}

pub(super) fn source_prefix(namespace: Uuid) -> Vec<u8> {
    let mut out = key(b'k');
    out.extend_from_slice(namespace.as_bytes());
    out
}

pub(super) fn source_key(namespace: Uuid, volume: Uuid) -> Vec<u8> {
    let mut out = source_prefix(namespace);
    out.extend_from_slice(volume.as_bytes());
    out
}

pub(super) fn summary_key(namespace: Uuid) -> Vec<u8> {
    let mut out = key(b'l');
    out.extend_from_slice(namespace.as_bytes());
    out
}

pub(super) fn confirmed_key(namespace: Uuid) -> Vec<u8> {
    let mut out = key(b'n');
    out.extend_from_slice(namespace.as_bytes());
    out
}

pub(super) fn binding_required_key(namespace: Uuid) -> Vec<u8> {
    let mut out = key(b'o');
    out.extend_from_slice(namespace.as_bytes());
    out
}

pub(super) fn encode_binding(binding: &ImportSourceBinding) -> Result<Vec<u8>> {
    let placement = &binding.placement;
    let serial = placement.library_serial.as_bytes();
    let barcode = placement.barcode.as_bytes();
    let serial_len =
        u32::try_from(serial.len()).map_err(|_| CatalogError::Invalid("带库序列号过长"))?;
    let barcode_len =
        u32::try_from(barcode.len()).map_err(|_| CatalogError::Invalid("磁带条码过长"))?;
    let mut out = Vec::with_capacity(108 + serial.len() + barcode.len());
    out.push(1);
    out.extend_from_slice(placement.pool_id.as_bytes());
    out.extend_from_slice(binding.scan.id.as_bytes());
    out.extend_from_slice(&binding.scan.generation.to_be_bytes());
    out.push(binding.source.location.partition as u8);
    out.extend_from_slice(&binding.source.location.start_block.to_be_bytes());
    out.extend_from_slice(&binding.source.sha256);
    out.extend_from_slice(&serial_len.to_be_bytes());
    out.extend_from_slice(serial);
    out.extend_from_slice(&barcode_len.to_be_bytes());
    out.extend_from_slice(barcode);
    Ok(out)
}

pub(super) fn decode_binding(
    namespace: Uuid,
    volume: Uuid,
    data: &[u8],
) -> Result<ImportSourceBinding> {
    if data.first() != Some(&1) {
        return Err(CatalogError::Corrupt("暂存来源记录版本"));
    }
    let mut at = 1;
    let pool_id = Uuid::from_slice(staged_import::take(data, &mut at, 16)?)
        .map_err(|_| CatalogError::Corrupt("暂存来源池 UUID"))?;
    let scan_id = Uuid::from_slice(staged_import::take(data, &mut at, 16)?)
        .map_err(|_| CatalogError::Corrupt("暂存来源扫描 UUID"))?;
    let generation = staged_import::take_u64(data, &mut at)?;
    let partition = staged_import::take(data, &mut at, 1)?[0] as char;
    let start_block = staged_import::take_u64(data, &mut at)?;
    let sha256 = staged_import::take(data, &mut at, 32)?
        .try_into()
        .expect("fixed length");
    let serial = staged_import::take_text(data, &mut at)?;
    let barcode = staged_import::take_text(data, &mut at)?;
    if at != data.len() || pool_id.is_nil() || scan_id.is_nil() || generation == 0 {
        return Err(CatalogError::Corrupt("暂存来源记录身份或尾部"));
    }
    Ok(ImportSourceBinding {
        placement: VolumePlacement {
            volume_id: volume,
            namespace,
            pool_id,
            library_serial: serial,
            barcode,
        },
        scan: VolumeScan {
            generation,
            id: scan_id,
        },
        source: ExpectedIndex {
            volume_uuid: volume,
            generation,
            location: IndexLocation {
                partition,
                start_block,
            },
            sha256,
        },
    })
}

fn sorted_sources(
    namespace: Uuid,
    bindings: &[ImportSourceBinding],
) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let mut sources = Vec::with_capacity(bindings.len());
    for binding in bindings {
        if binding.placement.namespace != namespace {
            return Err(CatalogError::Invalid("暂存来源命名空间不符"));
        }
        sources.push((
            source_key(namespace, binding.placement.volume_id),
            encode_binding(binding)?,
        ));
    }
    sources.sort_by(|left, right| left.0.cmp(&right.0));
    if sources.windows(2).any(|pair| pair[0].0 == pair[1].0) {
        return Err(CatalogError::Invalid("暂存来源卷 UUID 重复"));
    }
    Ok(sources)
}

fn digest_sources<'a>(sources: impl Iterator<Item = (&'a [u8], &'a [u8])>) -> [u8; 32] {
    let mut digest = sha2::Sha256::new();
    digest.update(SOURCES_DOMAIN);
    let mut count = 0u64;
    for (source_key, value) in sources {
        digest.update(source_key);
        digest.update((value.len() as u32).to_be_bytes());
        digest.update(value);
        count += 1;
    }
    digest.update(count.to_be_bytes());
    digest.finalize().into()
}

fn proof_write_plan(
    directory_saved: Option<&[u8]>,
    bound_saved: Option<&[u8]>,
    expected_staged: &[u8],
    expected_sources: &[u8],
) -> Result<(bool, bool)> {
    if directory_saved.is_some_and(|saved| saved != expected_staged) {
        return Err(CatalogError::Corrupt("暂存目录确认与来源证明冲突"));
    }
    if bound_saved.is_some_and(|saved| saved != expected_sources) {
        return Err(CatalogError::Corrupt("暂存来源确认与完成摘要冲突"));
    }
    Ok((directory_saved.is_none(), bound_saved.is_none()))
}

impl ManagedCatalog {
    pub(super) async fn verified_staged_binding(
        &self,
        namespace: Uuid,
        volume: Uuid,
    ) -> Result<ImportSourceBinding> {
        let tr = self.db.create_trx()?;
        let raw = tr
            .get(&source_key(namespace, volume), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let binding = decode_binding(namespace, volume, &raw)?;
        drop(tr);
        self.verify_import_source_binding(&binding).await?;
        Ok(binding)
    }

    pub(super) async fn stage_source_bindings(
        &self,
        namespace: Uuid,
        audit: DirectoryAudit,
        bindings: &[ImportSourceBinding],
    ) -> Result<()> {
        let sources = sorted_sources(namespace, bindings)?;
        let manifest = staged_import::manifest_key(namespace);
        let state = namespace_key(namespace);
        let managed = control::managed_key(namespace);
        let required = binding_required_key(namespace);
        let directory_proof = staged_import::confirmed_key(namespace);
        self.db
            .run(|tr, _| {
                let (manifest, state, managed, required, directory_proof) = (
                    manifest.clone(),
                    state.clone(),
                    managed.clone(),
                    required.clone(),
                    directory_proof.clone(),
                );
                async move {
                    if tr.get(&state, false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                        || tr.get(&managed, false).await?.as_deref() != Some(&[][..])
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let saved = tr.get(&manifest, false).await?;
                    let saved = match saved.as_deref().map(StagedImportSummary::decode) {
                        Some(Ok(saved)) if saved.audit == audit => saved,
                        Some(Err(error)) => return Ok(Err(error)),
                        _ => return Ok(Err(CatalogError::Incomplete)),
                    };
                    match tr.get(&required, false).await? {
                        Some(value) if value.as_ref() == REQUIRED_VERSION => Ok(Ok(())),
                        Some(_) => Ok(Err(CatalogError::Corrupt("来源绑定要求标记损坏"))),
                        None => {
                            if saved.complete || tr.get(&directory_proof, false).await?.is_some() {
                                return Ok(Err(CatalogError::Incomplete));
                            }
                            tr.set(&required, REQUIRED_VERSION);
                            Ok(Ok(()))
                        }
                    }
                }
            })
            .await??;
        for (source, value) in sources {
            self.db
                .run(|tr, _| {
                    let (source, value, manifest, state, managed) = (
                        source.clone(),
                        value.clone(),
                        manifest.clone(),
                        state.clone(),
                        managed.clone(),
                    );
                    async move {
                        if tr.get(&state, false).await?.as_deref()
                            != Some(&[NamespaceState::Building as u8][..])
                            || tr.get(&managed, false).await?.as_deref() != Some(&[][..])
                        {
                            return Ok(Err(CatalogError::Incomplete));
                        }
                        let saved = tr.get(&manifest, false).await?;
                        let saved = match saved.as_deref().map(StagedImportSummary::decode) {
                            Some(Ok(saved)) => saved,
                            Some(Err(error)) => return Ok(Err(error)),
                            None => return Ok(Err(CatalogError::Incomplete)),
                        };
                        if saved.audit != audit {
                            return Ok(Err(CatalogError::Incomplete));
                        }
                        match tr.get(&source, false).await? {
                            Some(saved) if saved.as_ref() == value => Ok(Ok(())),
                            Some(_) => Ok(Err(CatalogError::Corrupt("暂存来源绑定与重试不一致"))),
                            None => {
                                tr.set(&source, &value);
                                Ok(Ok(()))
                            }
                        }
                    }
                })
                .await??;
        }
        Ok(())
    }

    pub(super) async fn finish_source_bindings(
        &self,
        namespace: Uuid,
        staged: StagedImportSummary,
        bindings: &[ImportSourceBinding],
    ) -> Result<StagedSourceSummary> {
        let expected = sorted_sources(namespace, bindings)?;
        let prefix = source_prefix(namespace);
        let mut after = None;
        let mut ordinal = 0usize;
        loop {
            let tr = self.db.create_trx()?;
            let page = read_page(&tr, &prefix, after.as_deref(), SOURCE_PAGE, true).await?;
            for (source, value) in page.items {
                if expected.get(ordinal) != Some(&(source, value)) {
                    return Err(CatalogError::Corrupt("暂存来源卷集合与审计输入不一致"));
                }
                ordinal += 1;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        if ordinal != expected.len() {
            return Err(CatalogError::Incomplete);
        }
        let summary = StagedSourceSummary {
            staged,
            volumes: expected.len() as u64,
            sources_sha256: digest_sources(
                expected
                    .iter()
                    .map(|(key, value)| (key.as_slice(), value.as_slice())),
            ),
        };
        let summary_key = summary_key(namespace);
        let manifest = staged_import::manifest_key(namespace);
        let state = namespace_key(namespace);
        let managed = control::managed_key(namespace);
        let encoded = summary.encode();
        self.db
            .run(|tr, _| {
                let (summary_key, manifest, state, managed, required) = (
                    summary_key.clone(),
                    manifest.clone(),
                    state.clone(),
                    managed.clone(),
                    binding_required_key(namespace),
                );
                async move {
                    if tr.get(&state, false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                        || tr.get(&managed, false).await?.as_deref() != Some(&[][..])
                        || tr.get(&required, false).await?.as_deref() != Some(REQUIRED_VERSION)
                        || tr.get(&manifest, false).await?.as_deref()
                            != Some(staged.encode().as_slice())
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    match tr.get(&summary_key, false).await? {
                        Some(saved) if saved.as_ref() == encoded => Ok(Ok(())),
                        Some(_) => Ok(Err(CatalogError::Corrupt("暂存来源完成摘要冲突"))),
                        None => {
                            tr.set(&summary_key, &encoded);
                            Ok(Ok(()))
                        }
                    }
                }
            })
            .await??;
        Ok(summary)
    }

    /// Re-read every private source row after restart, compare the completed
    /// digest, and reject a source whose FDB ready scan or placement changed.
    /// This does not make staged files visible or grant media access.
    pub async fn verify_staged_source_bindings(
        &self,
        namespace: Uuid,
    ) -> Result<StagedSourceSummary> {
        self.verified_staged_source_map(namespace)
            .await
            .map(|(summary, _)| summary)
    }

    async fn verified_staged_source_map(
        &self,
        namespace: Uuid,
    ) -> Result<(StagedSourceSummary, SourceMap)> {
        let summary_key = summary_key(namespace);
        let tr = self.db.create_trx()?;
        let raw = tr
            .get(&summary_key, false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let summary = StagedSourceSummary::decode(&raw)?;
        if tr
            .get(&staged_import::manifest_key(namespace), false)
            .await?
            .as_deref()
            != Some(summary.staged.encode().as_slice())
            || tr
                .get(&binding_required_key(namespace), false)
                .await?
                .as_deref()
                != Some(REQUIRED_VERSION)
            || tr.get(&namespace_key(namespace), false).await?.as_deref()
                != Some(&[NamespaceState::Building as u8][..])
            || tr
                .get(&control::managed_key(namespace), false)
                .await?
                .as_deref()
                != Some(&[][..])
        {
            return Err(CatalogError::Incomplete);
        }
        drop(tr);
        let prefix = source_prefix(namespace);
        let mut after = None;
        let mut digest = sha2::Sha256::new();
        digest.update(SOURCES_DOMAIN);
        let mut count = 0u64;
        let mut source_map = SourceMap::new();
        loop {
            let tr = self.db.create_trx()?;
            let page = read_page(&tr, &prefix, after.as_deref(), SOURCE_PAGE, true).await?;
            for (source, value) in page.items {
                if source.len() != prefix.len() + 16 {
                    return Err(CatalogError::Corrupt("暂存来源键长度"));
                }
                let volume = uuid_from(&source[prefix.len()..])?;
                let binding = decode_binding(namespace, volume, &value)?;
                self.verify_import_source_binding(&binding).await?;
                if source_map
                    .insert(
                        binding.placement.barcode.clone(),
                        (binding.placement.pool_id, volume, binding.scan.generation),
                    )
                    .is_some()
                {
                    return Err(CatalogError::Corrupt("暂存来源条码重复"));
                }
                digest.update(&source);
                digest.update((value.len() as u32).to_be_bytes());
                digest.update(&value);
                count += 1;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        digest.update(count.to_be_bytes());
        if count != summary.volumes || <[u8; 32]>::from(digest.finalize()) != summary.sources_sha256
        {
            return Err(CatalogError::Incomplete);
        }
        let tr = self.db.create_trx()?;
        if tr.get(&summary_key, false).await?.as_deref() != Some(raw.as_ref())
            || tr
                .get(&binding_required_key(namespace), false)
                .await?
                .as_deref()
                != Some(REQUIRED_VERSION)
            || tr
                .get(&staged_import::manifest_key(namespace), false)
                .await?
                .as_deref()
                != Some(summary.staged.encode().as_slice())
            || tr.get(&namespace_key(namespace), false).await?.as_deref()
                != Some(&[NamespaceState::Building as u8][..])
        {
            return Err(CatalogError::Incomplete);
        }
        Ok((summary, source_map))
    }

    /// Confirm both the staged rows and every current source after an
    /// independent FDB reread. The proof remains private in Building state.
    pub async fn confirm_bound_staged_import(
        &self,
        namespace: Uuid,
    ) -> Result<StagedSourceSummary> {
        let (sources, source_map) = self.verified_staged_source_map(namespace).await?;
        let staged = self
            .verify_staged_import_inner(namespace, true, Some(&source_map))
            .await?;
        if staged != sources.staged {
            return Err(CatalogError::Incomplete);
        }
        let state = namespace_key(namespace);
        let managed = control::managed_key(namespace);
        let manifest = staged_import::manifest_key(namespace);
        let source_summary = summary_key(namespace);
        let directory_proof = staged_import::confirmed_key(namespace);
        let bound_proof = confirmed_key(namespace);
        let expected_staged = staged.encode();
        let expected_sources = sources.encode();
        self.db
            .run(|tr, _| {
                let (
                    state,
                    managed,
                    manifest,
                    source_summary,
                    directory_proof,
                    bound_proof,
                    required,
                ) = (
                    state.clone(),
                    managed.clone(),
                    manifest.clone(),
                    source_summary.clone(),
                    directory_proof.clone(),
                    bound_proof.clone(),
                    binding_required_key(namespace),
                );
                async move {
                    if tr.get(&state, false).await?.as_deref()
                        != Some(&[NamespaceState::Building as u8][..])
                        || tr.get(&managed, false).await?.as_deref() != Some(&[][..])
                        || tr.get(&required, false).await?.as_deref() != Some(REQUIRED_VERSION)
                        || tr.get(&manifest, false).await?.as_deref()
                            != Some(expected_staged.as_slice())
                        || tr.get(&source_summary, false).await?.as_deref()
                            != Some(expected_sources.as_slice())
                    {
                        return Ok(Err(CatalogError::Incomplete));
                    }
                    let directory_saved = tr.get(&directory_proof, false).await?;
                    let bound_saved = tr.get(&bound_proof, false).await?;
                    let (write_directory, write_bound) = match proof_write_plan(
                        directory_saved.as_deref(),
                        bound_saved.as_deref(),
                        &expected_staged,
                        &expected_sources,
                    ) {
                        Ok(plan) => plan,
                        Err(error) => return Ok(Err(error)),
                    };
                    if write_directory {
                        tr.set(&directory_proof, &expected_staged);
                    }
                    if write_bound {
                        tr.set(&bound_proof, &expected_sources);
                    }
                    Ok(Ok(()))
                }
            })
            .await??;
        Ok(sources)
    }

    /// Read a prior private proof. A caller planning a media read must still
    /// recheck that the chosen scan is currently ready and owner/PR permits it.
    pub async fn bound_staged_import_summary(
        &self,
        namespace: Uuid,
    ) -> Result<StagedSourceSummary> {
        let tr = self.db.create_trx()?;
        if tr.get(&namespace_key(namespace), false).await?.as_deref()
            != Some(&[NamespaceState::Building as u8][..])
            || tr
                .get(&binding_required_key(namespace), false)
                .await?
                .as_deref()
                != Some(REQUIRED_VERSION)
            || tr
                .get(&control::managed_key(namespace), false)
                .await?
                .as_deref()
                != Some(&[][..])
        {
            return Err(CatalogError::Incomplete);
        }
        let raw = tr
            .get(&confirmed_key(namespace), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if tr.get(&summary_key(namespace), false).await?.as_deref() != Some(raw.as_ref()) {
            return Err(CatalogError::Incomplete);
        }
        let summary = StagedSourceSummary::decode(&raw)?;
        if tr
            .get(&staged_import::manifest_key(namespace), false)
            .await?
            .as_deref()
            != Some(summary.staged.encode().as_slice())
            || tr
                .get(&staged_import::confirmed_key(namespace), false)
                .await?
                .as_deref()
                != Some(summary.staged.encode().as_slice())
        {
            return Err(CatalogError::Incomplete);
        }
        Ok(summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_row_is_exact_and_rejects_truncation() {
        let namespace = Uuid::new_v4();
        let volume = Uuid::new_v4();
        let binding = ImportSourceBinding {
            placement: VolumePlacement {
                volume_id: volume,
                namespace,
                pool_id: Uuid::new_v4(),
                library_serial: "SERIAL01".into(),
                barcode: "TAPE01L8".into(),
            },
            scan: VolumeScan {
                generation: 2,
                id: Uuid::new_v4(),
            },
            source: ExpectedIndex {
                volume_uuid: volume,
                generation: 2,
                location: IndexLocation {
                    partition: 'b',
                    start_block: 5,
                },
                sha256: [9; 32],
            },
        };
        let bytes = encode_binding(&binding).unwrap();
        assert_eq!(decode_binding(namespace, volume, &bytes).unwrap(), binding);
        for at in 0..bytes.len() {
            assert!(decode_binding(namespace, volume, &bytes[..at]).is_err());
        }
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_binding(namespace, volume, &trailing).is_err());
        let sources = sorted_sources(namespace, std::slice::from_ref(&binding)).unwrap();
        assert!(sorted_sources(namespace, &[binding.clone(), binding]).is_err());
        assert_ne!(
            digest_sources(
                sources
                    .iter()
                    .map(|(key, value)| (key.as_slice(), value.as_slice()))
            ),
            digest_sources(std::iter::empty::<(&[u8], &[u8])>()),
        );
    }

    #[test]
    fn proof_conflict_is_decided_before_either_write() {
        assert_eq!(
            proof_write_plan(None, None, b"stage", b"sources").unwrap(),
            (true, true)
        );
        assert_eq!(
            proof_write_plan(Some(b"stage"), None, b"stage", b"sources").unwrap(),
            (false, true)
        );
        assert!(proof_write_plan(None, Some(b"wrong"), b"stage", b"sources").is_err());
        assert!(proof_write_plan(Some(b"wrong"), None, b"stage", b"sources").is_err());
    }
}
