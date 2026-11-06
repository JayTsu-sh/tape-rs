//! Bound physical attempts of immutable logical upload requests. A real owner
//! fence and write-before-data reservation precede appends. A full receipt is
//! derived only after LTFS assigns actual file UID and attributes; an uncertain
//! index commit remains fenced until media recovery proves its outcome.

use super::*;
use crate::ltfs::index::FileNode;
use crate::ltfs::volume::LtfsVolume;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveFileIntent {
    pub namespace: Uuid,
    pub parent: Uuid,
    /// Canonical LTFS path of the FDB parent directory, rooted at `/`.
    pub parent_path: Vec<u8>,
    pub name: Vec<u8>,
    pub file_id: Uuid,
    pub expected_version: Option<u64>,
    pub operation_id: Uuid,
    pub request_epoch: u64,
    pub library_serial: String,
    pub pool_id: Option<Uuid>,
    pub barcode_constraint: Option<String>,
    /// Actual execution fence; physical attempts require a nonzero owner.
    pub owner_epoch: u64,
    pub volume_id: Uuid,
    pub volume_generation: u64,
    pub volume_scan_id: Uuid,
    pub expected_size: u64,
    pub expected_digest: ContentDigest,
}

impl ActiveFileIntent {
    /// The exact LTFS path covered by this logical reservation. LTFS names
    /// must be UTF-8 even though FDB directory keys can contain raw bytes.
    pub fn ltfs_path(&self) -> Result<String> {
        validate_active_file_intent(self)?;
        let parent = std::str::from_utf8(&self.parent_path)
            .map_err(|_| CatalogError::Invalid("LTFS 父路径必须是 UTF-8"))?;
        let name = std::str::from_utf8(&self.name)
            .map_err(|_| CatalogError::Invalid("LTFS 文件名必须是 UTF-8"))?;
        Ok(if parent == "/" {
            format!("/{name}")
        } else {
            format!("{parent}/{name}")
        })
    }
}

#[derive(Clone, Debug)]
pub struct PreparedActiveFile {
    pub receipt: CommitReceipt,
    pub attributes: NodeAttributes,
}

/// Derive the FDB receipt from the exact file now present in the LTFS working
/// index. The caller must already hold the drive PR and must register/stage the
/// receipt before committing the index. A failed FDB step must not commit it.
pub fn prepare_active_file(
    volume: &LtfsVolume<'_>,
    path: &str,
    intent: &ActiveFileIntent,
) -> Result<PreparedActiveFile> {
    if volume.label().volume_uuid != intent.volume_id
        || volume.index().generation.checked_add(1) != Some(intent.volume_generation)
    {
        return Err(CatalogError::Invalid(
            "LTFS 卷身份或目标代数与写前预约不一致",
        ));
    }
    let file = volume
        .pending_file_node(path)
        .ok_or(CatalogError::Invalid("LTFS 工作索引中没有未提交的目标文件"))?;
    prepare_file_node(path, intent, file)
}

/// Rebuild the exact receipt from a PR-verified committed index during owner
/// takeover. This must match the sealed I receipt before FDB observes S4.
pub fn prepare_committed_active_file(
    volume: &LtfsVolume<'_>,
    path: &str,
    intent: &ActiveFileIntent,
) -> Result<PreparedActiveFile> {
    if volume.label().volume_uuid != intent.volume_id
        || volume.index().generation != intent.volume_generation
    {
        return Err(CatalogError::Invalid("已提交 LTFS 索引与目标代数不一致"));
    }
    let file = volume
        .index()
        .find_file(path)
        .ok_or(CatalogError::Invalid("已提交 LTFS 索引中没有目标文件"))?;
    prepare_file_node(path, intent, file)
}

fn prepare_file_node(
    path: &str,
    intent: &ActiveFileIntent,
    file: &FileNode,
) -> Result<PreparedActiveFile> {
    let expected_path = intent.ltfs_path()?;
    if path != expected_path {
        return Err(CatalogError::Invalid(
            "LTFS 完整路径与 FDB 父目录/文件名不一致",
        ));
    }
    if file.length != intent.expected_size {
        return Err(CatalogError::Invalid("LTFS 写入长度与上传收据不一致"));
    }
    if intent.expected_digest == ContentDigest::Unknown
        || ContentDigest::from_ltfs_xattrs(&file.xattrs)? != intent.expected_digest
    {
        return Err(CatalogError::Invalid("LTFS 内容摘要与上传收据不一致"));
    }
    let attributes = NodeAttributes::from_file(file)?;
    let mut receipt = CommitReceipt {
        namespace: intent.namespace,
        parent: intent.parent,
        name: intent.name.clone(),
        file_id: intent.file_id,
        expected_version: intent.expected_version,
        operation_id: intent.operation_id,
        request_epoch: intent.request_epoch,
        copy_id: Uuid::nil(),
        library_serial: intent.library_serial.clone(),
        pool_id: intent.pool_id,
        owner_epoch: intent.owner_epoch,
        volume_id: intent.volume_id,
        volume_generation: intent.volume_generation,
        volume_scan_id: intent.volume_scan_id,
        ltfs_file_uid: file.meta.file_uid,
        size: file.length,
        content_digest: intent.expected_digest,
        metadata_digest: Some(attributes.digest()),
    };
    receipt.copy_id = derive_copy_id(&receipt);
    validate_receipt(&receipt)?;
    Ok(PreparedActiveFile {
        receipt,
        attributes,
    })
}

pub(super) fn validate_active_file_intent(intent: &ActiveFileIntent) -> Result<()> {
    intent.request().validate()?;
    if intent.owner_epoch == 0 {
        return Err(CatalogError::Invalid("活动文件物理尝试尚未绑定执行 owner"));
    }
    validate_scan(
        intent.volume_id,
        VolumeScan {
            generation: intent.volume_generation,
            id: intent.volume_scan_id,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
    use crate::scsi::sim::{SimCartridge, SimLibrary};
    use sha2::{Digest, Sha256};
    use std::io::Cursor;

    #[test]
    fn upload_paths_reject_xml_forbidden_characters_before_admission() {
        let mut intent = ActiveFileIntent {
            barcode_constraint: None,
            namespace: Uuid::new_v4(),
            parent: Uuid::nil(),
            parent_path: b"/".to_vec(),
            name: b"good".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            library_serial: "SIM".into(),
            pool_id: Some(Uuid::new_v4()),
            owner_epoch: 1,
            volume_id: Uuid::new_v4(),
            volume_generation: 2,
            volume_scan_id: Uuid::new_v4(),
            expected_size: 3,
            expected_digest: ContentDigest::Sha256([1; 32]),
        };
        intent.owner_epoch = 0;
        assert!(intent.ltfs_path().is_err());
        assert_eq!(intent.request().ltfs_path().unwrap(), "/good");
        assert!(
            validate_active_file_intent(&intent).is_err(),
            "logical identity grants no execution permission"
        );
        intent.owner_epoch = 1;
        assert!(validate_active_file_intent(&intent).is_ok());
        intent.expected_version = Some(0);
        assert!(intent.ltfs_path().is_err());
        intent.expected_version = None;
        for bad in ["bad\u{1}", "bad\u{fffe}", "bad\u{ffff}"] {
            intent.name = bad.as_bytes().to_vec();
            assert!(
                intent.ltfs_path().is_err(),
                "非法 XML 文件名不能进入写前预约"
            );
            intent.name = b"good".to_vec();
            intent.parent_path = format!("/{bad}").into_bytes();
            assert!(
                intent.ltfs_path().is_err(),
                "非法 XML 父路径不能进入写前预约"
            );
            intent.parent_path = b"/".to_vec();
        }
        intent.name = "空 格 & < > \" 🧵".as_bytes().to_vec();
        assert!(
            intent.ltfs_path().is_ok(),
            "合法 XML 特殊字符由索引 writer 转义"
        );
    }

    #[test]
    fn real_simulated_append_yields_exact_precommit_receipt() {
        let library = SimLibrary::new(1, 4, 1);
        library
            .insert_cartridge(SimCartridge::blank("FDB001L8", 64 << 20), 0)
            .unwrap();
        library.load_into_drive("FDB001L8", 0).unwrap();
        let drive = library.drive(0);
        mkltfs(
            &drive,
            &MkltfsOptions {
                volume_id: "FDB001".into(),
                block_size: 64 * 1024,
                ..Default::default()
            },
        )
        .unwrap();
        let mut volume = LtfsVolume::mount(&drive).unwrap();
        let bytes = b"FDB physical receipt";
        let digest = ContentDigest::Sha256(Sha256::digest(bytes).into());
        let intent = ActiveFileIntent {
            barcode_constraint: None,
            namespace: Uuid::new_v4(),
            parent: Uuid::new_v4(),
            parent_path: b"/nested".to_vec(),
            name: b"sample".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            library_serial: "SIM-CHANGER".into(),
            pool_id: Some(Uuid::new_v4()),
            owner_epoch: 7,
            volume_id: volume.label().volume_uuid,
            volume_generation: volume.index().generation + 1,
            volume_scan_id: Uuid::new_v4(),
            expected_size: bytes.len() as u64,
            expected_digest: digest,
        };
        assert_eq!(intent.ltfs_path().unwrap(), "/nested/sample");
        let mut oversized = intent.clone();
        oversized.parent_path = format!("/{}", vec!["p".repeat(250); 8].join("/")).into_bytes();
        oversized.name = vec![b'n'; 50];
        assert!(oversized.ltfs_path().is_err());
        assert!(prepare_active_file(&volume, "/nested/sample", &intent).is_err());
        volume
            .append_file("/nested/sample", &mut Cursor::new(bytes))
            .unwrap();
        let prepared = prepare_active_file(&volume, "/nested/sample", &intent).unwrap();
        assert_eq!(prepared.receipt.volume_id, volume.label().volume_uuid);
        assert_eq!(
            prepared.receipt.volume_generation,
            volume.index().generation + 1
        );
        assert_eq!(prepared.receipt.content_digest, digest);
        let mut invalid = prepared.receipt.clone();
        invalid.expected_version = Some(0);
        invalid.copy_id = derive_copy_id(&invalid);
        assert!(
            validate_receipt(&invalid).is_err(),
            "zero previous must not alias an original create receipt"
        );
        assert_eq!(
            prepared.receipt.ltfs_file_uid,
            prepared.attributes.meta().unwrap().file_uid
        );
        assert_eq!(prepared.receipt.copy_id, derive_copy_id(&prepared.receipt));
        let mut wrong = intent.clone();
        wrong.expected_size += 1;
        assert!(prepare_active_file(&volume, "/nested/sample", &wrong).is_err());
        wrong = intent.clone();
        wrong.expected_digest = ContentDigest::Sha256([0; 32]);
        assert!(prepare_active_file(&volume, "/nested/sample", &wrong).is_err());
        wrong = intent.clone();
        wrong.volume_id = Uuid::new_v4();
        assert!(prepare_active_file(&volume, "/nested/sample", &wrong).is_err());
        wrong = intent.clone();
        wrong.volume_generation += 1;
        assert!(prepare_active_file(&volume, "/nested/sample", &wrong).is_err());
        wrong = intent.clone();
        wrong.parent_path = b"/".to_vec();
        assert!(prepare_active_file(&volume, "/nested/sample", &wrong).is_err());
        volume.sync_batch().unwrap();
        assert!(prepare_active_file(&volume, "/nested/sample", &intent).is_err());
    }
}
