//! Prepare a stable upload from one catalog read version, without admitting
//! work or changing either media or catalog. The executor selects the tape.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct ActiveUploadSpec {
    pub pool_id: Uuid,
    pub path: Vec<u8>,
    pub operation_id: Uuid,
    pub expected_version: Option<u64>,
    pub size: u64,
    pub digest: ContentDigest,
}

impl ManagedCatalog {
    /// Preflight only: admission must check this gate again in its transaction.
    pub async fn check_active_pool_gate(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        pool: Uuid,
    ) -> Result<()> {
        control::validate_physical_serial(serial)?;
        let tr = self.db.create_trx()?;
        let namespace = routing::require_runtime_active_owner(&tr, serial, route, owner).await?;
        control::require_pool_library(&tr, namespace, pool, serial).await?;
        control::require_pool_writable_at(&tr, namespace, pool).await
    }

    pub async fn prepare_active_upload(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner_epoch: u64,
        barcode: &str,
        spec: &ActiveUploadSpec,
    ) -> Result<ActiveFileIntent> {
        control::validate_physical_serial(serial)?;
        let tr = self.db.create_trx()?;
        let namespace =
            routing::require_runtime_active_owner(&tr, serial, route, owner_epoch).await?;
        control::require_pool_writable_at(&tr, namespace, spec.pool_id).await?;
        prepare_upload_at(&tr, namespace, serial, owner_epoch, barcode, spec).await
    }
}

// Callers must prove the write-side gate in this same transaction. Keeping
// construction here prevents maintenance requests from diverging from PUT.
pub(super) async fn prepare_upload_at(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    owner_epoch: u64,
    barcode: &str,
    spec: &ActiveUploadSpec,
) -> Result<ActiveFileIntent> {
    control::validate_physical_serial(serial)?;
    if spec.path.len() > 2048
        || spec.pool_id.is_nil()
        || spec.operation_id.is_nil()
        || !matches!(spec.digest, ContentDigest::Sha256(_))
        || spec.expected_version == Some(0)
        || spec.expected_version.unwrap_or(0).checked_add(1).is_none()
    {
        return Err(CatalogError::Invalid("上传准备身份、版本或摘要无效"));
    }
    let path = std::str::from_utf8(&spec.path)
        .map_err(|_| CatalogError::Invalid("上传路径必须是 UTF-8"))?;
    if !crate::ltfs::xml_text_valid(path) {
        return Err(CatalogError::Invalid("LTFS 写入路径包含 XML 1.0 非法字符"));
    }
    let catalog_path = pool_catalog_path(spec.pool_id, &spec.path)?;
    let parent_path = path.rsplit_once('/').expect("validated absolute path").0;
    let components = lookup::path_components(&catalog_path)?;
    let Some((&name, parents)) = components.split_last() else {
        return Err(CatalogError::Invalid("上传路径不能是根目录"));
    };
    if spec.path == b"/" {
        return Err(CatalogError::Invalid("上传路径不能是根目录"));
    }
    let assignment = ManagedCatalog::verified_assignment(tr, serial, barcode)
        .await?
        .filter(|a| a.namespace == namespace && a.pool_id == spec.pool_id)
        .ok_or(CatalogError::Invalid("磁带不属于目标活动池"))?;
    if !assignment.allows_io() {
        return Err(CatalogError::MediaUnavailable);
    }
    tape_monitor::require_data_writable(tr, namespace, serial, barcode).await?;
    let volume = assignment.volume_id.ok_or(CatalogError::Incomplete)?;
    let (scan, index) = active_volume::observed_at(tr, serial, volume).await?;
    let pool_raw = tr
        .get(&control::pool_key(namespace, spec.pool_id), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let pool = control::decode_pool(spec.pool_id, &pool_raw)?;
    if active_volume::file_count_at(tr, serial, volume).await? >= pool.file_limit {
        return Err(CatalogError::Incomplete);
    }

    let generation = index
        .generation
        .checked_add(1)
        .ok_or(CatalogError::Invalid("卷代数溢出"))?;
    if active_volume::recovery_active(tr, volume).await?
        || requests::prefix_has_rows(tr, &reservation::volume_prefix(volume, generation)).await?
        || requests::prefix_has_rows(tr, &reservation::upgraded_prefix(volume, generation)).await?
        || !active_batch::unsealed_or_absent_for_recovery(tr, volume, generation).await?
    {
        return Err(CatalogError::Incomplete);
    }
    let parent =
        match lookup::resolve_active_path_at_mode(tr, namespace, parents.to_vec(), false).await? {
            Some(ResolvedPath::Directory { id, .. }) => id,
            _ => return Err(CatalogError::Invalid("上传父目录不存在或不是目录")),
        };
    for claim in [
        reservation::path_key_for(namespace, parent, name),
        active_batch::path_claim_key_for(namespace, parent, name),
    ] {
        if tr.get(&claim, false).await?.is_some() {
            return Err(CatalogError::Incomplete);
        }
    }
    let current =
        lookup::resolve_active_path_at_mode(tr, namespace, components.clone(), false).await?;
    let window = requests::window(tr, namespace).await?;
    let mut fingerprint = Sha256::new();
    fingerprint.update(b"tape-rs/upload-inode/1");
    fingerprint.update(namespace.as_bytes());
    fingerprint.update(spec.operation_id.as_bytes());
    fingerprint.update(window.accepting.to_be_bytes());
    let file_id = match current {
        None if spec.expected_version.is_none() => {
            Uuid::from_bytes(fingerprint.finalize()[..16].try_into().expect("SHA prefix"))
        }
        Some(ResolvedPath::File {
            id,
            head,
            placement,
            copy,
            ..
        }) if spec.expected_version == Some(head.version)
            && placement.pool_id == spec.pool_id
            && !copy
                .media
                .as_ref()
                .is_some_and(|media| media.native_symlink) =>
        {
            id
        }
        _ => {
            return Err(CatalogError::Invalid(
                "路径已存在、版本不匹配、跨池或不是普通文件",
            ));
        }
    };
    let intent = ActiveFileIntent {
        barcode_constraint: Some(barcode.to_owned()),
        namespace,
        parent,
        parent_path: if parent_path.is_empty() {
            b"/".to_vec()
        } else {
            parent_path.as_bytes().to_vec()
        },
        name: name.to_vec(),
        file_id,
        expected_version: spec.expected_version,
        operation_id: spec.operation_id,
        request_epoch: window.accepting,
        library_serial: serial.into(),
        pool_id: Some(spec.pool_id),
        owner_epoch,
        volume_id: volume,
        volume_generation: generation,
        volume_scan_id: scan.id,
        expected_size: spec.size,
        expected_digest: spec.digest,
    };
    // Preparing the same operation after admission must never invent a
    // different target. Clients resume the identity persisted before PUT.
    for kind in *b"0IP" {
        let mut key = requests::epoch_prefix(kind, namespace, window.accepting);
        key.extend_from_slice(spec.operation_id.as_bytes());
        if tr.get(&key, false).await?.is_some() {
            return Err(CatalogError::Invalid(
                "请求已使用，请查询并重试保存的原请求",
            ));
        }
    }
    intent.ltfs_path()?;
    Ok(intent)
}
