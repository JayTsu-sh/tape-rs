//! mkltfs：把一盘 raw LTO 带初始化成 LTFS 格式。
//!
//! 流程：
//! 1. 确认 TUR、定位分区0起点。
//! 2. 发送 MODE SELECT(10) Page 0x11 (Medium Partition Mode Page) 请求把介质
//!    划分成 2 个 partition（P0 = Index，P1 = Data）。使用 IDP=1、GB 单位，
//!    P0 size=1 GB 由驱动器向上取整到最小分区，P1 size=0xffff 使用剩余容量。
//! 3. FORMAT MEDIUM format=1（按 mode page 重新格式化）。
//! 4. 在 P0 写入 VOL1 → FM → LTFS Label → FM → FM → Index XML → FM。
//! 5. 在 P1 写入 VOL1 → FM → LTFS Label → FM → FM → 初始 Index → FM。
//! 6. 写 MAM 属性：Application Vendor / Name / Version / Barcode / VCI。
//!
//! **所有参数字节的含义都在注释里展开**——mkltfs 是容易写错的一步（写错会毁带），
//! 第一次在真带上执行前务必逐字节对照 SSC-5 Table 248 / LTFS 2.4 §9。

use log::info;
use uuid::Uuid;

use crate::error::{Result, TapeError};
use crate::scsi::transport::TapeTransport;
use crate::tape::commands::TapeDrive;

use super::index::{IndexLocation, LtfsIndex};
use super::label::{LtfsLabel, PART_DATA, PART_INDEX, Vol1Label};
use super::mam::{
    ATTR_APP_FORMAT_VERSION, ATTR_APP_NAME, ATTR_APP_VENDOR, ATTR_APP_VERSION, ATTR_BARCODE, Mam,
    VolumeCoherencyInfo,
};
use super::volume::{
    DEFAULT_BLOCK_SIZE, FIRST_INDEX_BLOCK, P0_INDEX_BLOCK, P1_DATA_START, VOL1_BLOCK,
};

/// 应用端标识。写入 MAM 的 Application Vendor / Name / Version 字段。
pub const APP_VENDOR: &str = "tape-rs";
pub const APP_NAME: &str = "tape-rs ltfs";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// mkltfs 参数。
#[derive(Debug, Clone)]
pub struct MkltfsOptions {
    /// VOL1 和 LTFS label 的 Volume Identifier（一般 = barcode，截到 6 字符）。
    pub volume_id: String,
    /// VOL1 Owner Identifier（14 字符以内）。
    pub owner: String,
    /// 块大小（LTO 推荐 512 KiB 或 1 MiB）。
    pub block_size: u32,
    /// 是否启用 LTFS 逻辑压缩标记（仅记录在 label，不影响驱动器硬件压缩）。
    pub compression: bool,
    /// 可选：指定 volume UUID（不指定则随机）。
    pub volume_uuid: Option<Uuid>,
    /// 快速模式：跳过 MODE SELECT + FORMAT MEDIUM，仅重写 labels/index/MAM。
    /// 调用方需自行确保磁带已是 LTFS 2-partition 布局。
    pub quick: bool,
}

impl Default for MkltfsOptions {
    fn default() -> Self {
        Self {
            volume_id: "TAPERS".into(),
            owner: String::new(),
            block_size: DEFAULT_BLOCK_SIZE,
            compression: false,
            volume_uuid: None,
            quick: false,
        }
    }
}

/// 主入口。**破坏性操作**——会抹掉整盘磁带的内容。
pub fn mkltfs(device: &dyn TapeTransport, opts: &MkltfsOptions) -> Result<Uuid> {
    if opts.volume_id.is_empty() || opts.volume_id.len() > 6 {
        return Err(TapeError::Ltfs(format!(
            "volume_id 长度非法: {}（必须 1..=6）",
            opts.volume_id.len()
        )));
    }
    if opts.block_size < 4096 || opts.block_size > 16 * 1024 * 1024 {
        return Err(TapeError::Ltfs(format!(
            "block_size {} 超出合理范围 [4KiB, 16MiB]",
            opts.block_size
        )));
    }

    let drive = TapeDrive::new(device);
    let mam = Mam::new(device);
    let volume_uuid = opts.volume_uuid.unwrap_or_else(Uuid::new_v4);

    info!(
        "mkltfs 开始: volume_id={} uuid={}",
        opts.volume_id, volume_uuid
    );

    // 1. FORMAT 要求 BOP 0；REWIND 只回当前分区起点，不能用于这里。
    drive.test_unit_ready()?;
    drive.locate(0, 0, true)?;

    if opts.quick {
        info!("quick 模式：跳过 MODE SELECT + FORMAT MEDIUM，假定磁带已是 LTFS 2-partition 布局");
    } else {
        // 2. 两分区 MODE SELECT(10) 页 0x11
        apply_two_partition_mode(device)?;

        // 3. FORMAT MEDIUM format=1（按 mode page 重新分区）
        //    耗时巨大（LTO-8 可能 > 1 小时），已在 TapeDrive::format 里给 8 小时超时。
        info!("格式化介质（2-partition）...");
        drive.format(1, /*verify=*/ false)?;
    }

    // 两个分区的卷标除 <location> 外必须逐字段相同（LTFS 2.5.1 §8.1.2），formattime 只取一次。
    // IBM LTFS 会比较两份卷标，不同则拒绝加载（LTFS11184E）。
    let format_time = crate::ltfs::label::ltfs_time_now();

    // 4. 写 P0 labels + 空 index
    write_partition_prologue(
        &drive,
        /*partition=*/ 0,
        opts,
        volume_uuid,
        PART_INDEX,
        &format_time,
    )?;

    // P0 的 index XML
    let mut p0_index = LtfsIndex::empty(volume_uuid, APP_NAME.to_string(), PART_DATA);
    p0_index.self_location = IndexLocation {
        partition: PART_INDEX,
        start_block: P0_INDEX_BLOCK,
    };
    // IP 索引回指 DP 上同代的那份（一致卷的定义）；DP 首代没有回指针。
    let dp_first = IndexLocation {
        partition: PART_DATA,
        start_block: FIRST_INDEX_BLOCK,
    };
    let mut p0_written = p0_index.clone();
    p0_written.previous_location = Some(dp_first);
    let p0_xml = p0_written.to_xml()?;
    // Index Construct = FM + 索引 + FM（§5.2.3）
    drive.locate(0, P1_DATA_START, true)?;
    drive.write_filemark(1)?;
    write_blocks(&drive, &p0_xml, opts.block_size as usize)?;
    drive.write_filemark(1)?;

    // 5. 写 P1 labels + 同代空 index（LTFS 2.5.1 §5.3：完整分区的最后构造必须是 index）
    write_partition_prologue(
        &drive,
        /*partition=*/ 1,
        opts,
        volume_uuid,
        PART_DATA,
        &format_time,
    )?;
    let mut p1_index = p0_index.clone();
    p1_index.self_location = IndexLocation {
        partition: PART_DATA,
        start_block: FIRST_INDEX_BLOCK,
    };
    let p1_xml = p1_index.to_xml()?;
    drive.locate(1, P1_DATA_START, true)?;
    drive.write_filemark(1)?;
    write_blocks(&drive, &p1_xml, opts.block_size as usize)?;
    drive.write_filemark(1)?;

    // 6. 写 MAM
    mam.write_ascii(ATTR_APP_VENDOR, APP_VENDOR, 8)?;
    mam.write_ascii(ATTR_APP_NAME, APP_NAME, 32)?;
    mam.write_ascii(ATTR_APP_VERSION, APP_VERSION, 8)?;
    mam.write_ascii(ATTR_BARCODE, &opts.volume_id, 32)?;
    // 0x080B: LTFS Application Format Version（ASCII 16 bytes，LTFS 2.4 Annex B.3.1）
    mam.write_ascii(ATTR_APP_FORMAT_VERSION, super::label::LTFS_VERSION, 16)?;
    // 0x080C: Volume Coherency Information（LTFS 2.5.1 §10.3）：刷缓冲 → 读 VCR → 写两分区 VCI
    drive.write_filemark(0)?;
    match mam.read_vcr()? {
        Some(vcr) if super::mam::vcr_is_valid(&vcr) => {
            for (partition, block) in [(0u8, P0_INDEX_BLOCK), (1u8, FIRST_INDEX_BLOCK)] {
                let vci = VolumeCoherencyInfo {
                    vcr: vcr.to_vec(),
                    generation: p0_index.generation,
                    block,
                    volume_uuid,
                };
                Mam::with_partition(device, partition).write_vci(&vci)?;
            }
        }
        _ => info!("驱动器未提供有效 VCR，跳过 VCI"),
    }

    drive.locate(0, 0, true)?;
    info!("mkltfs 完成: {}", volume_uuid);
    Ok(volume_uuid)
}

/// 写入某个 partition 的开头：VOL1 → FM → LTFS Label XML → FM。
fn write_partition_prologue(
    drive: &TapeDrive<'_>,
    partition: u8,
    opts: &MkltfsOptions,
    volume_uuid: Uuid,
    location: char,
    format_time: &str,
) -> Result<()> {
    drive.locate(partition, VOL1_BLOCK, true)?;
    let vol1 = Vol1Label::encode(&opts.volume_id, &opts.owner);
    drive.write_block(&vol1)?;
    drive.write_filemark(1)?;

    // 此处已是 LABEL_BLOCK（block 2）——LOCATE 省略
    let label = LtfsLabel {
        version: super::label::LTFS_VERSION.into(),
        creator: APP_NAME.into(),
        format_time: format_time.to_string(),
        volume_uuid,
        location,
        index_partition: PART_INDEX,
        data_partition: PART_DATA,
        blocksize: opts.block_size,
        compression: opts.compression,
    };
    let xml = label.to_xml()?;
    write_blocks(drive, &xml, opts.block_size as usize)?;
    drive.write_filemark(1)?;
    Ok(())
}

/// 用 MODE SELECT(10) 把介质划成 2 个 partition。
///
/// 经验教训：
/// - SDP=1（让驱动器自选）被 TS4300 的 LTO-8 拒 `INVALID FIELD IN PARAMETER LIST`。
/// - 仅 1 个 2-byte size entry（page length=0x08）同样被拒。
/// - 驱动器 MODE SENSE 回读 page length=**0x0E**（4 个 size slot）、flags=**0x3c**
///   （IDP=1 | PSUM=11b GB | POFM=1），所以 MODE SELECT 必须匹配这个形状。
///
/// IBM GA32-0928-08 §6.6.13：保留不可更改字段，IDP 的 P0/P1 大小必须非零。
/// 与参考 LTFS tape_format 一致，P0=1 GB 向上取整到最小分区，P1=剩余空间。
fn apply_two_partition_mode(device: &dyn TapeTransport) -> Result<()> {
    let mut current = [0u8; 64];
    let cdb = crate::scsi::cdb::mode_sense_10(0x11, current.len() as u16);
    let result = crate::scsi::transport::retry_unit_attention("MODE SENSE 分区参数", || {
        device.execute_read(&cdb, &mut current, 30_000)
    })?;
    let data = current
        .get(..result.transferred)
        .ok_or(TapeError::InvalidResponse {
            expected: current.len(),
            actual: result.transferred,
        })?;
    let params = two_partition_parameters(data)?;
    info!("MODE SELECT page 0x11: 2-partition (IDP=1, unit=GB, P0=1GB/minimum, P1=max)");
    TapeDrive::new(device).mode_select(&params, false)
}

fn two_partition_parameters(current: &[u8]) -> Result<Vec<u8>> {
    let invalid = || TapeError::Ltfs("MODE SENSE 分区页缺失、截断或不支持两分区".into());
    if current.len() < 8 {
        return Err(invalid());
    }
    let end = usize::from(u16::from_be_bytes([current[0], current[1]])) + 2;
    if end > current.len() || end < 8 {
        return Err(invalid());
    }
    let start = 8 + usize::from(u16::from_be_bytes([current[6], current[7]]));
    let page = current.get(start..end).ok_or_else(invalid)?;
    if page.len() < 12 || page[0] & 0x7f != 0x11 {
        return Err(invalid());
    }
    let len = usize::from(page[1]) + 2;
    if len < 12 || len > page.len() || page[2] < 1 {
        return Err(invalid());
    }
    let mut params = vec![0; 8];
    // §6.6.1：Buffered Mode=1，Speed=0；省略 block descriptor，不改变块模式。
    params[3] = 0x10;
    params.extend_from_slice(&page[..len]);
    params[8] &= 0x7f;
    params[11] = 1;
    params[12] = 0x38 | (params[12] & 0x07); // IDP=1，PSUM=11；保留 POFM 等位。
    params[14] = 9; // partitioning type=0 自动选择流式；单位10^9字节。
    params[16..].fill(0);
    params[17] = 1;
    params[18..20].copy_from_slice(&[0xff, 0xff]);
    Ok(params)
}

fn write_blocks(drive: &TapeDrive<'_>, data: &[u8], block_size: usize) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    for chunk in data.chunks(block_size) {
        drive.write_block(chunk)?;
    }
    Ok(())
}

#[cfg(test)]
mod partition_tests {
    use super::two_partition_parameters;

    fn device_response() -> Vec<u8> {
        vec![
            0, 30, 0x98, 0x10, 0, 0, 0, 8, 0x60, 0, 0, 0, 0, 0, 0, 0, 0x11, 14, 3, 1, 0x3c, 3,
            0x19, 0, 0, 128, 0x44, 0xce, 0, 0, 0, 0,
        ]
    }

    #[test]
    fn partition_parameters_preserve_device_fields_and_use_nonzero_gb_size() {
        let p = two_partition_parameters(&device_response()).unwrap();
        assert_eq!(
            p,
            [
                0, 0, 0, 0x10, 0, 0, 0, 0, 0x11, 14, 3, 1, 0x3c, 3, 9, 0, 0, 1, 0xff, 0xff, 0, 0,
                0, 0
            ]
        );
    }

    #[test]
    fn malformed_partition_page_is_rejected_before_mode_select() {
        let response = device_response();
        for n in 0..response.len() {
            assert!(
                two_partition_parameters(&response[..n]).is_err(),
                "length {n}"
            );
        }
        for (offset, value) in [
            (0, 0xff),
            (7, 0xff),
            (16, 0x51),
            (17, 0xff),
            (17, 8),
            (18, 0),
        ] {
            let mut bad = response.clone();
            bad[offset] = value;
            assert!(two_partition_parameters(&bad).is_err(), "offset {offset}");
        }
    }
}
