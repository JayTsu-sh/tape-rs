//! mkltfs：把一盘 raw LTO 带初始化成 LTFS 格式。
//!
//! 流程：
//! 1. 确认 TUR、REWIND、读 INQUIRY（可选）。
//! 2. 发送 MODE SELECT(10) Page 0x11 (Medium Partition Mode Page) 请求把介质
//!    划分成 2 个 partition（P0 = Index，P1 = Data）。我们用 **SDP=1** 让驱动器
//!    选择自己的默认 LTFS-friendly 划分（LTO-7+ 在此模式下会给 P0 一个 wrap
//!    ≈ 37.5 GiB，其余全给 P1）。
//! 3. FORMAT MEDIUM format=1（按 mode page 重新格式化）。
//! 4. 在 P0 写入 VOL1 → FM → LTFS Label → FM → Index XML → FM。
//! 5. 在 P1 写入 VOL1 → FM → LTFS Label → FM（数据区本身留空，首次 commit 才写 index）。
//! 6. 写 MAM 属性：Application Vendor / Name / Version / Barcode / VCI。
//!
//! **所有参数字节的含义都在注释里展开**——mkltfs 是容易写错的一步（写错会毁带），
//! 第一次在真带上执行前务必逐字节对照 SSC-5 Table 248 / LTFS 2.4 §9。

use log::info;
use uuid::Uuid;

use crate::error::{Result, TapeError};
use crate::scsi::device::ScsiDevice;
use crate::tape::commands::TapeDrive;

use super::index::{IndexLocation, LtfsIndex};
use super::label::{LtfsLabel, Vol1Label, PART_DATA, PART_INDEX};
use super::mam::{
    Mam, VolumeCoherencyInfo, ATTR_APP_FORMAT_VERSION, ATTR_APP_NAME, ATTR_APP_VENDOR,
    ATTR_APP_VERSION, ATTR_BARCODE,
};
use super::volume::{DEFAULT_BLOCK_SIZE, P0_INDEX_BLOCK, VOL1_BLOCK};

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
pub fn mkltfs(device: &ScsiDevice, opts: &MkltfsOptions) -> Result<Uuid> {
    if opts.volume_id.is_empty() || opts.volume_id.len() > 6 {
        return Err(TapeError::Ltfs(format!(
            "volume_id 长度非法: {}（必须 1..=6）",
            opts.volume_id.len()
        )));
    }
    if opts.block_size < 4096 || opts.block_size > 16 * 1024 * 1024 {
        return Err(TapeError::Ltfs(format!("block_size {} 超出合理范围 [4KiB, 16MiB]", opts.block_size)));
    }

    let drive = TapeDrive::new(device);
    let mam = Mam::new(device);
    let volume_uuid = opts.volume_uuid.unwrap_or_else(Uuid::new_v4);

    info!("mkltfs 开始: volume_id={} uuid={}", opts.volume_id, volume_uuid);

    // 1. TUR + REWIND
    drive.test_unit_ready()?;
    drive.rewind()?;

    if opts.quick {
        info!("quick 模式：跳过 MODE SELECT + FORMAT MEDIUM，假定磁带已是 LTFS 2-partition 布局");
    } else {
        // 2. 两分区 MODE SELECT(10) 页 0x11
        apply_two_partition_mode(&drive)?;

        // 3. FORMAT MEDIUM format=1（按 mode page 重新分区）
        //    耗时巨大（LTO-8 可能 > 1 小时），已在 TapeDrive::format 里给 8 小时超时。
        info!("格式化介质（2-partition）...");
        drive.format(1, /*verify=*/ false)?;
    }

    // 4. 写 P0 labels + 空 index
    write_partition_prologue(&drive, /*partition=*/ 0, &opts, volume_uuid, PART_INDEX)?;

    // P0 的 index XML
    let mut p0_index = LtfsIndex::empty(volume_uuid, APP_NAME.to_string(), PART_DATA);
    p0_index.self_location = IndexLocation { partition: PART_INDEX, start_block: P0_INDEX_BLOCK };
    let p0_xml = p0_index.to_xml()?;
    drive.locate(0, P0_INDEX_BLOCK, true)?;
    write_blocks(&drive, &p0_xml, opts.block_size as usize)?;
    drive.write_filemark(1)?;

    // 5. 写 P1 labels（数据区不写 index，首次 commit 才写）
    write_partition_prologue(&drive, /*partition=*/ 1, &opts, volume_uuid, PART_DATA)?;

    // 6. 写 MAM
    mam.write_ascii(ATTR_APP_VENDOR, APP_VENDOR, 8)?;
    mam.write_ascii(ATTR_APP_NAME, APP_NAME, 32)?;
    mam.write_ascii(ATTR_APP_VERSION, APP_VERSION, 8)?;
    mam.write_ascii(ATTR_BARCODE, &opts.volume_id, 32)?;
    // 0x080B: LTFS Application Format Version（ASCII 16 bytes，LTFS 2.4 Annex B.3.1）
    mam.write_ascii(ATTR_APP_FORMAT_VERSION, super::label::LTFS_VERSION, 16)?;
    // 0x080C: Volume Coherency Information（Binary 66 bytes，LTFS 2.4 Annex B.3.2）
    let vci = VolumeCoherencyInfo {
        vcr: 1,
        count: 0,
        generation: p0_index.generation,
        volume_uuid,
    };
    mam.write_vci(&vci)?;

    drive.rewind()?;
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
) -> Result<()> {
    drive.locate(partition, VOL1_BLOCK, true)?;
    let vol1 = Vol1Label::encode(&opts.volume_id, &opts.owner);
    drive.write_block(&vol1)?;
    drive.write_filemark(1)?;

    // 此处已是 LABEL_BLOCK（block 2）——LOCATE 省略
    let label = LtfsLabel {
        version: super::label::LTFS_VERSION.into(),
        creator: APP_NAME.into(),
        format_time: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
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
/// Mode parameter list:
/// ```text
/// Header (8 bytes): 全 0（Mode Data Length / Medium Type / Device-Specific / BDL 都写 0）
///
/// Page 0x11 (16-byte 整页，body 14 字节):
///   0  : Page Code   = 0x11
///   1  : Page Length = 0x0E  (body 2..15 共 14 字节)
///   2  : Max additional partitions (RO on SELECT, 写 0)
///   3  : Additional Partitions Defined = 0x01  → P0 + P1
///   4  : FDP=0 | SDP=0 | IDP=1 | PSUM=11b(GB) | POFM=1 | CLEAR=0 | ADDP=0 = 0x3C
///   5  : Media Format Recognition (RO, 写 0)
///   6  : Partition Units (RO, 写 0 → 用 PSUM 默认单位 GB)
///   7  : reserved
///   8-9  : P0 size = 0x0000  → "驱动器默认大小"（LTO-8 给 ≈ 37.5 GiB wrap）
///   10-11: P1 size = 0xFFFF  → "remainder of medium"
///   12-15: padding（驱动器 max=3，填 0）
/// ```
/// 总长 24 字节。
fn apply_two_partition_mode(drive: &TapeDrive<'_>) -> Result<()> {
    let params: [u8; 24] = [
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x11, // page code
        0x0E, // page length = 14
        0x00, // max additional (RO)
        0x01, // additional defined → 2 total
        0x3C, // IDP=1 | PSUM=11 GB | POFM=1
        0x00, // media format recognition (RO on select)
        0x00, // partition units (RO on select)
        0x00, // reserved
        0x00, 0x00, // P0 size = default (driver picks ≈37 GiB wrap)
        0xFF, 0xFF, // P1 size = max remainder
        0x00, 0x00, // pad
        0x00, 0x00, // pad
    ];
    info!("MODE SELECT page 0x11: 2-partition (IDP=1, PSUM=GB, P0=default, P1=max)");
    drive.mode_select(&params, /*save=*/ false)?;
    Ok(())
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
