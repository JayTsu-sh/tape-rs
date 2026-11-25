//! Strict current MAM observation before the original media probe seals facts.
use super::OriginalAttemptProbeIo;
use crate::daemon::fdb_media::CapturedAttemptEvidence;
use crate::error::{Result, TapeError};
use crate::ltfs::mam::{self, AttributeFormat, Mam, VolumeCoherencyInfo};

pub(super) struct OriginalMam {
    vcr: Vec<u8>,
    ip: VolumeCoherencyInfo,
    dp: VolumeCoherencyInfo,
}
fn invalid(reason: &str) -> TapeError {
    TapeError::Refused {
        reason: format!("原KnownFull MAM未确认：{reason}"),
    }
}
fn binary(io: &OriginalAttemptProbeIo<'_>, partition: u8, id: u16) -> Result<Vec<u8>> {
    let attr = Mam::with_partition(io.transport(), partition)
        .read_attribute(id)?
        .ok_or_else(|| invalid("缺少原属性"))?;
    if attr.id != id || attr.format != AttributeFormat::Binary as u8 {
        return Err(invalid("属性ID/格式不匹配"));
    }
    Ok(attr.value.to_vec())
}
pub(super) fn read_original_mam(io: &OriginalAttemptProbeIo<'_>) -> Result<OriginalMam> {
    io.verify()?;
    let epoch = io
        .transport()
        .session_epoch()
        .ok_or_else(|| invalid("缺少session epoch"))?;
    let before = binary(io, 0, mam::ATTR_VCR)?;
    if !mam::vcr_is_valid(&before) {
        return Err(invalid("VCR无效"));
    }
    let read_vci = |partition| -> Result<VolumeCoherencyInfo> {
        let raw = binary(io, partition, mam::ATTR_VCI)?;
        let vci = VolumeCoherencyInfo::decode(&raw)?;
        if vci.encode()? != raw || !mam::vcr_is_valid(&vci.vcr) {
            return Err(invalid("VCI编码或VCR无效"));
        }
        Ok(vci)
    };
    let ip = read_vci(0)?;
    let dp = read_vci(1)?;
    let after = binary(io, 0, mam::ATTR_VCR)?;
    io.verify()?;
    if before != after || io.transport().session_epoch() != Some(epoch) {
        return Err(invalid("MAM读取期间VCR/session改变"));
    }
    Ok(OriginalMam {
        vcr: before,
        ip,
        dp,
    })
}
impl OriginalMam {
    /// Pure: no CDB/await after the media probe's final Some(epoch) seal.
    pub(super) fn require_facts(&self, facts: &CapturedAttemptEvidence) -> Result<()> {
        let report = facts.report();
        if facts.current().is_none()
            || report.ip_ahead()
            || !report.append_ok
            || report.append_position != Some(report.dp.eod)
            || !report.chain_ok
            || !report.history_checked
            || report.chain_truncated_by_budget
            || report.ip.budget_exhausted
            || report.dp.budget_exhausted
            || report.restricted_reason().is_some()
        {
            return Err(invalid("原完整Current/历史或追加边界不成立"));
        }
        for (scan, vci, partition) in [(&report.ip, &self.ip, 0), (&report.dp, &self.dp, 1)] {
            let last = scan
                .last_index
                .as_ref()
                .ok_or_else(|| invalid("缺少分区索引"))?;
            if scan.partition != partition
                || scan.tail != crate::ltfs::recovery::TailKind::Complete
                || last.end_block.checked_add(1) != Some(scan.eod)
                || vci.volume_uuid != facts.label().volume_uuid
                || last.index.volume_uuid != facts.label().volume_uuid
                || !mam::vcr_matches(&self.vcr, &vci.vcr)
                || vci.generation != last.index.generation
                || vci.block != last.start_block
                || vci.block != last.index.self_location.start_block
            {
                return Err(invalid("分区VCI不匹配实际卷/代/块/完整尾"));
            }
        }
        // Old, complete IP is legal debt; it need not share the DP generation.
        let ip = report.ip.last_index.as_ref().expect("checked");
        let dp = report.dp.last_index.as_ref().expect("checked");
        if ip.index.generation > dp.index.generation || dp.index.incremental {
            return Err(invalid("IP领先或DP不是原Full"));
        }
        Ok(())
    }
}
