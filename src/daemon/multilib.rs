//! 一个 Raft 日志管理多个逻辑库。每个库独立获得多数派许可、PR 隔离和执行线程。
//! 网关也通过同一日志获得有时效的许可，但不接触 SCSI；VIP 不随某个库的可用性撤销。
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    mpsc::{self, Receiver, RecvTimeoutError, Sender},
};
use std::time::{Duration, Instant};

use protobuf::Message as _;
use raft::eraftpb::EntryType;
use raft::{Config, RawNode, StateRole, Storage as _};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use slog::Drain;

use super::coordinator::Coordinator;
use super::directory::Directory;
use super::executor::{DeviceProvider, ExecEvent, ExecRequest};
use super::files::FileService;
use super::net::{AdminReply, DirectoryFetch, Network, NodeInput};
use super::node::{CoordinationConfig, NodeConfig, SharedStatus, on_ready, publish_status};
use super::state::{Applied, Command, ControlSnapshot, ControlState};
use super::store::RaftStore;
use crate::error::{Result, TapeError};

pub const GATEWAY: &str = "@gateway";

fn invalid(s: impl Into<String>) -> TapeError {
    TapeError::Ltfs(s.into())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryConfig {
    pub changer_serial: String,
    pub changer_target_port: Option<String>,
    pub drive_serials: Vec<String>,
}

impl LibraryConfig {
    fn ensure_same_changer_port(&self, next: &Self) -> Result<()> {
        if self.changer_target_port != next.changer_target_port {
            return Err(invalid("离线库/驱动变更不得隐式改变换带器 FC 预留域"));
        }
        Ok(())
    }

    pub fn http_prefix(&self) -> String {
        format!(
            "/libraries/{}",
            crate::client::encode_query(&self.changer_serial)
        )
    }
}

#[derive(Debug, Clone)]
pub struct LibrariesConfig {
    pub default_library: String,
    pub libraries: Vec<LibraryConfig>,
    /// 显式离线扩容允许旧快照暂时不含这些新库；正常启动时为空。
    additions: BTreeSet<String>,
    addition_floors: BTreeMap<String, u64>,
    drive_replacements: Vec<DriveReplacement>,
    drive_removals: Vec<DriveRemoval>,
}

#[derive(Debug, Clone)]
struct DriveReplacement {
    library: String,
    old: String,
    new: String,
}

#[derive(Debug, Clone)]
struct DriveRemoval {
    library: String,
    serial: String,
}

impl LibrariesConfig {
    pub fn parse(value: &Value) -> Result<Self> {
        let default_library = value["default_library"]
            .as_str()
            .ok_or_else(|| invalid("多库配置需要 default_library"))?
            .to_owned();
        let rows = value["libraries"]
            .as_array()
            .ok_or_else(|| invalid("多库配置需要 libraries 数组"))?;
        let mut ids = BTreeSet::new();
        let mut devices = BTreeSet::new();
        let mut libraries = Vec::new();
        for row in rows {
            let changer_serial = row["changer_serial"].as_str().unwrap_or("").to_owned();
            if row.get("id").is_some() {
                return Err(invalid(
                    "不支持带库别名 id；请使用 changer_serial 作为唯一带库标识",
                ));
            }
            let id = changer_serial.clone();
            let changer_target_port = row
                .get("changer_target_port")
                .map(|port| {
                    let port = port
                        .as_str()
                        .ok_or_else(|| invalid("changer_target_port 必须是 FC WWPN 字符串"))?;
                    crate::scsi::inquiry::normalize_fc_target_port(port)
                })
                .transpose()?;
            if id.is_empty()
                || id.len() > 64
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || !ids.insert(id.clone())
            {
                return Err(invalid(
                    "带库序列号必须唯一，且仅含 1..64 个字母、数字、- 或 _",
                ));
            }
            let mut drive_serials = row["drive_serials"]
                .as_array()
                .ok_or_else(|| invalid(format!("库 {id} 缺少 drive_serials")))?
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| invalid("驱动序列号必须是字符串"))
                })
                .collect::<Result<Vec<_>>>()?;
            if changer_serial.trim().is_empty() || drive_serials.is_empty() {
                return Err(invalid(format!("库 {id} 需要换带器和至少一台驱动")));
            }
            drive_serials.sort();
            for serial in std::iter::once(&changer_serial).chain(&drive_serials) {
                if serial.trim().is_empty()
                    || serial.trim() != serial
                    || !devices.insert(serial.clone())
                {
                    return Err(invalid(format!(
                        "设备序列号为空、含首尾空白或跨库重复：{serial}"
                    )));
                }
            }
            libraries.push(LibraryConfig {
                changer_serial,
                changer_target_port,
                drive_serials,
            });
        }
        if !ids.contains(&default_library) {
            return Err(invalid("default_library 必须引用已配置的库"));
        }
        libraries.sort_by(|a, b| a.changer_serial.cmp(&b.changer_serial));
        Ok(Self {
            default_library,
            libraries,
            additions: BTreeSet::new(),
            addition_floors: BTreeMap::new(),
            drive_replacements: Vec::new(),
            drive_removals: Vec::new(),
        })
    }

    pub fn resolve_library(&self, selector: &str) -> Option<&LibraryConfig> {
        self.libraries.iter().find(|l| l.changer_serial == selector)
    }

    pub fn value(&self) -> Value {
        json!({"default_library":self.default_library,"libraries":self.libraries.iter().map(|l| {
            let mut row = json!({"changer_serial":l.changer_serial,"drive_serials":l.drive_serials});
            if let Some(port) = &l.changer_target_port {
                row["changer_target_port"] = json!(port);
            }
            row
        }).collect::<Vec<_>>()})
    }

    /// 默认库沿用原单库文件，其余库的文件按稳定 id 隔离。
    pub fn library_dir(&self, root: &Path, id: &str) -> PathBuf {
        if id == self.default_library {
            root.to_path_buf()
        } else {
            root.join("libraries").join(id)
        }
    }

    pub fn check_manifest(&self, root: &Path) -> Result<()> {
        let path = root.join("libraries.json");
        let value = self.value();
        if path.exists() {
            let old: Value = serde_json::from_slice(&std::fs::read(&path)?)
                .map_err(|e| invalid(format!("库配置记录损坏：{e}")))?;
            if old != value {
                return Err(invalid(
                    "持久化多库配置与启动配置不一致；禁止隐式重映射库或设备",
                ));
            }
        } else {
            let tmp = path.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_vec(&value).unwrap())?;
            std::fs::rename(tmp, path)?;
        }
        Ok(())
    }

    /// 仅在全体节点停机后逐节点执行。保留旧库/驱动身份；新增库从空状态开始。
    pub fn add_libraries_offline(&self, root: &Path) -> Result<u64> {
        let manifest = root.join("libraries.json");
        let old_value: Value = serde_json::from_slice(&std::fs::read(&manifest)?)
            .map_err(|e| invalid(format!("原带库配置损坏：{e}")))?;
        let old = Self::parse(&old_value)?.load_additions(root)?;
        self.reject_removed_serial_reuse(&old)?;
        if old.default_library != self.default_library {
            return Err(invalid("离线扩容不得变更默认带库"));
        }
        let mut additions = BTreeSet::new();
        let mut drive_additions = BTreeMap::<String, BTreeSet<String>>::new();
        for previous in &old.libraries {
            let Some(next) = self.resolve_library(&previous.changer_serial) else {
                return Err(invalid(format!(
                    "离线扩容不得移除或更换旧库设备：{}",
                    previous.changer_serial
                )));
            };
            previous.ensure_same_changer_port(next)?;
            let before: BTreeSet<_> = previous.drive_serials.iter().cloned().collect();
            let after: BTreeSet<_> = next.drive_serials.iter().cloned().collect();
            if !before.is_subset(&after) {
                return Err(invalid(format!(
                    "离线扩容不得移除或更换旧库驱动：{}",
                    previous.changer_serial
                )));
            }
            let added = after.difference(&before).cloned().collect::<BTreeSet<_>>();
            if !added.is_empty() {
                drive_additions.insert(previous.changer_serial.clone(), added);
            }
        }
        for library in &self.libraries {
            if old.resolve_library(&library.changer_serial).is_none() {
                let directory = self.library_dir(root, &library.changer_serial);
                match directory.symlink_metadata() {
                    Ok(metadata)
                        if metadata.is_dir()
                            && !metadata.file_type().is_symlink()
                            && directory.read_dir()?.next().is_none() => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(_) => {
                        return Err(invalid(format!(
                            "新库 {} 的数据目录不是空目录，拒绝覆盖",
                            library.changer_serial
                        )));
                    }
                    Err(e) => return Err(e.into()),
                }
                additions.insert(library.changer_serial.clone());
            }
        }
        if additions.is_empty() && drive_additions.is_empty() {
            return Err(invalid("没有待新增的带库或驱动器"));
        }
        let floor = committed_log_floor(root)?;
        let step = json!({"from":old.value(),"to":self.value(),"additions":additions,"drive_additions":drive_additions,"before_index":floor});
        let marker = root.join("libraries-addition.json");
        let mut steps = if marker.exists() {
            let current: Value = serde_json::from_slice(&std::fs::read(&marker)?)
                .map_err(|e| invalid(format!("扩容记录损坏：{e}")))?;
            current["steps"]
                .as_array()
                .cloned()
                .ok_or_else(|| invalid("扩容记录缺少步骤"))?
        } else {
            Vec::new()
        };
        if let Some(last) = steps.last() {
            if last == &step {
                // 上次在记录落盘后、manifest 更新前中断，直接完成原步骤。
            } else if last["to"] == old.value() {
                steps.push(step);
            } else {
                return Err(invalid("已有未完成或不同的扩容记录"));
            }
        } else {
            steps.push(step);
        }
        atomic_json(&marker, &json!({"steps":steps}))?;
        atomic_json(&manifest, &self.value())?;
        Ok(floor)
    }

    /// 全体节点停机后逐节点执行；只允许同一库的一台驱动换成一台新序列号。
    /// 旧管理策略在日志回放时迁到新驱动，旧库存观测在变更索引前失效。
    pub fn replace_drive_offline(
        &self,
        root: &Path,
        old_serial: &str,
        new_serial: &str,
    ) -> Result<u64> {
        let manifest = root.join("libraries.json");
        let old_value: Value = serde_json::from_slice(&std::fs::read(&manifest)?)
            .map_err(|e| invalid(format!("原带库配置损坏：{e}")))?;
        let old = Self::parse(&old_value)?.load_additions(root)?;
        self.reject_removed_serial_reuse(&old)?;
        if old.default_library != self.default_library
            || old.libraries.len() != self.libraries.len()
        {
            return Err(invalid("更换驱动不得改变默认库或库集合"));
        }
        let mut replacements = BTreeMap::<String, BTreeMap<String, String>>::new();
        for previous in &old.libraries {
            let next = self
                .resolve_library(&previous.changer_serial)
                .ok_or_else(|| invalid("更换驱动不得移除带库"))?;
            previous.ensure_same_changer_port(next)?;
            let before: BTreeSet<_> = previous.drive_serials.iter().cloned().collect();
            let after: BTreeSet<_> = next.drive_serials.iter().cloned().collect();
            let removed: Vec<_> = before.difference(&after).cloned().collect();
            let added: Vec<_> = after.difference(&before).cloned().collect();
            if !removed.is_empty() || !added.is_empty() {
                if removed.first().map(String::as_str) != Some(old_serial)
                    || added.first().map(String::as_str) != Some(new_serial)
                    || removed.len() != 1
                    || added.len() != 1
                {
                    return Err(invalid("一次只能在同一库显式更换一台驱动"));
                }
                replacements.insert(
                    previous.changer_serial.clone(),
                    BTreeMap::from([(old_serial.to_owned(), new_serial.to_owned())]),
                );
            }
        }
        if replacements.len() != 1 || old_serial == new_serial {
            return Err(invalid("配置中没有指定的旧驱动到新驱动变更"));
        }
        let floor = committed_log_floor(root)?;
        let step = json!({"from":old.value(),"to":self.value(),"additions":[],"drive_additions":{},"drive_replacements":replacements,"before_index":floor});
        let marker = root.join("libraries-addition.json");
        let mut steps = if marker.exists() {
            let current: Value = serde_json::from_slice(&std::fs::read(&marker)?)
                .map_err(|e| invalid(format!("配置变更记录损坏：{e}")))?;
            current["steps"]
                .as_array()
                .cloned()
                .ok_or_else(|| invalid("配置变更记录缺少步骤"))?
        } else {
            Vec::new()
        };
        if let Some(last) = steps.last() {
            if last != &step {
                if last["to"] != old.value() {
                    return Err(invalid("已有未完成或不同的配置变更记录"));
                }
                steps.push(step);
            }
        } else {
            steps.push(step);
        }
        atomic_json(&marker, &json!({"steps":steps}))?;
        atomic_json(&manifest, &self.value())?;
        Ok(floor)
    }

    /// 全体节点停机后逐节点执行；一次只移除一台非末台驱动，保留库及磁带目录。
    pub fn remove_drive_offline(&self, root: &Path, serial: &str) -> Result<u64> {
        let manifest = root.join("libraries.json");
        let old_value: Value = serde_json::from_slice(&std::fs::read(&manifest)?)
            .map_err(|e| invalid(format!("原带库配置损坏：{e}")))?;
        let old = Self::parse(&old_value)?.load_additions(root)?;
        self.reject_removed_serial_reuse(&old)?;
        if old.default_library != self.default_library
            || old.libraries.len() != self.libraries.len()
        {
            return Err(invalid("移除驱动不得改变默认库或库集合"));
        }
        let mut removals = BTreeMap::<String, BTreeSet<String>>::new();
        for previous in &old.libraries {
            let next = self
                .resolve_library(&previous.changer_serial)
                .ok_or_else(|| invalid("移除驱动不得移除带库"))?;
            previous.ensure_same_changer_port(next)?;
            let before: BTreeSet<_> = previous.drive_serials.iter().cloned().collect();
            let after: BTreeSet<_> = next.drive_serials.iter().cloned().collect();
            let removed: Vec<_> = before.difference(&after).cloned().collect();
            if !after.is_subset(&before) || removed.len() > 1 {
                return Err(invalid("一次只能显式移除一台驱动，不得同时新增或更换"));
            }
            if let Some(removed) = removed.first() {
                if removed != serial {
                    return Err(invalid("移除的驱动与命令指定的序列号不一致"));
                }
                removals.insert(
                    previous.changer_serial.clone(),
                    BTreeSet::from([removed.clone()]),
                );
            }
        }
        if removals.len() != 1 {
            return Err(invalid("配置中没有指定的驱动移除变更"));
        }
        let floor = committed_log_floor(root)?;
        let step = json!({"from":old.value(),"to":self.value(),"additions":[],"drive_additions":{},"drive_removals":removals,"before_index":floor});
        let marker = root.join("libraries-addition.json");
        let mut steps = if marker.exists() {
            let current: Value = serde_json::from_slice(&std::fs::read(&marker)?)
                .map_err(|e| invalid(format!("配置变更记录损坏：{e}")))?;
            current["steps"]
                .as_array()
                .cloned()
                .ok_or_else(|| invalid("配置变更记录缺少步骤"))?
        } else {
            Vec::new()
        };
        if let Some(last) = steps.last() {
            if last != &step {
                if last["to"] != old.value() {
                    return Err(invalid("已有未完成或不同的配置变更记录"));
                }
                steps.push(step);
            }
        } else {
            steps.push(step);
        }
        atomic_json(&marker, &json!({"steps":steps}))?;
        atomic_json(&manifest, &self.value())?;
        Ok(floor)
    }

    /// 只有显式扩容记录列出的新增库可缺席于旧控制/目录快照。
    pub fn load_additions(mut self, root: &Path) -> Result<Self> {
        let marker = root.join("libraries-addition.json");
        if !marker.exists() {
            return Ok(self);
        }
        let record: Value = serde_json::from_slice(&std::fs::read(marker)?)
            .map_err(|e| invalid(format!("扩容记录损坏：{e}")))?;
        let steps = record["steps"]
            .as_array()
            .filter(|steps| !steps.is_empty())
            .ok_or_else(|| invalid("扩容记录步骤为空"))?;
        let mut previous: Option<Value> = None;
        let mut additions = BTreeMap::new();
        let mut replacements = Vec::new();
        let mut removals = Vec::new();
        let mut matched = None;
        let mut matched_replacements = None;
        let mut matched_removals = None;
        let mut previous_floor = None;
        for step in steps {
            let old = Self::parse(&step["from"])?;
            let next = Self::parse(&step["to"])?;
            if previous.as_ref().is_some_and(|v| *v != old.value())
                || old.default_library != next.default_library
            {
                return Err(invalid("扩容记录修改了旧库身份或设备"));
            }
            if next.libraries.iter().any(|library| {
                library.drive_serials.iter().any(|serial| {
                    removals
                        .iter()
                        .any(|removed: &DriveRemoval| &removed.serial == serial)
                })
            }) {
                return Err(invalid("已移除的驱动序列号不得在后续配置中复用"));
            }
            let mut drive_delta = BTreeMap::<String, BTreeSet<String>>::new();
            let mut replacement_delta = BTreeMap::<String, BTreeMap<String, String>>::new();
            let mut removal_delta = BTreeMap::<String, BTreeSet<String>>::new();
            for library in &old.libraries {
                let Some(updated) = next.resolve_library(&library.changer_serial) else {
                    return Err(invalid("扩容记录移除了旧库"));
                };
                library.ensure_same_changer_port(updated)?;
                let before: BTreeSet<_> = library.drive_serials.iter().cloned().collect();
                let after: BTreeSet<_> = updated.drive_serials.iter().cloned().collect();
                let removed: Vec<_> = before.difference(&after).cloned().collect();
                let added = after.difference(&before).cloned().collect::<BTreeSet<_>>();
                if removed.is_empty() && !added.is_empty() {
                    drive_delta.insert(library.changer_serial.clone(), added);
                } else if removed.len() == 1 && added.len() == 1 {
                    replacement_delta.insert(
                        library.changer_serial.clone(),
                        BTreeMap::from([(
                            removed[0].clone(),
                            added.iter().next().unwrap().clone(),
                        )]),
                    );
                } else if removed.len() == 1 && added.is_empty() {
                    removal_delta.insert(
                        library.changer_serial.clone(),
                        BTreeSet::from([removed[0].clone()]),
                    );
                } else if !removed.is_empty() {
                    return Err(invalid("配置变更记录移除或批量更换了驱动"));
                }
            }
            let delta: BTreeSet<String> = next
                .libraries
                .iter()
                .filter(|l| old.resolve_library(&l.changer_serial).is_none())
                .map(|l| l.changer_serial.clone())
                .collect();
            let recorded: BTreeSet<String> = serde_json::from_value(step["additions"].clone())
                .map_err(|e| invalid(format!("扩容记录新增库字段无效：{e}")))?;
            let recorded_drives: BTreeMap<String, BTreeSet<String>> =
                if step.get("drive_additions").is_some() {
                    serde_json::from_value(step["drive_additions"].clone())
                        .map_err(|e| invalid(format!("扩容记录新增驱动字段无效：{e}")))?
                } else {
                    BTreeMap::new()
                };
            let recorded_replacements: BTreeMap<String, BTreeMap<String, String>> =
                if step.get("drive_replacements").is_some() {
                    serde_json::from_value(step["drive_replacements"].clone())
                        .map_err(|e| invalid(format!("配置变更记录更换驱动字段无效：{e}")))?
                } else {
                    BTreeMap::new()
                };
            let recorded_removals: BTreeMap<String, BTreeSet<String>> =
                if step.get("drive_removals").is_some() {
                    serde_json::from_value(step["drive_removals"].clone())
                        .map_err(|e| invalid(format!("配置变更记录移除驱动字段无效：{e}")))?
                } else {
                    BTreeMap::new()
                };
            if (delta.is_empty()
                && drive_delta.is_empty()
                && replacement_delta.is_empty()
                && removal_delta.is_empty())
                || delta != recorded
                || drive_delta != recorded_drives
                || replacement_delta != recorded_replacements
                || removal_delta != recorded_removals
                || (!replacement_delta.is_empty()
                    && (!delta.is_empty()
                        || !drive_delta.is_empty()
                        || !removal_delta.is_empty()
                        || replacement_delta.len() != 1))
                || (!removal_delta.is_empty()
                    && (!delta.is_empty()
                        || !drive_delta.is_empty()
                        || !replacement_delta.is_empty()
                        || removal_delta.len() != 1))
            {
                return Err(invalid("配置变更记录与新增库或驱动集合不匹配"));
            }
            let floor = step["before_index"]
                .as_u64()
                .ok_or_else(|| invalid("扩容记录缺少 Raft 索引"))?;
            if previous_floor.is_some_and(|previous| previous > floor)
                || additions.values().any(|previous| *previous > floor)
            {
                return Err(invalid("扩容记录 Raft 索引回退"));
            }
            previous_floor = Some(floor);
            if old.value() == self.value() {
                matched = Some(additions.clone());
                matched_replacements = Some(replacements.clone());
                matched_removals = Some(removals.clone());
            }
            additions.extend(delta.into_iter().map(|id| (id, floor)));
            for (library, pairs) in replacement_delta {
                for (old, new) in pairs {
                    replacements.push(DriveReplacement {
                        library: library.clone(),
                        old,
                        new,
                    });
                }
            }
            for (library, serials) in removal_delta {
                for serial in serials {
                    removals.push(DriveRemoval {
                        library: library.clone(),
                        serial,
                    });
                }
            }
            if next.value() == self.value() {
                matched = Some(additions.clone());
                matched_replacements = Some(replacements.clone());
                matched_removals = Some(removals.clone());
            }
            previous = Some(next.value());
        }
        self.addition_floors = matched.ok_or_else(|| invalid("扩容记录与带库配置不匹配"))?;
        self.additions = self.addition_floors.keys().cloned().collect();
        self.drive_replacements = matched_replacements.unwrap_or_default();
        self.drive_removals = matched_removals.unwrap_or_default();
        Ok(self)
    }

    fn reject_removed_serial_reuse(&self, old: &Self) -> Result<()> {
        if self.libraries.iter().any(|library| {
            library.drive_serials.iter().any(|serial| {
                old.drive_removals
                    .iter()
                    .any(|removed| &removed.serial == serial)
            })
        }) {
            return Err(invalid("已移除的驱动序列号不得在后续配置中复用"));
        }
        Ok(())
    }
}

fn committed_index(path: &Path) -> Result<u64> {
    if !path.exists() {
        return Err(invalid("缺少 Raft 存储；不得在未同步的节点离线扩容"));
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| invalid(format!("读取 Raft 存储失败：{e}")))?;
    let bytes: Option<Vec<u8>> = db
        .query_row("SELECT data FROM hardstate WHERE id=1", [], |r| r.get(0))
        .optional()
        .map_err(|e| invalid(format!("读取 Raft 提交索引失败：{e}")))?;
    let bytes = bytes.ok_or_else(|| invalid("Raft 存储缺少 HardState；不得离线扩容"))?;
    raft::eraftpb::HardState::parse_from_bytes(&bytes)
        .map(|h| h.commit)
        .map_err(|e| invalid(format!("Raft HardState 损坏：{e}")))
}

fn committed_log_floor(root: &Path) -> Result<u64> {
    let path = root.join("raft.db");
    let floor = committed_index(&path)?;
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| invalid(format!("读取 Raft 存储失败：{e}")))?;
    let last: Option<u64> = db
        .query_row("SELECT MAX(idx) FROM entries", [], |r| r.get(0))
        .map_err(|e| invalid(format!("读取 Raft 日志尾部失败：{e}")))?;
    if last.is_some_and(|last| last > floor) {
        return Err(invalid("Raft 存在未提交日志；不得离线变更带库配置"));
    }
    Ok(floor)
}

fn atomic_json(path: &Path, value: &Value) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_file_name(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)?;
    file.write_all(&serde_json::to_vec(value).unwrap())?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

pub(super) fn encode_command(library: &str, command: &Command) -> Vec<u8> {
    let command: Value = serde_json::from_slice(&command.encode()).expect("命令编码为 JSON");
    serde_json::to_vec(&json!({"cluster_version":2,"library":library,"command":command})).unwrap()
}

fn decode_command(data: &[u8], default: &str) -> Result<(String, Command)> {
    let value: Value =
        serde_json::from_slice(data).map_err(|e| invalid(format!("日志不是有效 JSON：{e}")))?;
    if value.get("cluster_version").is_some() {
        if value["cluster_version"] != 2 {
            return Err(invalid("不支持的多库日志版本"));
        }
        let scope = value["library"]
            .as_str()
            .ok_or_else(|| invalid("日志缺少库标识"))?;
        let command = Command::decode(&serde_json::to_vec(&value["command"]).unwrap())
            .ok_or_else(|| invalid("多库日志命令无法解析"))?;
        Ok((scope.to_owned(), command))
    } else {
        Command::decode(data)
            .map(|c| (default.to_owned(), c))
            .ok_or_else(|| invalid("旧日志命令无法解析"))
    }
}

#[derive(Clone)]
struct ClusterState {
    default_library: String,
    controls: BTreeMap<String, ControlState>,
    drive_replacements: Vec<DriveReplacement>,
    drive_removals: Vec<DriveRemoval>,
}

impl ClusterState {
    fn new(config: &LibrariesConfig) -> Self {
        let controls = config
            .libraries
            .iter()
            .map(|l| (l.changer_serial.clone(), ControlState::default()))
            .chain(std::iter::once((GATEWAY.into(), ControlState::default())))
            .collect();
        Self {
            default_library: config.default_library.clone(),
            controls,
            drive_replacements: config.drive_replacements.clone(),
            drive_removals: config.drive_removals.clone(),
        }
    }
    fn reconcile_drive_replacements(&mut self, library: &str) -> Result<()> {
        let Some(control) = self.controls.get_mut(library) else {
            return Ok(());
        };
        if control.catalog_frozen_index.is_some() {
            return Ok(());
        }
        for replacement in self
            .drive_replacements
            .iter()
            .filter(|r| r.library == library)
        {
            if let Some(drives) = control
                .management
                .get_mut("drives")
                .and_then(Value::as_object_mut)
                && let Some(previous) = drives.remove(&replacement.old)
            {
                let current = drives
                    .entry(replacement.new.clone())
                    .or_insert_with(|| json!({}));
                let (Some(current), Some(previous)) =
                    (current.as_object_mut(), previous.as_object())
                else {
                    return Err(invalid("驱动策略损坏，无法完成离线更换"));
                };
                // 回放旧日志时同一驱动的多次策略更新依次覆盖对应字段。
                current.extend(previous.clone());
            }
            let old_inventory = control.management["inventory"]["drives"]
                .as_array()
                .is_some_and(|drives| {
                    drives
                        .iter()
                        .any(|d| d["serial"].as_str() == Some(replacement.old.as_str()))
                });
            if old_inventory && control.management.is_object() {
                control.management["inventory"] = Value::Null;
            }
        }
        for removal in self.drive_removals.iter().filter(|r| r.library == library) {
            if let Some(drives) = control
                .management
                .get_mut("drives")
                .and_then(Value::as_object_mut)
            {
                drives.remove(&removal.serial);
            }
            let old_inventory = control.management["inventory"]["drives"]
                .as_array()
                .is_some_and(|drives| {
                    drives
                        .iter()
                        .any(|d| d["serial"].as_str() == Some(removal.serial.as_str()))
                });
            if old_inventory && control.management.is_object() {
                control.management["inventory"] = Value::Null;
            }
            if let Some(tasks) = control
                .management
                .get_mut("tasks")
                .and_then(Value::as_object_mut)
            {
                for task in tasks.values_mut() {
                    if task["request"]["resource"] == "drives"
                        && task["request"]["target"] == removal.serial
                        && (task["state"] == "queued" || task["state"] == "running")
                    {
                        task["state"] = json!("indeterminate");
                        task["result"] = json!({"reason":"驱动已从配置移除；任务未继续执行"});
                    }
                }
            }
        }
        Ok(())
    }
    fn snapshot(&self, source: u64, index: u64) -> Vec<u8> {
        let controls: BTreeMap<_, Value> = self
            .controls
            .iter()
            .map(|(id, control)| {
                (
                    id.clone(),
                    serde_json::from_slice(
                        &ControlSnapshot {
                            control: control.clone(),
                            source,
                            directory_index: index,
                        }
                        .encode(),
                    )
                    .unwrap(),
                )
            })
            .collect();
        serde_json::to_vec(&json!({"cluster_version":2,"default_library":self.default_library,"source":source,"directory_index":index,"controls":controls})).unwrap()
    }
    fn restore(config: &LibrariesConfig, data: &[u8]) -> Result<(Self, u64, u64)> {
        let value: Value =
            serde_json::from_slice(data).map_err(|e| invalid(format!("控制快照损坏：{e}")))?;
        if value.get("cluster_version").is_none() {
            let legacy =
                ControlSnapshot::decode(data).ok_or_else(|| invalid("旧控制快照无法解析"))?;
            let default = config
                .libraries
                .iter()
                .find(|l| l.changer_serial == config.default_library)
                .unwrap();
            if let Some(serial) = legacy.control.management["inventory"]["library_serial"].as_str()
                && serial != default.changer_serial
            {
                return Err(invalid("默认库设备与旧库存身份不一致，拒绝迁移"));
            }
            let mut state = Self::new(config);
            // 旧 VIP 占用必须保留至原节点确认撤址，不能因升级清空。
            state.controls.get_mut(GATEWAY).unwrap().vips = legacy.control.vips.clone();
            state.controls.get_mut(GATEWAY).unwrap().executor = legacy.control.executor.clone();
            state
                .controls
                .insert(config.default_library.clone(), legacy.control);
            state.reconcile_drive_replacements(&config.default_library)?;
            return Ok((state, legacy.source, legacy.directory_index));
        }
        if value["cluster_version"] != 2 || value["default_library"] != config.default_library {
            return Err(invalid("控制快照版本或默认库不匹配"));
        }
        let rows = value["controls"]
            .as_object()
            .ok_or_else(|| invalid("控制快照缺少分库状态"))?;
        let directory_index = value["directory_index"]
            .as_u64()
            .ok_or_else(|| invalid("快照缺少目录索引"))?;
        let mut state = Self::new(config);
        let expected: BTreeSet<_> = state.controls.keys().cloned().collect();
        let actual: BTreeSet<_> = rows.keys().cloned().collect();
        let missing: BTreeSet<_> = expected.difference(&actual).cloned().collect();
        if !actual.is_subset(&expected)
            || missing.iter().any(|id| {
                config
                    .addition_floors
                    .get(id)
                    .is_none_or(|floor| directory_index > *floor)
            })
        {
            return Err(invalid("快照与配置的库集合不匹配"));
        }
        for (id, row) in rows {
            state.controls.insert(
                id.clone(),
                ControlSnapshot::decode(&serde_json::to_vec(row).unwrap())
                    .ok_or_else(|| invalid(format!("库 {id} 的控制快照损坏")))?
                    .control,
            );
            state.reconcile_drive_replacements(id)?;
        }
        Ok((
            state,
            value["source"]
                .as_u64()
                .ok_or_else(|| invalid("快照缺少 source"))?,
            directory_index,
        ))
    }
}

pub struct LibraryRuntime {
    pub id: String,
    pub group: String,
    pub drives: usize,
    pub directory_file: Option<PathBuf>,
    pub exec: Sender<ExecRequest>,
    pub status: SharedStatus,
    pub files: Arc<FileService>,
}

/// 把现有 HTTP/执行输入绑定到库，不允许调用方影响另一库。
pub fn scoped_sender(library: String, target: Sender<NodeInput>) -> Sender<NodeInput> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for event in rx {
            if target
                .send(NodeInput::Library {
                    library: library.clone(),
                    event: Box::new(event),
                })
                .is_err()
            {
                break;
            }
        }
    });
    tx
}

/// 入口候选仅要求网络资格；其隔离/恢复步骤没有设备副作用。
// 一个虚拟资格槽复用已有候选协议；不会作为物理驱动对外报告。
pub struct GatewayProvider;
impl DeviceProvider for GatewayProvider {
    fn open_all(&self) -> Result<Vec<super::executor::ManagedDevice>> {
        Ok(Vec::new())
    }
    fn probe(&self) -> Result<usize> {
        Ok(0)
    }
}

pub fn run_gateway(
    provider: Box<dyn DeviceProvider>,
    rx: Receiver<ExecRequest>,
    events: Sender<ExecEvent>,
    files: Arc<FileService>,
) {
    let mut next = Instant::now();
    loop {
        if Instant::now() >= next {
            let _ = events.send(ExecEvent::Capability {
                drives: 1,
                eligible: provider.probe().is_ok(),
            });
            next = Instant::now() + Duration::from_millis(500);
        }
        match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(ExecRequest::AttachPermission { permission }) => files.set_permission(permission),
            Ok(ExecRequest::Takeover { round }) => {
                let _ = events.send(ExecEvent::Fenced { round });
            }
            Ok(ExecRequest::Recover { round }) => {
                let _ = events.send(ExecEvent::Serving {
                    round,
                    summary: "统一 HTTP 入口就绪".into(),
                });
            }
            Ok(ExecRequest::Shutdown { done }) => {
                if let Some(done) = done {
                    let _ = done.send(());
                }
                break;
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

struct Running {
    runtime: LibraryRuntime,
    coordinator: Coordinator,
    directory: Option<Directory>,
    catalog_job: Option<Receiver<Result<Directory>>>,
    directory_queue: VecDeque<(u64, Command, Applied)>,
    cfg: NodeConfig,
    local: String,
    last_status: String,
}

struct SnapshotJob {
    index: u64,
    control: Vec<u8>,
    result: Receiver<Result<BTreeMap<String, Directory>>>,
}

fn start_snapshot(
    path: PathBuf,
    running: &mut BTreeMap<String, Running>,
    index: u64,
    control: Vec<u8>,
) -> SnapshotJob {
    let directories: BTreeMap<_, _> = running
        .iter_mut()
        .filter_map(|(id, r)| r.directory.take().map(|d| (id.clone(), d)))
        .collect();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = write_bundle_refs(
            &path,
            directories.iter().map(|(id, d)| (id.as_str(), d)),
            index,
        )
        .map(|_| directories);
        let _ = tx.send(result);
    });
    SnapshotJob {
        index,
        control,
        result: rx,
    }
}

fn bulk_catalog(command: &Command, outcome: &Applied) -> bool {
    match (command, outcome) {
        (Command::ExecutorReport { command, .. }, Applied::Reported(inner)) => {
            bulk_catalog(command, inner)
        }
        (Command::TapeCommitted { parts, files, .. }, Applied::CatalogCommitted) => {
            *parts > 1 || *files > 4096
        }
        _ => false,
    }
}

fn apply_directory(r: &mut Running, index: u64, command: Command, outcome: Applied) -> Result<()> {
    let Some(mut directory) = r.directory.take() else {
        r.directory_queue.push_back((index, command, outcome));
        return Ok(());
    };
    if bulk_catalog(&command, &outcome) {
        let (tx, rx) = mpsc::channel();
        r.catalog_job = Some(rx);
        std::thread::spawn(move || {
            let result = directory
                .apply(index, &command, &outcome)
                .map(|_| directory);
            let _ = tx.send(result);
        });
    } else {
        directory.apply(index, &command, &outcome)?;
        r.directory = Some(directory);
    }
    Ok(())
}

fn directory_idle(r: &Running) -> bool {
    r.runtime.directory_file.is_none() || (r.directory.is_some() && r.directory_queue.is_empty())
}

fn poll_directories(running: &mut BTreeMap<String, Running>) -> Result<()> {
    for r in running.values_mut() {
        if let Some(job) = &r.catalog_job {
            match job.try_recv() {
                Ok(result) => {
                    r.directory = Some(result?);
                    r.catalog_job = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => return Err(invalid("目录合并线程退出")),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        // 有界批量追齐：只放行一条会被持续到达的 Raft 条目永久追平，导致队列无法清空。
        let until = Instant::now() + Duration::from_millis(10);
        for _ in 0..32 {
            if r.directory.is_none() {
                break;
            }
            let Some((index, command, outcome)) = r.directory_queue.pop_front() else {
                break;
            };
            apply_directory(r, index, command, outcome)?;
            if Instant::now() >= until {
                break;
            }
        }
    }
    Ok(())
}

/// 单一 Raft 循环；每库独立 Coordinator 和 ReadIndex 上下文。所有快照一起覆盖同一全局索引。
#[allow(clippy::too_many_arguments)]
pub fn run(
    cfg: NodeConfig,
    config: LibrariesConfig,
    mut store: RaftStore,
    net: Box<dyn Network>,
    fetcher: Option<Arc<dyn DirectoryFetch>>,
    self_tx: Sender<NodeInput>,
    inbox: Receiver<NodeInput>,
    runtimes: Vec<LibraryRuntime>,
) -> Result<()> {
    let raft_cfg = Config {
        id: cfg.id,
        election_tick: cfg.election_ticks,
        heartbeat_tick: cfg.heartbeat_ticks,
        check_quorum: true,
        pre_vote: true,
        ..Default::default()
    };
    raft_cfg.validate().map_err(|e| invalid(e.to_string()))?;
    let logger = slog::Logger::root(slog_stdlog::StdLog.fuse(), slog::o!());
    let storage = store.raft_storage();
    let mut node =
        RawNode::new(&raft_cfg, storage.clone(), &logger).map_err(|e| invalid(e.to_string()))?;
    let mut applied = store.snapshot_index();
    let (mut state, source, directory_index) = match store.loaded_snapshot() {
        Some(s) => ClusterState::restore(&config, s.get_data())?,
        None => (ClusterState::new(&config), 0, 0),
    };
    let legacy_snapshot = store.loaded_snapshot().is_some_and(|s| {
        serde_json::from_slice::<Value>(s.get_data())
            .is_ok_and(|v| v.get("cluster_version").is_none())
    });
    let snapshot_additions: BTreeSet<String> = store
        .loaded_snapshot()
        .and_then(|s| serde_json::from_slice::<Value>(s.get_data()).ok())
        .and_then(|v| v["controls"].as_object().cloned())
        .map(|controls| {
            config
                .additions
                .iter()
                .filter(|id| !controls.contains_key(id.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let mut running = BTreeMap::new();
    for runtime in runtimes {
        let coordination = CoordinationConfig {
            group: runtime.group.clone(),
            validity: cfg
                .coordination
                .as_ref()
                .ok_or_else(|| invalid("多库需要执行许可配置"))?
                .validity,
        };
        let mut coordinator = Coordinator::new(
            cfg.id,
            store.next_instance()?,
            coordination.clone(),
            cfg.cooldown,
            &runtime.exec,
        )?;
        coordinator.set_scope(runtime.id.clone());
        let mut directory = runtime
            .directory_file
            .as_deref()
            .map(Directory::open)
            .transpose()?;
        if legacy_snapshot
            && runtime.id != config.default_library
            && let Some(d) = directory.as_mut()
        {
            d.apply(
                applied,
                &Command::Takeover { node: 0, term: 0 },
                &Applied::Nothing,
            )?;
        }
        let mut local_cfg = cfg.clone();
        local_cfg.coordination = Some(coordination);
        local_cfg.drives = runtime.drives;
        local_cfg.directory_file = runtime.directory_file.clone();
        local_cfg.status_file = None;
        running.insert(
            runtime.id.clone(),
            Running {
                runtime,
                coordinator,
                directory,
                catalog_job: None,
                directory_queue: VecDeque::new(),
                cfg: local_cfg,
                local: "启动中".into(),
                last_status: String::new(),
            },
        );
    }
    if running.keys().collect::<BTreeSet<_>>() != state.controls.keys().collect::<BTreeSet<_>>() {
        return Err(invalid("运行时与配置库集合不一致"));
    }
    for (id, r) in &running {
        r.runtime
            .files
            .set_catalog_quiesced(state.controls[id].catalog_quiesce_index.is_some());
    }
    if legacy_snapshot
        && running[&config.default_library]
            .directory
            .as_ref()
            .is_some_and(|d| d.applied_index() >= directory_index)
    {
        // 本机旧目录已齐全；新库起始为空，之后统一按全局索引推进。
        advance_directories(&mut running, directory_index)?;
    }
    if !snapshot_additions.is_empty()
        && running
            .iter()
            .filter(|(id, _)| !snapshot_additions.contains(*id))
            .all(|(_, r)| {
                r.directory
                    .as_ref()
                    .is_none_or(|d| d.applied_index() >= directory_index)
            })
    {
        for id in &snapshot_additions {
            if let Some(d) = running.get_mut(id).and_then(|r| r.directory.as_mut()) {
                d.apply(
                    directory_index,
                    &Command::Takeover { node: 0, term: 0 },
                    &Applied::Nothing,
                )?;
            }
        }
    }
    let behind = running
        .values()
        .filter_map(|r| r.directory.as_ref())
        .any(|d| d.applied_index() < directory_index);
    let mut directory_floor = running
        .values()
        .filter_map(|r| r.directory.as_ref().map(Directory::applied_index))
        .max()
        .unwrap_or(0);
    let mut pending = behind.then(|| (source, directory_index, applied, state.clone()));
    let mut fetching = false;
    let mut next_fetch = Instant::now();
    let mut next_tick = Instant::now() + cfg.tick;
    let mut waiters: HashMap<u64, Sender<AdminReply>> = HashMap::new();
    let mut request_id = 0u64;
    let mut shutdown: Option<(Instant, Vec<Receiver<()>>)> = None;
    let mut snapshot_job: Option<SnapshotJob> = None;
    let mut deferred_directory = None;
    loop {
        if let Some(job) = snapshot_job.as_ref() {
            match job.result.try_recv() {
                Ok(result) => {
                    for (id, directory) in result? {
                        running.get_mut(&id).unwrap().directory = Some(directory);
                    }
                    let job = snapshot_job.take().unwrap();
                    // 若期间安装了更新的控制快照，旧的后台结果只归还连接，不再压缩日志。
                    if job.index > store.snapshot_index() {
                        let snapshot = store.build_snapshot(job.index, job.control)?;
                        store.compact_to(&snapshot)?;
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => return Err(invalid("目录快照线程退出")),
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        poll_directories(&mut running)?;
        if let Some((deadline, receivers)) = &shutdown {
            let done = receivers
                .iter()
                .all(|r| !matches!(r.try_recv(), Err(mpsc::TryRecvError::Empty)));
            let held = state.controls[GATEWAY]
                .vips
                .values()
                .any(|v| v.holder.as_ref().is_some_and(|h| h.node == cfg.id));
            if (done && !held && running.values().all(directory_idle))
                || Instant::now() >= *deadline
            {
                return Ok(());
            }
        }
        let deferred_ready = deferred_directory.is_some()
            && snapshot_job.is_none()
            && running.values().all(directory_idle);
        let input = if deferred_ready {
            deferred_directory.take().map(NodeInput::Directory)
        } else {
            match inbox.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                Ok(input) => Some(input),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Err(invalid("多库输入通道关闭")),
            }
        };
        let (scope, event) = match input {
            Some(NodeInput::Library { library, event }) => (library, Some(*event)),
            event => (GATEWAY.to_owned(), event),
        };
        match event {
            Some(NodeInput::Shutdown) => {
                if shutdown.is_none() {
                    let mut done = Vec::new();
                    for r in running.values() {
                        r.runtime
                            .status
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .shutting_down = true;
                        let (tx, rx) = mpsc::channel();
                        let _ = r
                            .runtime
                            .exec
                            .send(ExecRequest::Shutdown { done: Some(tx) });
                        done.push(rx);
                    }
                    shutdown = Some((Instant::now() + cfg.shutdown_grace, done));
                }
            }
            Some(NodeInput::Raft(m)) => {
                if scope == GATEWAY {
                    let _ = node.step(*m);
                }
            }
            Some(NodeInput::Exec(event)) => {
                let r = running
                    .get_mut(&scope)
                    .ok_or_else(|| invalid(format!("未知事件库 {scope}")))?;
                r.coordinator
                    .event(event, &mut node, &state.controls[&scope], &mut r.local);
            }
            Some(NodeInput::Vip(command)) => {
                let valid = match &command {
                    Command::VipAcquire { node, .. } => *node == cfg.id && shutdown.is_none(),
                    Command::VipRelease { node, .. } => *node == cfg.id,
                    _ => false,
                };
                if scope == GATEWAY && valid {
                    let _ = node.propose(vec![], encode_command(GATEWAY, &command));
                }
            }
            Some(NodeInput::Admin { cmd, reply }) => {
                let reject = if scope == GATEWAY || !state.controls.contains_key(&scope) {
                    Some("管理命令需要明确的已配置带库".into())
                } else if matches!(cmd, Command::VipAcquire { .. } | Command::VipRelease { .. }) {
                    Some("禁止通过管理入口提交 VIP 命令".into())
                } else if shutdown.is_some() {
                    Some("节点正在停机".into())
                } else if matches!(cmd, Command::TapeReclaim { .. })
                    && state.controls[&scope]
                        .executor
                        .as_ref()
                        .and_then(|e| state.controls[&scope].candidates.get(&e.node))
                        .is_none_or(|c| c.drives < 2)
                {
                    Some("回收需要目标库 owner 的两台驱动器".into())
                } else {
                    None
                };
                if let Some(reason) = reject {
                    let _ = reply.send(AdminReply::Rejected(reason));
                } else if node.raft.state != StateRole::Leader {
                    let _ = reply.send(AdminReply::NotLeader(node.raft.leader_id));
                } else {
                    request_id += 1;
                    match node.propose(
                        request_id.to_be_bytes().to_vec(),
                        encode_command(&scope, &cmd),
                    ) {
                        Ok(()) => {
                            waiters.insert(request_id, reply);
                        }
                        Err(e) => {
                            let _ = reply.send(AdminReply::Rejected(e.to_string()));
                        }
                    }
                }
            }
            Some(NodeInput::Directory(result))
                if snapshot_job.is_some() || !running.values().all(directory_idle) =>
            {
                deferred_directory = Some(result);
            }
            Some(NodeInput::Directory(result)) => {
                fetching = false;
                next_fetch = Instant::now() + Duration::from_secs(1);
                if let (Ok((reported, file)), Some((_, want, at, base))) =
                    (result, pending.as_ref())
                {
                    let index = verified_bundle_index(&file, reported, *want)?;
                    install_bundle(&file, &mut running, index, &config.addition_floors)?;
                    directory_floor = index;
                    let entries = storage
                        .entries(
                            at + 1,
                            applied + 1,
                            None,
                            raft::GetEntriesContext::empty(false),
                        )
                        .map_err(|e| invalid(e.to_string()))?;
                    let mut shadow = base.clone();
                    for entry in entries {
                        if entry.get_entry_type() == EntryType::EntryNormal
                            && !entry.data.is_empty()
                        {
                            apply_entry_for_replay(
                                &mut shadow,
                                &mut running,
                                entry.index,
                                &entry.data,
                                true,
                            )?;
                        }
                    }
                    // 空 Raft 条目也必须推进各目录索引。
                    advance_directories(&mut running, applied)?;
                    pending = None;
                    log::info!(
                        "多库目录快照已装入：报告索引 {reported}，文件索引 {index}，本地应用索引 {applied}"
                    );
                }
            }
            Some(NodeInput::Library { .. }) => return Err(invalid("禁止嵌套带库事件")),
            None => {}
        }
        if Instant::now() >= next_tick {
            node.tick();
            next_tick = Instant::now() + cfg.tick;
        }
        if node.raft.state != StateRole::Leader {
            waiters.clear();
        }
        for (id, r) in &mut running {
            // `tick` may send Recover immediately. Publish the frozen source
            // index it used before the executor can consume that request.
            r.runtime
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .catalog_frozen_index = state.controls[id].catalog_frozen_index;
            r.coordinator.set_catalog_ready(directory_idle(r));
            r.coordinator.tick(
                &mut node,
                &state.controls[id],
                pending.is_none() && applied >= directory_floor,
                &r.runtime.exec,
                &mut r.local,
            );
        }
        let (snapshot, entries, reads) = on_ready(&mut node, &mut store, net.as_ref())?;
        if let Some(snapshot) = snapshot {
            applied = snapshot.get_metadata().index;
            let (new_state, source, index) = ClusterState::restore(&config, snapshot.get_data())?;
            state = new_state;
            for (id, r) in &running {
                r.runtime
                    .files
                    .set_catalog_quiesced(state.controls[id].catalog_quiesce_index.is_some());
            }
            // FDB/control-only runtimes have no local directory bundle to fetch.
            // Waiting for one would permanently block ReadIndex permissions
            // after snapshot catchup, even though the control state is applied.
            let needs_directory = running.values().any(|r| r.runtime.directory_file.is_some());
            pending = needs_directory.then(|| (source, index, applied, state.clone()));
            if !needs_directory {
                directory_floor = applied;
            }
            for r in running.values() {
                let _ = r.runtime.exec.send(ExecRequest::Stop {
                    round: None,
                    reason: if needs_directory {
                        "等待分库目录快照"
                    } else {
                        "控制快照已更新，重新确认执行轮次"
                    }
                    .into(),
                });
            }
        }
        for entry in entries {
            applied = applied.max(entry.index);
            if entry.get_entry_type() != EntryType::EntryNormal || entry.data.is_empty() {
                continue;
            }
            let outcome = apply_entry(
                &mut state,
                &mut running,
                entry.index,
                &entry.data,
                pending.is_none(),
            )?;
            if let Ok(bytes) = <[u8; 8]>::try_from(entry.context.as_ref())
                && let Some(reply) = waiters.remove(&u64::from_be_bytes(bytes))
            {
                let response = match outcome {
                    Applied::Admin(Ok(v)) => AdminReply::Ok(v),
                    Applied::Admin(Err(e)) => AdminReply::Rejected(e),
                    Applied::CatalogRejected => {
                        AdminReply::Rejected("旧目录已暂停或冻结，拒绝新的变更".into())
                    }
                    _ => AdminReply::Ok("已应用".into()),
                };
                let _ = reply.send(response);
            }
        }
        if pending.is_none() {
            advance_directories(&mut running, applied)?;
        }
        for (id, r) in &mut running {
            let idle = directory_idle(r);
            r.runtime
                .files
                .set_catalog_quiesced(state.controls[id].catalog_quiesce_index.is_some());
            r.coordinator.observe(&state.controls[id], idle);
            r.runtime
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .catalog_pending = r.catalog_job.is_some()
                || r.directory_queue
                    .iter()
                    .any(|(_, command, outcome)| bulk_catalog(command, outcome));
            r.coordinator.confirm(
                reads.clone(),
                &node,
                &state.controls[id],
                applied,
                pending.is_none() && applied >= directory_floor,
            );
            publish_status(
                &r.cfg,
                &node,
                &state.controls[id],
                &r.local,
                &None,
                &r.runtime.status,
                &mut r.last_status,
            );
        }
        if let Some(path) = &cfg.status_file {
            let text = json!({"id":cfg.id,"role":format!("{:?}",node.raft.state),"term":node.raft.term,"leader":node.raft.leader_id,"applied_index":applied,"default_library":config.default_library,"vips":state.controls[GATEWAY].vips.iter().map(|(a,r)|json!({"address":a.to_string(),"token":r.token,"holder":r.holder.as_ref().map(|h|json!({"node":h.node,"round":h.round}))})).collect::<Vec<_>>(),"libraries":running.iter().filter(|(id,_)|id.as_str()!=GATEWAY).map(|(id,r)| { let s = r.runtime.status.lock().unwrap_or_else(|e| e.into_inner()); (id.clone(),json!({"executor":s.executor.as_ref().map(|e|json!({"node":e.node,"round":e.round,"fenced":e.fenced})),"executor_ready":s.executor_ready,"catalog_quiesce_index":s.catalog_quiesce_index,"catalog_frozen_index":s.catalog_frozen_index,"local":s.local})) }).collect::<BTreeMap<_,_>>()});
            let tmp = path.with_extension("tmp");
            if std::fs::write(&tmp, text.to_string()).is_ok() {
                let _ = std::fs::rename(tmp, path);
            }
        }
        let asked = storage.wanted().is_some_and(|want| applied >= want);
        if pending.is_none()
            && snapshot_job.is_none()
            && running.values().all(directory_idle)
            && applied >= directory_floor
            && applied > store.snapshot_index()
            && (store.due(&cfg.snapshot) || asked)
        {
            let control = state.snapshot(cfg.id, applied);
            if let Some(live) = &cfg.directory_file {
                snapshot_job = Some(start_snapshot(
                    Directory::snapshot_path(live),
                    &mut running,
                    applied,
                    control,
                ));
            } else {
                let snapshot = store.build_snapshot(applied, control)?;
                store.compact_to(&snapshot)?;
            }
        }
        if let Some((source, want, _, _)) = pending.as_ref()
            && !fetching
            && Instant::now() >= next_fetch
            && let (Some(fetcher), Some(live)) = (fetcher.as_ref(), cfg.directory_file.as_ref())
        {
            fetching = true;
            let (fetcher, tx, dest, source, want) = (
                fetcher.clone(),
                self_tx.clone(),
                Directory::incoming_path(live),
                *source,
                *want,
            );
            std::thread::spawn(move || {
                let result = fetcher
                    .fetch(source, want, &dest)
                    .map(|index| (index, dest))
                    .map_err(|e| e.to_string());
                let _ = tx.send(NodeInput::Directory(result));
            });
        }
    }
}

fn apply_entry(
    state: &mut ClusterState,
    running: &mut BTreeMap<String, Running>,
    index: u64,
    data: &[u8],
    write: bool,
) -> Result<Applied> {
    apply_entry_inner(state, running, index, data, write, true)
}

fn apply_entry_for_replay(
    state: &mut ClusterState,
    running: &mut BTreeMap<String, Running>,
    index: u64,
    data: &[u8],
    write: bool,
) -> Result<Applied> {
    apply_entry_inner(state, running, index, data, write, false)
}

fn apply_entry_inner(
    state: &mut ClusterState,
    running: &mut BTreeMap<String, Running>,
    index: u64,
    data: &[u8],
    write: bool,
    live_gate: bool,
) -> Result<Applied> {
    let (scope, command) = decode_command(data, &state.default_library)?;
    let control = state
        .controls
        .get_mut(&scope)
        .ok_or_else(|| invalid(format!("日志引用未配置的库 {scope}")))?;
    let outcome = control.apply(index, &command);
    if live_gate && let Some(r) = running.get(&scope) {
        r.runtime
            .files
            .set_catalog_quiesced(control.catalog_quiesce_index.is_some());
    }
    state.reconcile_drive_replacements(&scope)?;
    if write
        && let Some(r) = running
            .get_mut(&scope)
            .filter(|r| r.runtime.directory_file.is_some())
    {
        if r.directory_queue.is_empty() {
            apply_directory(r, index, command.clone(), outcome.clone())?;
        } else {
            r.directory_queue
                .push_back((index, command.clone(), outcome.clone()));
        }
    }
    // 历史 Acquire 必须由旧设备 owner 状态验证；网关 owner 是独立状态。
    // 直接拿新网关验证会丢掉快照之后提交的旧 VIP 撤址屏障。
    if scope == state.default_library
        && !matches!(outcome, Applied::CatalogRejected)
        && matches!(
            command,
            Command::VipAcquire { .. } | Command::VipRelease { .. }
        )
        && serde_json::from_slice::<Value>(data)
            .expect("日志 JSON 已解码")
            .get("cluster_version")
            .is_none()
    {
        let vips = state.controls[&scope].vips.clone();
        state.controls.get_mut(GATEWAY).unwrap().vips = vips;
    }
    Ok(outcome)
}

fn advance_directories(running: &mut BTreeMap<String, Running>, index: u64) -> Result<()> {
    for r in running.values_mut() {
        if r.directory_queue.is_empty()
            && let Some(d) = r.directory.as_mut()
        {
            d.apply(
                index,
                &Command::Takeover { node: 0, term: 0 },
                &Applied::Nothing,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn write_bundle(path: &Path, running: &BTreeMap<String, Running>, index: u64) -> Result<()> {
    write_bundle_refs(
        path,
        running
            .iter()
            .filter_map(|(id, r)| r.directory.as_ref().map(|d| (id.as_str(), d))),
        index,
    )
}

fn write_bundle_refs<'a>(
    path: &Path,
    directories: impl Iterator<Item = (&'a str, &'a Directory)>,
    index: u64,
) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut db = rusqlite::Connection::open(&tmp).map_err(|e| invalid(e.to_string()))?;
    db.execute_batch("CREATE TABLE meta(k TEXT PRIMARY KEY,v INTEGER NOT NULL); CREATE TABLE libraries(id TEXT NOT NULL,part INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(id,part));").map_err(|e| invalid(e.to_string()))?;
    let tx = db.transaction().map_err(|e| invalid(e.to_string()))?;
    for (id, d) in directories {
        if d.applied_index() != index {
            return Err(invalid("分库目录与全局快照索引不一致"));
        }
        let part = path.with_extension(format!("{id}.part"));
        d.write_snapshot(&part)?;
        let mut file = std::fs::File::open(&part)?;
        let mut buffer = vec![0u8; 1 << 20];
        let mut sequence = 0u64;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            tx.execute(
                "INSERT INTO libraries VALUES(?1,?2,?3)",
                rusqlite::params![id, sequence, &buffer[..n]],
            )
            .map_err(|e| invalid(e.to_string()))?;
            sequence += 1;
        }
        std::fs::remove_file(part)?;
    }
    tx.execute("INSERT INTO meta VALUES('applied_index',?1)", [index])
        .map_err(|e| invalid(e.to_string()))?;
    tx.commit().map_err(|e| invalid(e.to_string()))?;
    drop(db);
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// 传输线程读索引后，源节点可能原子替换快照文件。校验和保证了接收文件完整，
/// 实际安装必须使用该文件自己的索引；绝不把新目录按旧索引解除服务屏障。
fn verified_bundle_index(path: &Path, reported: u64, minimum: u64) -> Result<u64> {
    let actual = Directory::applied_index_of(path)?;
    if actual < reported || actual < minimum {
        return Err(invalid("目录快照实际索引落后于传输头或控制快照"));
    }
    Ok(actual)
}

fn install_bundle(
    path: &Path,
    running: &mut BTreeMap<String, Running>,
    index: u64,
    additions: &BTreeMap<String, u64>,
) -> Result<()> {
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| invalid(e.to_string()))?;
    let mut stmt = db
        .prepare("SELECT DISTINCT id FROM libraries ORDER BY id")
        .map_err(|e| invalid(e.to_string()))?;
    let ids: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .map_err(|e| invalid(e.to_string()))?
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| invalid(e.to_string()))?;
    let expected: BTreeSet<String> = running
        .iter()
        .filter(|(_, r)| r.directory.is_some())
        .map(|(id, _)| id.clone())
        .collect();
    let actual: BTreeSet<String> = ids.iter().cloned().collect();
    let missing: BTreeSet<String> = expected.difference(&actual).cloned().collect();
    if !actual.is_subset(&expected)
        || missing
            .iter()
            .any(|id| additions.get(id).is_none_or(|floor| index > *floor))
    {
        return Err(invalid("目录快照包的库集合不匹配"));
    }
    // 先验证全部内容，再更换任一库；每次只缓存一个分片，不把整个集群目录读入内存。
    let mut parts = Vec::new();
    for id in ids {
        let live = running[&id].runtime.directory_file.as_ref().unwrap();
        let part = Directory::incoming_path(live);
        let mut file = std::fs::File::create(&part)?;
        let mut stmt = db
            .prepare("SELECT part,data FROM libraries WHERE id=?1 ORDER BY part")
            .map_err(|e| invalid(e.to_string()))?;
        let chunks = stmt
            .query_map([&id], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|e| invalid(e.to_string()))?;
        for (expected, chunk) in chunks.enumerate() {
            let (sequence, bytes) = chunk.map_err(|e| invalid(e.to_string()))?;
            if sequence != expected as u64 || bytes.is_empty() || bytes.len() > 1 << 20 {
                return Err(invalid("目录快照分片缺失或长度异常"));
            }
            file.write_all(&bytes)?;
        }
        file.sync_all()?;
        drop(file);
        if Directory::applied_index_of(&part)? != index {
            return Err(invalid("分库目录快照索引不一致"));
        }
        parts.push((id, part));
    }
    for id in missing {
        let live = running[&id].runtime.directory_file.as_ref().unwrap();
        let part = Directory::incoming_path(live);
        let mut empty = Directory::open(Path::new(":memory:"))?;
        empty.apply(
            index,
            &Command::Takeover { node: 0, term: 0 },
            &Applied::Nothing,
        )?;
        empty.write_snapshot(&part)?;
        parts.push((id, part));
    }
    for (id, part) in parts {
        let r = running.get_mut(&id).unwrap();
        drop(r.directory.take());
        r.directory = Some(Directory::install(
            r.runtime.directory_file.as_ref().unwrap(),
            &part,
            index,
        )?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::node::NodeStatus;
    use super::super::state::FileRec;
    use super::super::store::SnapshotPolicy;
    use super::*;
    use std::sync::Mutex;
    fn config() -> LibrariesConfig {
        LibrariesConfig::parse(&json!({"default_library":"a","libraries":[{"changer_serial":"b","drive_serials":["db"]},{"changer_serial":"a","drive_serials":["da"]}]})).unwrap()
    }
    #[test]
    fn changer_fc_port_is_canonical_and_cannot_change_through_offline_edits() {
        let value = json!({"default_library":"A", "libraries":[{
            "changer_serial":"A", "drive_serials":["D1", "D2"],
            "changer_target_port":"5000E1117018605D"
        }]});
        let config = LibrariesConfig::parse(&value).unwrap();
        assert_eq!(
            config.libraries[0].changer_target_port.as_deref(),
            Some("0x5000e1117018605d")
        );
        assert_eq!(
            LibrariesConfig::parse(&config.value()).unwrap().value(),
            config.value()
        );
        for bad in [
            json!(null),
            json!(7),
            json!("0x0"),
            json!("0000000000000000"),
        ] {
            let mut changed = value.clone();
            changed["libraries"][0]["changer_target_port"] = bad;
            assert!(LibrariesConfig::parse(&changed).is_err());
        }
        let root = std::env::temp_dir().join(format!("changer-port-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        config.check_manifest(&root).unwrap();
        let original = std::fs::read(root.join("libraries.json")).unwrap();
        let mut changed = config.value();
        changed["libraries"][0]["changer_target_port"] = json!("0x5000e11170186070");
        let changed = LibrariesConfig::parse(&changed).unwrap();
        assert!(changed.check_manifest(&root).is_err());
        for result in [
            changed.add_libraries_offline(&root),
            changed.replace_drive_offline(&root, "D1", "D3"),
            changed.remove_drive_offline(&root, "D1"),
        ] {
            assert!(result.unwrap_err().to_string().contains("预留域"));
        }
        assert_eq!(
            std::fs::read(root.join("libraries.json")).unwrap(),
            original
        );
        assert!(!root.join("libraries-addition.json").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn serial_is_the_only_library_identity() {
        let cfg = config();
        assert_eq!(cfg.resolve_library("b").unwrap().changer_serial, "b");
        assert!(cfg.resolve_library("missing").is_none());
        let fresh = LibrariesConfig::parse(&json!({"default_library":"SERIAL01","libraries":[{"changer_serial":"SERIAL01","drive_serials":["DRIVE01"]}]})).unwrap();
        assert_eq!(fresh.libraries[0].changer_serial, "SERIAL01");
        let mut value = cfg.value();
        value["libraries"][0]["id"] = json!("alias");
        assert!(LibrariesConfig::parse(&value).is_err());
        value = cfg.value();
        value["default_library"] = json!("alias");
        assert!(LibrariesConfig::parse(&value).is_err());
    }
    #[test]
    fn reject_ambiguous_or_overlapping_libraries_and_persist_default() {
        let cfg = config();
        let mut value = cfg.value();
        value["libraries"][1]["drive_serials"] = json!(["da"]);
        assert!(LibrariesConfig::parse(&value).is_err());
        value = cfg.value();
        value["default_library"] = json!("missing");
        assert!(LibrariesConfig::parse(&value).is_err());
        value = cfg.value();
        value["libraries"][1]["id"] = json!("../escape");
        assert!(LibrariesConfig::parse(&value).is_err());
        let root = std::env::temp_dir().join(format!("multi-config-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        cfg.check_manifest(&root).unwrap();
        value = cfg.value();
        value["libraries"].as_array_mut().unwrap().reverse();
        LibrariesConfig::parse(&value)
            .unwrap()
            .check_manifest(&root)
            .unwrap();
        value["default_library"] = json!("b");
        assert!(
            LibrariesConfig::parse(&value)
                .unwrap()
                .check_manifest(&root)
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_offline_addition_preserves_old_snapshot_and_creates_empty_new_directory() {
        let root = std::env::temp_dir().join(format!("multi-add-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let old = config();
        old.check_manifest(&root).unwrap();
        let mut value = old.value();
        value["libraries"].as_array_mut().unwrap().push(json!({
            "changer_serial":"c", "drive_serials":["dc"]
        }));
        let desired = LibrariesConfig::parse(&value).unwrap();
        assert!(desired.add_libraries_offline(&root).is_err());
        let raft = rusqlite::Connection::open(root.join("raft.db")).unwrap();
        raft.execute_batch("CREATE TABLE hardstate(id INTEGER PRIMARY KEY, data BLOB NOT NULL); CREATE TABLE entries(idx INTEGER PRIMARY KEY, data BLOB NOT NULL)")
            .unwrap();
        assert!(desired.add_libraries_offline(&root).is_err());
        let mut hard = raft::eraftpb::HardState::new();
        hard.set_commit(42);
        raft.execute(
            "INSERT INTO hardstate VALUES(1,?1)",
            [hard.write_to_bytes().unwrap()],
        )
        .unwrap();
        raft.execute("INSERT INTO entries VALUES(43,?1)", [b"pending".as_slice()])
            .unwrap();
        assert!(desired.add_libraries_offline(&root).is_err());
        assert!(!root.join("libraries-addition.json").exists());
        old.check_manifest(&root).unwrap();
        raft.execute("DELETE FROM entries WHERE idx=43", [])
            .unwrap();
        drop(raft);
        assert!(desired.check_manifest(&root).is_err());
        desired.add_libraries_offline(&root).unwrap();
        // 扩驱版本仍可读取此前只记录新增库的扩容链。
        let marker = root.join("libraries-addition.json");
        let mut legacy_marker: Value =
            serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        legacy_marker["steps"][0]
            .as_object_mut()
            .unwrap()
            .remove("drive_additions");
        std::fs::write(&marker, legacy_marker.to_string()).unwrap();
        let new = desired.clone().load_additions(&root).unwrap();
        assert_eq!(new.additions, BTreeSet::from(["c".to_owned()]));
        new.check_manifest(&root).unwrap();
        let mut later_value = new.value();
        later_value["libraries"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "changer_serial":"d", "drive_serials":["dd"]
            }));
        let later = LibrariesConfig::parse(&later_value).unwrap();
        std::fs::create_dir_all(root.join("libraries")).unwrap();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::os::unix::fs::symlink(root.join("empty"), root.join("libraries/d")).unwrap();
        assert!(later.add_libraries_offline(&root).is_err());
        std::fs::remove_file(root.join("libraries/d")).unwrap();
        later.add_libraries_offline(&root).unwrap();
        let later = later.load_additions(&root).unwrap();
        assert_eq!(
            later.additions,
            BTreeSet::from(["c".to_owned(), "d".to_owned()])
        );
        let old_snapshot = ClusterState::new(&old).snapshot(1, 42);
        assert!(ClusterState::restore(&desired, &old_snapshot).is_err());
        let (restored, _, _) = ClusterState::restore(&later, &old_snapshot).unwrap();
        assert!(restored.controls.contains_key("c"));
        assert!(restored.controls.contains_key("d"));
        assert!(ClusterState::restore(&later, &ClusterState::new(&old).snapshot(1, 43)).is_err());

        let mut source = BTreeMap::new();
        for id in ["a", "b"] {
            source.insert(id.into(), running(&root.join("source"), id));
        }
        advance_directories(&mut source, 42).unwrap();
        let bundle = root.join("bundle.db");
        write_bundle(&bundle, &source, 42).unwrap();
        let mut target = BTreeMap::new();
        for id in ["a", "b", "c", "d"] {
            target.insert(id.into(), running(&root.join("target"), id));
        }
        assert!(install_bundle(&bundle, &mut target, 42, &BTreeMap::new()).is_err());
        assert!(install_bundle(&bundle, &mut target, 43, &later.addition_floors).is_err());
        install_bundle(&bundle, &mut target, 42, &later.addition_floors).unwrap();
        assert_eq!(target["c"].directory.as_ref().unwrap().applied_index(), 42);
        assert_eq!(target["d"].directory.as_ref().unwrap().applied_index(), 42);
        assert!(
            target["c"]
                .directory
                .as_ref()
                .unwrap()
                .pools()
                .unwrap()
                .is_empty()
        );

        let mut changed = old.value();
        changed["libraries"][0]["drive_serials"] = json!(["different"]);
        let changed = LibrariesConfig::parse(&changed).unwrap();
        assert!(changed.add_libraries_offline(&root).is_err());
        drop(target);
        drop(source);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn offline_drive_addition_keeps_old_control_snapshot_and_rejects_replacement() {
        let root = std::env::temp_dir().join(format!("multi-drive-add-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let old = config();
        old.check_manifest(&root).unwrap();
        let raft = rusqlite::Connection::open(root.join("raft.db")).unwrap();
        raft.execute_batch("CREATE TABLE hardstate(id INTEGER PRIMARY KEY, data BLOB NOT NULL); CREATE TABLE entries(idx INTEGER PRIMARY KEY, data BLOB NOT NULL)")
            .unwrap();
        let mut hard = raft::eraftpb::HardState::new();
        hard.set_commit(42);
        raft.execute(
            "INSERT INTO hardstate VALUES(1,?1)",
            [hard.write_to_bytes().unwrap()],
        )
        .unwrap();
        let mut value = old.value();
        value["libraries"][0]["drive_serials"] = json!(["da", "da2"]);
        let desired = LibrariesConfig::parse(&value).unwrap();
        raft.execute("INSERT INTO entries VALUES(43,?1)", [b"pending".as_slice()])
            .unwrap();
        assert!(desired.add_libraries_offline(&root).is_err());
        old.check_manifest(&root).unwrap();
        raft.execute("DELETE FROM entries WHERE idx=43", [])
            .unwrap();
        drop(raft);
        assert_eq!(desired.add_libraries_offline(&root).unwrap(), 42);
        let expanded = desired.load_additions(&root).unwrap();
        expanded.check_manifest(&root).unwrap();
        assert!(expanded.additions.is_empty());
        let snapshot = ClusterState::new(&old).snapshot(1, 42);
        ClusterState::restore(&expanded, &snapshot).unwrap();
        let mut replaced = expanded.value();
        replaced["libraries"][0]["drive_serials"] = json!(["da2"]);
        assert!(
            LibrariesConfig::parse(&replaced)
                .unwrap()
                .add_libraries_offline(&root)
                .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_offline_drive_replacement_preserves_policy_and_invalidates_old_inventory() {
        let root =
            std::env::temp_dir().join(format!("multi-drive-replace-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let old = config();
        old.check_manifest(&root).unwrap();
        let mut value = old.value();
        value["libraries"][0]["drive_serials"] = json!(["da2"]);
        let desired = LibrariesConfig::parse(&value).unwrap();
        assert!(desired.replace_drive_offline(&root, "da", "da2").is_err());
        let raft = rusqlite::Connection::open(root.join("raft.db")).unwrap();
        raft.execute_batch("CREATE TABLE hardstate(id INTEGER PRIMARY KEY, data BLOB NOT NULL); CREATE TABLE entries(idx INTEGER PRIMARY KEY, data BLOB NOT NULL)")
            .unwrap();
        let mut hard = raft::eraftpb::HardState::new();
        hard.set_commit(42);
        raft.execute(
            "INSERT INTO hardstate VALUES(1,?1)",
            [hard.write_to_bytes().unwrap()],
        )
        .unwrap();
        drop(raft);
        assert!(desired.replace_drive_offline(&root, "db", "da2").is_err());
        let raft = rusqlite::Connection::open(root.join("raft.db")).unwrap();
        raft.execute("INSERT INTO entries VALUES(43,?1)", [vec![1u8]])
            .unwrap();
        assert!(desired.replace_drive_offline(&root, "da", "da2").is_err());
        raft.execute("DELETE FROM entries WHERE idx=43", [])
            .unwrap();
        drop(raft);
        assert_eq!(
            desired.replace_drive_offline(&root, "da", "da2").unwrap(),
            42
        );
        desired.check_manifest(&root).unwrap();
        let changed = desired.clone().load_additions(&root).unwrap();
        assert_eq!(changed.drive_replacements.len(), 1);
        let mut before = ClusterState::new(&old);
        before.controls.get_mut("a").unwrap().management = json!({
            "pools":{},"tapes":{},"drives":{"da":{"enabled":false,"role":"rg","nodegroup":"G1"}},
            "nodegroups":["G0","G1"],"nodes":{},"tasks":{},
            "inventory":{"round":7,"drives":[{"serial":"da","barcode":null}]}
        });
        let (mut restored, _, _) =
            ClusterState::restore(&changed, &before.snapshot(1, 42)).unwrap();
        assert_eq!(
            restored.controls["a"].management["drives"]["da2"],
            json!({"enabled":false,"role":"rg","nodegroup":"G1"})
        );
        assert!(
            restored.controls["a"].management["drives"]
                .get("da")
                .is_none()
        );
        assert!(restored.controls["a"].management["inventory"].is_null());
        assert_eq!(restored.controls["b"].management, Value::Null);
        // 更换前日志里同一驱动的后续策略修改仍合并到新序列号。
        restored.controls.get_mut("a").unwrap().management["drives"]["da"] =
            json!({"enabled":true});
        restored.reconcile_drive_replacements("a").unwrap();
        assert_eq!(
            restored.controls["a"].management["drives"]["da2"]["enabled"],
            true
        );
        assert_eq!(
            restored.controls["a"].management["drives"]["da2"]["role"],
            "rg"
        );
        // 新一轮库存不应被旧迁移步骤清空。
        restored.controls.get_mut("a").unwrap().management["inventory"] =
            json!({"round":43,"drives":[{"serial":"da2","barcode":null}]});
        restored.reconcile_drive_replacements("a").unwrap();
        assert_eq!(restored.controls["a"].management["inventory"]["round"], 43);

        let mut invalid = changed.value();
        invalid["libraries"][0]["drive_serials"] = json!(["da2", "da3"]);
        assert!(
            LibrariesConfig::parse(&invalid)
                .unwrap()
                .replace_drive_offline(&root, "da2", "da3")
                .is_err()
        );
        let mut second_value = changed.value();
        second_value["libraries"][0]["drive_serials"] = json!(["da3"]);
        let second = LibrariesConfig::parse(&second_value).unwrap();
        assert_eq!(
            second.replace_drive_offline(&root, "da2", "da3").unwrap(),
            42
        );
        let second = second.load_additions(&root).unwrap();
        assert_eq!(second.drive_replacements.len(), 2);
        let (from_old, _, _) = ClusterState::restore(&second, &before.snapshot(1, 42)).unwrap();
        assert_eq!(
            from_old.controls["a"].management["drives"]["da3"]["enabled"],
            false
        );
        let (from_mid, _, _) = ClusterState::restore(&second, &restored.snapshot(1, 43)).unwrap();
        assert_eq!(
            from_mid.controls["a"].management["drives"]["da3"]["role"],
            "rg"
        );
        assert!(from_mid.controls["a"].management["inventory"].is_null());
        let marker = root.join("libraries-addition.json");
        let original = std::fs::read(&marker).unwrap();
        let mut forged: Value = serde_json::from_slice(&original).unwrap();
        forged["steps"][1]["before_index"] = json!(41);
        std::fs::write(&marker, forged.to_string()).unwrap();
        assert!(second.clone().load_additions(&root).is_err());
        forged = serde_json::from_slice(&original).unwrap();
        forged["steps"][0]["drive_replacements"] = json!({"a":{"db":"da2"}});
        std::fs::write(&marker, forged.to_string()).unwrap();
        assert!(second.clone().load_additions(&root).is_err());
        std::fs::write(&marker, original).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn explicit_offline_drive_removal_keeps_library_and_replays_without_removed_policy() {
        let root =
            std::env::temp_dir().join(format!("multi-drive-remove-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let old = LibrariesConfig::parse(&json!({
            "default_library":"a",
            "libraries":[
                {"changer_serial":"a","drive_serials":["da1","da2"]},
                {"changer_serial":"b","drive_serials":["db"]}
            ]
        }))
        .unwrap();
        old.check_manifest(&root).unwrap();
        let mut next_value = old.value();
        next_value["libraries"][0]["drive_serials"] = json!(["da2"]);
        let next = LibrariesConfig::parse(&next_value).unwrap();
        assert!(next.remove_drive_offline(&root, "da1").is_err());
        let raft = rusqlite::Connection::open(root.join("raft.db")).unwrap();
        raft.execute_batch("CREATE TABLE hardstate(id INTEGER PRIMARY KEY, data BLOB NOT NULL); CREATE TABLE entries(idx INTEGER PRIMARY KEY, data BLOB NOT NULL)").unwrap();
        let mut hard = raft::eraftpb::HardState::new();
        hard.set_commit(42);
        raft.execute(
            "INSERT INTO hardstate VALUES(1,?1)",
            [hard.write_to_bytes().unwrap()],
        )
        .unwrap();
        raft.execute("INSERT INTO entries VALUES(43,?1)", [vec![1u8]])
            .unwrap();
        drop(raft);
        assert!(next.remove_drive_offline(&root, "da1").is_err());
        let raft = rusqlite::Connection::open(root.join("raft.db")).unwrap();
        raft.execute("DELETE FROM entries WHERE idx=43", [])
            .unwrap();
        drop(raft);
        assert!(next.remove_drive_offline(&root, "da2").is_err());
        let mut illegal_value = next.value();
        illegal_value["libraries"][0]["drive_serials"] = json!(["da2", "da3"]);
        assert!(
            LibrariesConfig::parse(&illegal_value)
                .unwrap()
                .remove_drive_offline(&root, "da1")
                .is_err()
        );
        assert_eq!(next.remove_drive_offline(&root, "da1").unwrap(), 42);
        next.check_manifest(&root).unwrap();
        let changed = next.clone().load_additions(&root).unwrap();
        assert_eq!(changed.drive_removals.len(), 1);
        let mut before = ClusterState::new(&old);
        before.controls.get_mut("a").unwrap().management = json!({
            "pools":{},"tapes":{},"drives":{"da1":{"enabled":false},"da2":{"role":"write"}},
            "nodegroups":[],"nodes":{},"tasks":{"1":{"id":1,"request":{"resource":"drives","action":"down","target":"da1","params":{}},"state":"queued","cancel_requested":false}},
            "inventory":{"round":7,"drives":[{"serial":"da1"},{"serial":"da2"}]}
        });
        let (mut restored, _, _) =
            ClusterState::restore(&changed, &before.snapshot(1, 42)).unwrap();
        assert!(
            restored.controls["a"].management["drives"]
                .get("da1")
                .is_none()
        );
        assert_eq!(
            restored.controls["a"].management["drives"]["da2"]["role"],
            "write"
        );
        assert!(restored.controls["a"].management["inventory"].is_null());
        assert_eq!(
            restored.controls["a"].management["tasks"]["1"]["state"],
            "indeterminate"
        );
        assert!(restored.controls.contains_key("b"));
        restored.controls.get_mut("a").unwrap().management["drives"]["da1"] =
            json!({"enabled":true});
        restored.reconcile_drive_replacements("a").unwrap();
        assert!(
            restored.controls["a"].management["drives"]
                .get("da1")
                .is_none()
        );
        restored.controls.get_mut("a").unwrap().management["inventory"] =
            json!({"round":43,"drives":[{"serial":"da2"}]});
        restored.reconcile_drive_replacements("a").unwrap();
        assert_eq!(restored.controls["a"].management["inventory"]["round"], 43);
        let mut reused = changed.value();
        reused["libraries"][0]["drive_serials"] = json!(["da1", "da2"]);
        assert!(
            LibrariesConfig::parse(&reused)
                .unwrap()
                .add_libraries_offline(&root)
                .is_err()
        );
        assert!(
            LibrariesConfig::parse(&reused)
                .unwrap()
                .replace_drive_offline(&root, "da2", "da1")
                .is_err()
        );
        let mut forged: Value =
            serde_json::from_slice(&std::fs::read(root.join("libraries-addition.json")).unwrap())
                .unwrap();
        forged["steps"][0]["drive_removals"] = json!({"b":["db"]});
        std::fs::write(root.join("libraries-addition.json"), forged.to_string()).unwrap();
        assert!(changed.load_additions(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn scoped_wire_and_snapshots_keep_duplicate_resources_separate() {
        let cfg = config();
        let mut state = ClusterState::new(&cfg);
        for (i, id) in ["a", "b"].into_iter().enumerate() {
            let cmd = Command::PoolCreate {
                uuid: "same".into(),
                name: format!("pool-{id}"),
                file_limit: 42,
            };
            let bytes = encode_command(id, &cmd);
            let (scope, decoded) = decode_command(&bytes, "wrong-default").unwrap();
            assert_eq!(scope, id);
            assert_eq!(decoded, cmd);
            state
                .controls
                .get_mut(id)
                .unwrap()
                .apply(i as u64 + 1, &decoded);
        }
        let (restored, source, index) = ClusterState::restore(&cfg, &state.snapshot(7, 9)).unwrap();
        assert_eq!((source, index), (7, 9));
        assert_eq!(state.controls, restored.controls);
        assert_eq!(restored.controls["a"].pools["same"].name, "pool-a");
        assert_eq!(restored.controls["b"].pools["same"].name, "pool-b");
        assert!(decode_command(br#"{"cluster_version":3}"#, "a").is_err());
        assert!(
            decode_command(
                br#"{"cluster_version":2,"library":"a","command":{"unknown":1}}"#,
                "a"
            )
            .is_err()
        );
        let legacy = Command::PoolCreate {
            uuid: "old".into(),
            name: "legacy".into(),
            file_limit: 5,
        };
        assert_eq!(
            decode_command(&legacy.encode(), "a").unwrap(),
            ("a".into(), legacy)
        );
    }
    #[test]
    fn legacy_snapshot_maps_only_to_explicit_default_and_preserves_vip_barrier() {
        let cfg = config();
        let mut control = ControlState::default();
        control.apply(
            1,
            &Command::PoolCreate {
                uuid: "legacy".into(),
                name: "old".into(),
                file_limit: 1,
            },
        );
        control.apply(2, &Command::Takeover { node: 3, term: 1 });
        control.apply(
            3,
            &Command::VipAcquire {
                address: "10.0.0.9".parse().unwrap(),
                node: 3,
                round: 2,
                expected_token: 0,
            },
        );
        let bytes = ControlSnapshot {
            control: control.clone(),
            source: 3,
            directory_index: 3,
        }
        .encode();
        let (restored, _, _) = ClusterState::restore(&cfg, &bytes).unwrap();
        assert_eq!(restored.controls["a"], control);
        assert!(restored.controls["b"].pools.is_empty());
        assert_eq!(restored.controls[GATEWAY].vips, control.vips);
    }
    #[test]
    fn legacy_vip_acquired_after_the_snapshot_keeps_its_withdrawal_barrier() {
        let mut state = ClusterState::new(&config());
        let mut running = BTreeMap::new();
        apply_entry(
            &mut state,
            &mut running,
            9,
            &Command::Takeover { node: 71, term: 2 }.encode(),
            false,
        )
        .unwrap();
        let address = "10.0.0.9".parse().unwrap();
        apply_entry(
            &mut state,
            &mut running,
            10,
            &Command::VipAcquire {
                address,
                node: 71,
                round: 9,
                expected_token: 0,
            }
            .encode(),
            false,
        )
        .unwrap();
        assert_eq!(
            state.controls[GATEWAY].vips.get(&address),
            state.controls["a"].vips.get(&address)
        );
        assert_eq!(
            state.controls[GATEWAY].vips[&address]
                .holder
                .as_ref()
                .unwrap()
                .node,
            71
        );
        assert!(state.controls[GATEWAY].executor.is_none());
        apply_entry(
            &mut state,
            &mut running,
            11,
            &Command::VipRelease {
                address,
                node: 71,
                round: 9,
                token: 10,
            }
            .encode(),
            false,
        )
        .unwrap();
        assert!(state.controls[GATEWAY].vips[&address].holder.is_none());
    }
    fn running(root: &Path, id: &str) -> Running {
        std::fs::create_dir_all(root).unwrap();
        let (exec, _rx) = mpsc::channel();
        let coordination = CoordinationConfig {
            group: id.into(),
            validity: Duration::from_secs(6),
        };
        let coordinator =
            Coordinator::new(1, 1, coordination.clone(), Duration::from_secs(1), &exec).unwrap();
        let path = root.join(format!("{id}.db"));
        let directory = Directory::open(&path).unwrap();
        let runtime = LibraryRuntime {
            id: id.into(),
            group: id.into(),
            drives: 1,
            directory_file: Some(path.clone()),
            exec,
            status: Arc::new(Mutex::new(NodeStatus::default())),
            files: FileService::new(root.join(format!("{id}-spool"))).unwrap(),
        };
        let cfg = NodeConfig {
            id: 1,
            peers: vec![1, 2, 3],
            tick: Duration::from_millis(100),
            election_ticks: 10,
            heartbeat_ticks: 3,
            status_file: None,
            directory_file: Some(path),
            cooldown: Duration::from_secs(1),
            snapshot: SnapshotPolicy::default(),
            shutdown_grace: Duration::from_secs(1),
            drives: 1,
            coordination: Some(coordination),
        };
        Running {
            runtime,
            coordinator,
            directory: Some(directory),
            catalog_job: None,
            directory_queue: VecDeque::new(),
            cfg,
            local: String::new(),
            last_status: String::new(),
        }
    }

    #[test]
    fn catalog_freeze_is_scoped_to_one_library_and_fences_sqlite_projection() {
        let root = std::env::temp_dir().join(format!("library-freeze-{}", uuid::Uuid::new_v4()));
        let mut running: BTreeMap<_, _> = ["a", "b"]
            .into_iter()
            .map(|id| (id.to_owned(), running(&root, id)))
            .collect();
        let files = running["a"].runtime.files.clone();
        let index = crate::ltfs::index::LtfsIndex::empty(uuid::Uuid::new_v4(), "test".into(), 'b');
        files.open(
            1,
            crate::daemon::files::TapeIdent {
                pool_uuid: "p".into(),
                barcode: "TEST01L8".into(),
            },
            crate::daemon::files::TapeLimits {
                file_limit: 100,
                usable_capacity: 100,
            },
            &index,
            100,
            true,
        );
        let mut state = ClusterState::new(&config());
        let apply = |state: &mut ClusterState,
                     running: &mut BTreeMap<String, Running>,
                     id: &str,
                     index,
                     command| {
            apply_entry(state, running, index, &encode_command(id, &command), true).unwrap()
        };
        assert!(matches!(
            apply(&mut state, &mut running, "a", 1, Command::CatalogQuiesce),
            Applied::Admin(Ok(_))
        ));
        let denied = running["a"].runtime.files.begin("/new", 0);
        let detail = format!("{:?}", denied.as_ref().err());
        assert!(
            matches!(denied, Err(crate::daemon::files::ServiceError::NotServing(reason)) if reason.contains("旧目录已暂停")),
            "{detail}"
        );
        assert!(matches!(
            apply(
                &mut state,
                &mut running,
                "a",
                2,
                Command::CatalogFreeze { quiesce_index: 1 },
            ),
            Applied::Admin(Ok(_))
        ));
        let create = Command::PoolCreate {
            uuid: "p".into(),
            name: "pool".into(),
            file_limit: 100,
        };
        assert_eq!(
            apply(&mut state, &mut running, "a", 3, create.clone()),
            Applied::CatalogRejected
        );
        assert!(matches!(
            apply(&mut state, &mut running, "b", 4, create),
            Applied::Admin(Ok(_))
        ));
        advance_directories(&mut running, 4).unwrap();
        assert!(
            running["a"]
                .directory
                .as_ref()
                .unwrap()
                .pools()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            running["b"]
                .directory
                .as_ref()
                .unwrap()
                .pools()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(state.controls["a"].catalog_frozen_index, Some(2));
        assert_eq!(state.controls["b"].catalog_frozen_index, None);
        let (restored, _, _) = ClusterState::restore(&config(), &state.snapshot(1, 4)).unwrap();
        assert_eq!(restored.controls["a"].catalog_frozen_index, Some(2));
        assert_eq!(restored.controls["b"].catalog_frozen_index, None);
        let mut shadow = ClusterState::new(&config());
        apply_entry_for_replay(
            &mut shadow,
            &mut running,
            5,
            &encode_command(
                "a",
                &Command::PoolCreate {
                    uuid: "shadow".into(),
                    name: "shadow".into(),
                    file_limit: 10,
                },
            ),
            false,
        )
        .unwrap();
        assert!(matches!(
            files.begin("/still-frozen", 0),
            Err(crate::daemon::files::ServiceError::NotServing(reason)) if reason.contains("旧目录已暂停")
        ));
        drop(running);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn bulk_merge_keeps_other_libraries_responsive_and_replays_in_order() {
        let root = std::env::temp_dir().join(format!("bulk-jobs-{}", uuid::Uuid::new_v4()));
        let mut running: BTreeMap<_, _> = ["a", "b"]
            .into_iter()
            .map(|id| (id.to_owned(), running(&root, id)))
            .collect();
        let mut state = ClusterState::new(&config());
        let apply = |state: &mut ClusterState,
                     running: &mut BTreeMap<String, Running>,
                     id: &str,
                     index,
                     command| {
            apply_entry(state, running, index, &encode_command(id, &command), true).unwrap()
        };
        apply(
            &mut state,
            &mut running,
            "b",
            1,
            Command::PoolCreate {
                uuid: "p".into(),
                name: "pool".into(),
                file_limit: 10000,
            },
        );
        apply(
            &mut state,
            &mut running,
            "b",
            2,
            Command::TapeAssign {
                barcode: "T".into(),
                pool: "p".into(),
            },
        );
        for part in 0..2 {
            let files = (0..3000)
                .map(|n| FileRec {
                    path: format!("/{part}/{n}"),
                    length: 1,
                    sha256: String::new(),
                    version: (0, 0),
                    deleted: false,
                    metadata: json!({"note":"x".repeat(64)}),
                })
                .collect();
            apply(
                &mut state,
                &mut running,
                "b",
                3 + u64::from(part),
                Command::CatalogPart {
                    barcode: "T".into(),
                    generation: 1,
                    part,
                    files,
                },
            );
        }
        // SQLite writer lock deterministically delays the real merge, without slowing Raft admission.
        let blocker = rusqlite::Connection::open(root.join("b.db")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let at = Instant::now();
        apply(
            &mut state,
            &mut running,
            "b",
            5,
            Command::TapeCommitted {
                barcode: "T".into(),
                volume_uuid: "v".into(),
                generation: 1,
                files: 6000,
                bytes_used: 6000,
                bytes_written: 6000,
                parts: 2,
                full: true,
            },
        );
        apply(
            &mut state,
            &mut running,
            "a",
            6,
            Command::PoolCreate {
                uuid: "other".into(),
                name: "other".into(),
                file_limit: 100,
            },
        );
        apply(
            &mut state,
            &mut running,
            "b",
            7,
            Command::PoolCreate {
                uuid: "queued".into(),
                name: "queued".into(),
                file_limit: 100,
            },
        );
        advance_directories(&mut running, 8).unwrap();
        assert!(
            at.elapsed() < Duration::from_millis(100),
            "bulk SQL must not execute on the caller thread"
        );
        assert!(!directory_idle(&running["b"]));
        assert_eq!(
            running["a"]
                .directory
                .as_ref()
                .unwrap()
                .pools()
                .unwrap()
                .len(),
            1
        );
        let reader = Directory::open_reader(&root.join("b.db")).unwrap();
        assert!(
            reader.stat("p", "/0/0").unwrap().is_none(),
            "uncommitted generation remains invisible"
        );
        blocker.execute_batch("COMMIT").unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !running.values().all(directory_idle) {
            poll_directories(&mut running).unwrap();
            advance_directories(&mut running, 8).unwrap();
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(1));
        }
        let b = running["b"].directory.as_ref().unwrap();
        assert!(b.stat("p", "/0/0").unwrap().is_some());
        assert!(b.stat("p", "/1/2999").unwrap().is_some());
        assert_eq!(b.pools().unwrap().len(), 2);
        assert_eq!(b.applied_index(), 8);
        drop(reader);
        drop(blocker);
        drop(running);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_backlog_catches_up_while_new_log_entries_keep_arriving() {
        let root = std::env::temp_dir().join(format!("directory-catchup-{}", uuid::Uuid::new_v4()));
        let mut running: BTreeMap<_, _> = ["a", "b"]
            .into_iter()
            .map(|id| (id.to_owned(), running(&root, id)))
            .collect();
        for index in 1..=64 {
            running.get_mut("b").unwrap().directory_queue.push_back((
                index,
                Command::Takeover { node: 0, term: 0 },
                Applied::Nothing,
            ));
        }
        let mut state = ClusterState::new(&config());
        for index in 65..=84 {
            poll_directories(&mut running).unwrap();
            apply_entry(
                &mut state,
                &mut running,
                index,
                &encode_command(
                    "b",
                    &Command::PoolCreate {
                        uuid: format!("p{index}"),
                        name: format!("p{index}"),
                        file_limit: 100,
                    },
                ),
                true,
            )
            .unwrap();
        }
        assert!(
            directory_idle(&running["b"]),
            "continuous Raft traffic must not strand the directory backlog"
        );
        assert_eq!(
            running["b"]
                .directory
                .as_ref()
                .unwrap()
                .pools()
                .unwrap()
                .len(),
            20
        );
        drop(running);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn background_snapshot_pins_its_index_before_later_directory_commands() {
        let root = std::env::temp_dir().join(format!("snapshot-jobs-{}", uuid::Uuid::new_v4()));
        let mut running: BTreeMap<_, _> = ["a", "b"]
            .into_iter()
            .map(|id| (id.to_owned(), running(&root, id)))
            .collect();
        advance_directories(&mut running, 1).unwrap();
        let path = root.join("bundle.db");
        let at = Instant::now();
        let job = start_snapshot(path.clone(), &mut running, 1, vec![1]);
        let mut state = ClusterState::new(&config());
        apply_entry(
            &mut state,
            &mut running,
            2,
            &encode_command(
                "a",
                &Command::PoolCreate {
                    uuid: "later".into(),
                    name: "later".into(),
                    file_limit: 100,
                },
            ),
            true,
        )
        .unwrap();
        assert!(at.elapsed() < Duration::from_millis(100));
        assert!(!directory_idle(&running["a"]));
        let directories = job
            .result
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        for (id, directory) in directories {
            running.get_mut(&id).unwrap().directory = Some(directory);
        }
        poll_directories(&mut running).unwrap();
        advance_directories(&mut running, 2).unwrap();
        assert_eq!(
            running["a"]
                .directory
                .as_ref()
                .unwrap()
                .pools()
                .unwrap()
                .len(),
            1
        );
        let mut target: BTreeMap<_, _> = ["a", "b"]
            .into_iter()
            .map(|id| (id.to_owned(), self::running(&root.join("target"), id)))
            .collect();
        install_bundle(&path, &mut target, job.index, &BTreeMap::new()).unwrap();
        assert!(
            target["a"]
                .directory
                .as_ref()
                .unwrap()
                .pools()
                .unwrap()
                .is_empty()
        );
        assert_eq!(target["a"].directory.as_ref().unwrap().applied_index(), 1);
        drop(target);
        drop(running);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn directory_bundle_restores_all_libraries_and_rejects_wrong_scope_before_install() {
        let root = std::env::temp_dir().join(format!("multi-bundle-{}", uuid::Uuid::new_v4()));
        let mut source = BTreeMap::new();
        let mut target = BTreeMap::new();
        for id in ["a", "b"] {
            let mut r = running(&root.join("source"), id);
            let directory = r.directory.as_mut().unwrap();
            directory
                .apply(
                    1,
                    &Command::PoolCreate {
                        uuid: "same".into(),
                        name: format!("pool-{id}"),
                        file_limit: 100,
                    },
                    &Applied::Admin(Ok("ok".into())),
                )
                .unwrap();
            if id == "a" {
                use super::super::state::FileRec;
                directory
                    .apply(
                        2,
                        &Command::TapeAssign {
                            barcode: "same-tape".into(),
                            pool: "same".into(),
                        },
                        &Applied::Admin(Ok("ok".into())),
                    )
                    .unwrap();
                directory
                    .apply(
                        3,
                        &Command::CatalogPart {
                            barcode: "same-tape".into(),
                            generation: 1,
                            part: 0,
                            files: vec![FileRec {
                                path: "/large-metadata".into(),
                                length: 1,
                                sha256: String::new(),
                                version: (1, 1),
                                deleted: false,
                                metadata: json!({"value":"x".repeat(2<<20)}),
                            }],
                        },
                        &Applied::Nothing,
                    )
                    .unwrap();
                directory
                    .apply(
                        4,
                        &Command::TapeCommitted {
                            barcode: "same-tape".into(),
                            volume_uuid: "v".into(),
                            generation: 1,
                            files: 1,
                            bytes_used: 1,
                            bytes_written: 1,
                            parts: 1,
                            full: true,
                        },
                        &Applied::CatalogCommitted,
                    )
                    .unwrap();
            }
            source.insert(id.to_owned(), r);
            target.insert(id.to_owned(), running(&root.join("target"), id));
        }
        advance_directories(&mut source, 42).unwrap();
        let bundle = root.join("bundle.db");
        write_bundle(&bundle, &source, 42).unwrap();
        assert_eq!(Directory::applied_index_of(&bundle).unwrap(), 42);
        // 模拟传输头读取后源快照已被新版替换；必须按新版文件索引安装和等待日志追齐。
        assert_eq!(verified_bundle_index(&bundle, 40, 41).unwrap(), 42);
        assert!(verified_bundle_index(&bundle, 43, 41).is_err());
        assert!(verified_bundle_index(&bundle, 40, 43).is_err());
        install_bundle(&bundle, &mut target, 42, &BTreeMap::new()).unwrap();
        for (id, r) in &target {
            assert_eq!(
                r.directory.as_ref().unwrap().pools().unwrap()[0].name,
                format!("pool-{id}")
            );
            assert_eq!(r.directory.as_ref().unwrap().applied_index(), 42);
        }
        assert_eq!(
            target["a"]
                .directory
                .as_ref()
                .unwrap()
                .stat("same", "/large-metadata")
                .unwrap()
                .unwrap()
                .metadata["value"]
                .as_str()
                .unwrap()
                .len(),
            2 << 20
        );
        let db = rusqlite::Connection::open(&bundle).unwrap();
        assert!(
            db.query_row("SELECT count(*) FROM libraries WHERE id='a'", [], |r| r
                .get::<_, usize>(
                0
            ))
            .unwrap()
                > 1
        );
        db.execute("DELETE FROM libraries WHERE id='a' AND part=1", [])
            .unwrap();
        drop(db);
        assert!(install_bundle(&bundle, &mut target, 42, &BTreeMap::new()).is_err());
        assert_eq!(
            target["a"]
                .directory
                .as_ref()
                .unwrap()
                .stat("same", "/large-metadata")
                .unwrap()
                .unwrap()
                .metadata["value"]
                .as_str()
                .unwrap()
                .len(),
            2 << 20
        );
        write_bundle(&bundle, &source, 42).unwrap();
        let db = rusqlite::Connection::open(&bundle).unwrap();
        db.execute("DELETE FROM libraries WHERE id='b'", [])
            .unwrap();
        drop(db);
        assert!(install_bundle(&bundle, &mut target, 42, &BTreeMap::new()).is_err());
        assert_eq!(
            target["b"].directory.as_ref().unwrap().pools().unwrap()[0].name,
            "pool-b"
        );
        drop((source, target));
        std::fs::remove_dir_all(root).unwrap();
    }
}
