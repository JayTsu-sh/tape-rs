//! FUSE/管理 CLI 共用的显式物理设备绑定参数。
use crate::error::{Result, TapeError};
use crate::library::{Library, LibraryDrive};
use crate::scsi::device::ScsiDevice;
use crate::scsi::inquiry::read_unit_serial;
use crate::scsi::reservation::ReservationKey;
use clap::Args;
use std::path::PathBuf;

#[derive(Args)]
pub struct LibraryArgs {
    /// 逻辑库 changer 设备（必须与 LE/ltfsd 互斥使用）
    #[arg(long,requires_all=["library_changer_serial","library_state","library_drive","library_round"])]
    pub library_changer: Option<String>,
    /// 预期 changer VPD 序列号
    #[arg(long, requires = "library_changer")]
    pub library_changer_serial: Option<String>,
    /// 允许调度的 drive，格式 SERIAL=/dev/sgN，可重复
    #[arg(long, requires = "library_changer")]
    pub library_drive: Vec<String>,
    /// 资源状态 SQLite 路径；禁止复用 ltfsd catalog
    #[arg(long, requires = "library_changer")]
    pub library_state: Option<PathBuf>,
    /// 独占执行节点号
    #[arg(long, default_value_t = 1)]
    pub library_node: u8,
    /// 运维分配的单调递增执行轮次（1..2^48），不得复用旧轮次
    #[arg(long, requires = "library_changer")]
    pub library_round: Option<u64>,
}
impl LibraryArgs {
    pub fn open(&self) -> Result<Library<ScsiDevice, ScsiDevice>> {
        let err = |s: &str| TapeError::Refused { reason: s.into() };
        let round = self.library_round.ok_or_else(|| err("必须指定执行轮次"))?;
        if round == 0 || round >= 1 << 48 {
            return Err(err("执行轮次超出 48 位范围"));
        }
        let changer = ScsiDevice::open(
            self.library_changer
                .as_deref()
                .ok_or_else(|| err("必须指定 changer"))?,
        )?;
        if read_unit_serial(&changer).as_deref() != self.library_changer_serial.as_deref() {
            return Err(err("changer 序列号不匹配"));
        }
        let mut mc = crate::changer::commands::MediumChanger::new(&changer);
        let map = mc.load_address_map()?.clone();
        let inventory = mc.read_all_status()?;
        let mut drives = Vec::new();
        for spec in &self.library_drive {
            let (serial, path) = spec
                .split_once('=')
                .ok_or_else(|| err("drive 格式必须为 SERIAL=/dev/sgN"))?;
            let entry = inventory
                .iter()
                .find(|e| {
                    e.element_type == crate::changer::element::ElementType::DataTransfer
                        && e.drive_id
                            .as_deref()
                            .is_some_and(|id| crate::library::drive_id_matches(id, serial))
                })
                .ok_or_else(|| err("drive 序列号不在 changer 库存中"))?;
            drives.push(LibraryDrive {
                offset: entry.address - map.dt_start,
                device: ScsiDevice::open(path)?,
            });
        }
        if drives.is_empty() {
            return Err(err("必须配置至少一个 drive"));
        }
        Library::open(
            changer,
            drives,
            self.library_state
                .as_deref()
                .ok_or_else(|| err("必须指定资源库"))?,
            ReservationKey::new(self.library_node, round),
        )
    }
}
