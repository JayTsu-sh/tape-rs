//! Fixed-size immutable history nodes. These encode scheduling locations,
//! never media evidence or publication/format authorization.
use super::*;
use crate::ltfs::recovery::{
    FULL_HISTORY_ENTRY_BYTES, FULL_HISTORY_MANIFEST_BYTES, FullHistoryEntry, FullHistoryManifest,
};

pub(super) const NODE_BASE_BYTES: usize = 3 + FULL_HISTORY_MANIFEST_BYTES;
pub(super) const NODE_APPEND_BYTES: usize =
    3 + 32 + 32 + FULL_HISTORY_MANIFEST_BYTES + FULL_HISTORY_ENTRY_BYTES;

fn invalid() -> CatalogError {
    CatalogError::Corrupt("目的历史提示对象、父指针或单后继语义无效")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Root {
    pub(super) id: [u8; 32],
    pub(super) semantic: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Node {
    Base(FullHistoryManifest),
    Append {
        parent: Root,
        manifest: FullHistoryManifest,
        entry: FullHistoryEntry,
    },
}

impl Node {
    pub(super) fn manifest(self) -> FullHistoryManifest {
        match self {
            Self::Base(manifest) | Self::Append { manifest, .. } => manifest,
        }
    }

    pub(super) fn encode(self) -> Vec<u8> {
        match self {
            Self::Base(manifest) => {
                let mut bytes = Vec::with_capacity(NODE_BASE_BYTES);
                bytes.extend_from_slice(b"HB1");
                bytes.extend_from_slice(&manifest.to_bytes());
                bytes
            }
            Self::Append {
                parent,
                manifest,
                entry,
            } => {
                let mut bytes = Vec::with_capacity(NODE_APPEND_BYTES);
                bytes.extend_from_slice(b"HA1");
                bytes.extend_from_slice(&parent.id);
                bytes.extend_from_slice(&parent.semantic);
                bytes.extend_from_slice(&manifest.to_bytes());
                bytes.extend_from_slice(&entry.to_bytes());
                bytes
            }
        }
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        let node = match bytes.get(..3) {
            Some(b"HB1") if bytes.len() == NODE_BASE_BYTES => {
                Self::Base(FullHistoryManifest::from_bytes(&bytes[3..]).map_err(|_| invalid())?)
            }
            Some(b"HA1") if bytes.len() == NODE_APPEND_BYTES => Self::Append {
                parent: Root {
                    id: bytes[3..35].try_into().unwrap(),
                    semantic: bytes[35..67].try_into().unwrap(),
                },
                manifest: FullHistoryManifest::from_bytes(
                    &bytes[67..67 + FULL_HISTORY_MANIFEST_BYTES],
                )
                .map_err(|_| invalid())?,
                entry: FullHistoryEntry::from_bytes(&bytes[67 + FULL_HISTORY_MANIFEST_BYTES..])
                    .map_err(|_| invalid())?,
            },
            _ => return Err(invalid()),
        };
        if let Self::Append {
            parent,
            manifest,
            entry,
        } = node
        {
            if parent.id == [0; 32]
                || parent.semantic == [0; 32]
                || manifest.entries < 2
                || entry.generation != manifest.head.generation
                || entry.start_block != manifest.head.location.start_block
                || entry.sha256 != manifest.head.sha256
                || entry.end_block != manifest.dp_eod
                || entry.previous.is_none_or(|previous| {
                    previous.partition != manifest.data_partition
                        || previous.start_block >= entry.start_block
                })
            {
                return Err(invalid());
            }
        }
        Ok(node)
    }

    pub(super) fn root(self) -> Root {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/reclaim-destination-history/node/1");
        hash.update(self.encode());
        Root {
            id: hash.finalize().into(),
            semantic: self.manifest().fingerprint(),
        }
    }

    pub(super) fn require_root(self, root: Root) -> Result<Self> {
        if self.root() != root {
            return Err(invalid());
        }
        Ok(self)
    }

    pub(super) fn require_parent(self, parent_node: Node) -> Result<()> {
        let Self::Append {
            parent,
            manifest,
            entry,
        } = self
        else {
            return Err(invalid());
        };
        let old = parent_node.manifest();
        if parent != parent_node.root()
            || old.entries.checked_add(1) != Some(manifest.entries)
            || old.head.volume_uuid != manifest.head.volume_uuid
            || old.block_size != manifest.block_size
            || old.index_partition != manifest.index_partition
            || old.data_partition != manifest.data_partition
            || manifest.head.generation < old.head.generation
            || entry.previous != Some(old.head.location)
            || entry.start_block <= old.head.location.start_block
            || old.dp_eod > entry.start_block
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Staging {
    pub(super) node: Vec<u8>,
    pub(super) expected_current: Option<Root>,
    pub(super) next_chunk: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Retired {
    pub(super) root: Root,
    /// Complete initial root count is retained as a strict finite GC bound.
    pub(super) entries_left: u64,
    /// Base chunks are deleted in fixed-size pages before its node.
    pub(super) next_chunk: u64,
    pub(super) collecting: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    version: u8,
    pub(super) current: Option<Root>,
    pub(super) staging: Option<Staging>,
    pub(super) retired: Option<Retired>,
    pub(super) pins: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            version: 1,
            current: None,
            staging: None,
            retired: None,
            pins: 0,
        }
    }
}

impl State {
    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        if self.version != 1 {
            return Err(invalid());
        }
        if let Some(stage) = &self.staging {
            let node = Node::decode(&stage.node)?;
            if stage.next_chunk > node.manifest().entries.div_ceil(super::ENTRIES_PER_CHUNK)
                || stage.expected_current != self.current
                || matches!(node, Node::Append { .. }) && stage.next_chunk != 0
            {
                return Err(invalid());
            }
        }
        if let Some(retired) = &self.retired {
            if self.current == Some(retired.root)
                || retired.entries_left == 0
                || retired.entries_left > u64::from(u32::MAX) + 1
                || retired.collecting && self.pins != 0
            {
                return Err(invalid());
            }
        }
        let bytes = serde_json::to_vec(self).map_err(cache)?;
        if bytes.len() > 8192 {
            return Err(invalid());
        }
        Ok(bytes)
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > 8192 {
            return Err(invalid());
        }
        let state: Self = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        if state.encode()?.as_slice() != bytes {
            return Err(invalid());
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn base() -> Node {
        Node::Base(FullHistoryManifest {
            head: CommittedIndexConstruct {
                volume_uuid: Uuid::from_u128(1),
                generation: 4,
                location: crate::ltfs::index::IndexLocation {
                    partition: 'b',
                    start_block: 20,
                },
                sha256: [1; 32],
                incremental: false,
            },
            dp_eod: 22,
            entries: 4,
            block_size: 65536,
            index_partition: 'a',
            data_partition: 'b',
            entries_sha256: [2; 32],
        })
    }
    fn append(old: Node) -> Node {
        let old_manifest = old.manifest();
        let mut manifest = old_manifest;
        manifest.entries += 1;
        manifest.head.location.start_block += 3;
        manifest.head.generation += 1;
        manifest.head.sha256[0] += 1;
        manifest.entries_sha256[0] += 1;
        manifest.dp_eod += 3;
        let entry = FullHistoryEntry {
            generation: manifest.head.generation,
            start_block: manifest.head.location.start_block,
            sha256: manifest.head.sha256,
            end_block: manifest.dp_eod,
            previous: Some(old_manifest.head.location),
        };
        Node::Append {
            parent: old.root(),
            manifest,
            entry,
        }
    }
    #[test]
    fn immutable_base_and_append_have_constant_encoding_and_strict_links() {
        let old = base();
        let next = append(old);
        assert_eq!(old.encode().len(), NODE_BASE_BYTES);
        assert_eq!(next.encode().len(), NODE_APPEND_BYTES);
        assert_eq!(Node::decode(&old.encode()).unwrap(), old);
        assert_eq!(Node::decode(&next.encode()).unwrap(), next);
        next.require_parent(old).unwrap();
        next.require_root(next.root()).unwrap();
        assert!(next.require_root(old.root()).is_err());
        for length in [0, 1, NODE_APPEND_BYTES - 1, NODE_APPEND_BYTES + 1] {
            assert!(Node::decode(&vec![0; length]).is_err());
        }
        let Node::Append {
            parent,
            manifest,
            entry,
        } = next
        else {
            unreachable!()
        };
        let mut same = manifest;
        let mut same_entry = entry;
        same.head.generation = old.manifest().head.generation;
        same_entry.generation = same.head.generation;
        Node::Append {
            parent,
            manifest: same,
            entry: same_entry,
        }
        .require_parent(old)
        .unwrap();
        same.head.generation -= 1;
        same_entry.generation -= 1;
        assert!(
            Node::Append {
                parent,
                manifest: same,
                entry: same_entry
            }
            .require_parent(old)
            .is_err()
        );
        let mut wrong = entry;
        wrong.previous = None;
        assert!(
            Node::decode(
                &Node::Append {
                    parent,
                    manifest,
                    entry: wrong
                }
                .encode()
            )
            .is_err()
        );
        let mut overlap = entry;
        overlap.start_block = old.manifest().dp_eod - 1;
        assert!(
            Node::Append {
                parent,
                manifest,
                entry: overlap
            }
            .require_parent(old)
            .is_err()
        );
    }
    #[test]
    fn append_storage_grows_linearly_without_history_lists() {
        let mut current = base();
        let mut bytes =
            current.encode().len() + current.manifest().entries as usize * FULL_HISTORY_ENTRY_BYTES;
        let initial = bytes;
        for n in 1..=70 {
            let next = append(current);
            next.require_parent(current).unwrap();
            bytes += next.encode().len();
            assert_eq!(bytes, initial + n * NODE_APPEND_BYTES);
            current = next;
        }
        assert_eq!(current.manifest().entries, 74);
    }
    #[test]
    fn staging_and_retired_limits_preserve_pin_gc_exclusion() {
        let old = base();
        let next = append(old);
        let mut state = State::default();
        state.current = Some(old.root());
        state.staging = Some(Staging {
            node: next.encode(),
            expected_current: state.current,
            next_chunk: 0,
        });
        assert_eq!(State::decode(&state.encode().unwrap()).unwrap(), state);
        state.staging.as_mut().unwrap().next_chunk = 1;
        assert!(state.encode().is_err());
        state.staging = None;
        state.current = Some(next.root());
        state.retired = Some(Retired {
            root: old.root(),
            entries_left: old.manifest().entries,
            next_chunk: 0,
            collecting: false,
        });
        state.pins = 1;
        state.retired.as_mut().unwrap().collecting = true;
        assert!(state.encode().is_err());
        state.pins = 0;
        assert!(state.encode().is_ok());
        state.retired.as_mut().unwrap().root = next.root();
        assert!(state.encode().is_err());
    }
    #[test]
    fn reversed_append_load_uses_one_bounded_builder_and_sorted_digest() {
        let root = std::env::temp_dir().join(format!("history-append-codec-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let entries: Vec<_> = (0..3)
            .map(|n| FullHistoryEntry {
                generation: 1 + n.min(1),
                start_block: 5 + 3 * n,
                sha256: [n as u8; 32],
                end_block: 7 + 3 * n,
                previous: (n > 0).then(|| crate::ltfs::index::IndexLocation {
                    partition: 'b',
                    start_block: 2 + 3 * n,
                }),
            })
            .collect();
        let mut manifest = base().manifest();
        manifest.entries = 3;
        manifest.head.generation = 2;
        manifest.head.location.start_block = 11;
        manifest.head.sha256 = [2; 32];
        manifest.dp_eod = 13;
        let mut sha = Sha256::new();
        for entry in &entries {
            sha.update(entry.to_bytes());
        }
        manifest.entries_sha256 = sha.finalize().into();
        let mut budget = IndexScratchBudget::default();
        budget.directory = root.clone();
        budget.max_bytes = 32 << 20;
        budget.headroom_bytes = 0;
        let mut builder = FullHistoryHintBuilder::new(manifest, &budget).unwrap();
        for entry in entries.iter().rev() {
            builder.append_entries(&entry.to_bytes()).unwrap();
        }
        let directory = builder.finish().unwrap();
        let mut actual = Vec::new();
        directory
            .for_each_entry(|entry| {
                actual.push(entry);
                Ok(())
            })
            .unwrap();
        assert_eq!(actual, entries);
        assert_eq!(directory.manifest(), manifest);
        assert!(directory.reserved_bytes() <= budget.max_bytes);
        drop(directory);
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(root).unwrap();
    }
}
