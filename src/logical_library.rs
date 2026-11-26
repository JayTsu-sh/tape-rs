//! The resource selection shared by the logical library API and its client.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const MAX_RESOURCES: usize = 4096;
pub(crate) const MAX_REQUEST_BYTES: usize = 90_000;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LogicalLibraryResources {
    pub drives: Vec<String>,
    /// Absolute addresses in the physical library's storage element range.
    pub storage_slots: Vec<u16>,
    /// Absolute addresses in the physical library's import/export element range.
    pub ie_slots: Vec<u16>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CreateLogicalLibrary {
    pub version: u8,
    pub id: Uuid,
    pub name: String,
    pub resources: LogicalLibraryResources,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceLogicalLibrary {
    pub name: String,
    pub resources: LogicalLibraryResources,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeLogicalLibrary {
    pub version: u8,
    pub operation: Uuid,
    pub revision: u64,
    pub replacement: Option<ReplaceLogicalLibrary>,
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !matches!(name, "." | "..")
        && name.trim() == name
        && !name.contains('/')
        && !name.chars().any(char::is_control)
        && Uuid::parse_str(name).is_err()
}

impl CreateLogicalLibrary {
    pub(crate) fn normalize(&mut self) -> Result<(), &'static str> {
        if self.version != 1 || self.id.is_nil() || !valid_name(&self.name) {
            return Err("逻辑带库版本、UUID 或名称无效");
        }
        let resources = &mut self.resources;
        let count =
            resources.drives.len() + resources.storage_slots.len() + resources.ie_slots.len();
        if !(1..=MAX_RESOURCES).contains(&count)
            || resources.drives.iter().any(|drive| {
                drive.is_empty()
                    || drive.len() > 255
                    || drive.contains('/')
                    || drive.chars().any(|c| c.is_whitespace() || c.is_control())
            })
        {
            return Err("逻辑带库资源数量或驱动器序列号无效");
        }
        fn unique<T: Ord>(items: &mut [T]) -> bool {
            items.sort_unstable();
            !items.windows(2).any(|pair| pair[0] == pair[1])
        }
        if !unique(&mut resources.drives)
            || !unique(&mut resources.storage_slots)
            || !unique(&mut resources.ie_slots)
            || resources
                .storage_slots
                .iter()
                .any(|slot| resources.ie_slots.binary_search(slot).is_ok())
        {
            return Err("逻辑带库请求含重复资源");
        }
        Ok(())
    }
}
