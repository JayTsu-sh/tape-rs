//! Shared empty-volume initialization for guarded native lifecycle tasks.
//! The caller must persist the exact output UUID and Formatting phase before
//! entry. A recovered or attempted operation is observation-only, even if
//! partial labels cannot prove completion.
use super::*;
use crate::managed_catalog::ActiveIndexIdentity;

fn refused(detail: impl Into<String>) -> TapeError {
    TapeError::Refused {
        reason: detail.into(),
    }
}

/// Verify the exact original empty-volume format on both partitions. Ordinary mount reads only
/// the IP label; that alone cannot certify a newly formatted two-partition volume.
fn verify_labels(device: &dyn TapeTransport, expected: uuid::Uuid, barcode: &str) -> Result<()> {
    use crate::ltfs::label::{
        LTFS_VERSION, LtfsLabel, PART_DATA, PART_INDEX, VOL1_IMPL_ID, Vol1Label,
    };
    let drive = TapeDrive::new(device);
    let mut buffer = vec![0; 64 * 1024];
    let mut canonical = None;
    for (partition, location) in [(0, PART_INDEX), (1, PART_DATA)] {
        drive.locate(partition, crate::ltfs::volume::VOL1_BLOCK, true)?;
        let n = drive.read_block(&mut buffer)?;
        let record = buffer.get(..n).ok_or(TapeError::InvalidResponse {
            expected: buffer.len(),
            actual: n,
        })?;
        let vol1 = Vol1Label::parse(record)?;
        if n != 80
            || &buffer[24..37] != VOL1_IMPL_ID
            || vol1.volume_id != barcode.chars().take(6).collect::<String>()
            || !vol1.owner.is_empty()
        {
            return Err(refused(format!("分区{partition}的VOL1与原格式化参数不符")));
        }
        if drive.read_block(&mut buffer)? != 0 {
            return Err(refused(format!("分区{partition}的VOL1缺少闭合文件标记")));
        }
        drive.locate(partition, crate::ltfs::volume::LABEL_BLOCK, true)?;
        let n = drive.read_block(&mut buffer)?;
        let record = buffer.get(..n).ok_or(TapeError::InvalidResponse {
            expected: buffer.len(),
            actual: n,
        })?;
        let mut label = LtfsLabel::parse(record)?;
        if label.volume_uuid != expected
            || label.location != location
            || label.index_partition != PART_INDEX
            || label.data_partition != PART_DATA
            || label.version != LTFS_VERSION
            || label.creator != crate::ltfs::mkltfs::APP_NAME
            || label.blocksize != crate::ltfs::volume::DEFAULT_BLOCK_SIZE
            || label.compression
        {
            return Err(refused(format!(
                "分区{partition}的LTFS标签与原卷UUID或格式参数不符"
            )));
        }
        if drive.read_block(&mut buffer)? != 0 {
            return Err(refused(format!(
                "分区{partition}的LTFS标签缺少闭合文件标记"
            )));
        }
        label.location = PART_INDEX;
        let normalized = label.to_xml()?;
        if canonical.as_ref().is_some_and(|first| first != &normalized) {
            return Err(refused("两分区LTFS标签公共字段不一致"));
        }
        canonical = Some(normalized);
    }
    Ok(())
}

pub(super) fn initialize_empty(
    device: &dyn TapeTransport,
    permission: &Permission,
    key: ReservationKey,
    expected: uuid::Uuid,
    barcode: &str,
    attempted: &mut bool,
) -> Result<ActiveIndexIdentity> {
    if device.execution_guard() != Some((key.round(), permission as *const Permission as usize)) {
        return Err(TapeError::OwnershipLost {
            device: device.identity().into(),
            detail: "空卷初始化缺少原执行实例的多数派许可守护".into(),
        });
    }
    if expected.is_nil() {
        return Err(refused("空卷初始化须固定非空原输出UUID"));
    }
    if !*attempted {
        *attempted = true;
        crate::ltfs::mkltfs::mkltfs(
            device,
            &crate::ltfs::mkltfs::MkltfsOptions {
                volume_id: barcode.chars().take(6).collect(),
                volume_uuid: Some(expected),
                ..Default::default()
            },
        )?;
    }
    TapeDrive::new(device).test_unit_ready()?;
    verify_labels(device, expected, barcode)?;
    let mut volume = crate::ltfs::volume::LtfsVolume::mount(device)?;
    volume.set_reservation_guard(Some(key));
    let recovery = volume.recovery();
    if !volume.writable()
        || volume.label().volume_uuid != expected
        || !volume.index().root.files.is_empty()
        || !volume.index().root.subdirs.is_empty()
        || recovery.ip_debt
        || recovery.ip_ahead()
    {
        return Err(refused(
            "原格式化结果未证明完整空卷，保留原UUID/检查点，禁止再次FORMAT",
        ));
    }
    let proof: ActiveIndexIdentity = volume.verified_committed_index_construct()?.into();
    if proof.volume_uuid != expected || proof.generation != 1 || proof.incremental {
        return Err(refused("原格式化卷UUID或初代索引不符，禁止再次FORMAT"));
    }
    Ok(proof)
}

#[cfg(test)]
#[path = "executor_fdb_initialize_tests.rs"]
mod tests;
