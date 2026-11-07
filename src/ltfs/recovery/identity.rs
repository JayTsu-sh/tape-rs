//! Bounded identity of a raw index and its complete required delta chain.
use crate::error::{Result, TapeError};
use crate::ltfs::index::IndexLocation;
use crate::ltfs::volume::CommittedIndexConstruct;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexSourceIdentity {
    head: CommittedIndexConstruct,
    predecessors: Vec<CommittedIndexConstruct>,
}

fn invalid() -> TapeError {
    TapeError::Catalog("索引来源身份、种类或完整前驱链无效".into())
}
impl IndexSourceIdentity {
    /// An identity descriptor, not proof of physical reads or owner authority.
    pub fn new(
        head: CommittedIndexConstruct,
        predecessors: Vec<CommittedIndexConstruct>,
    ) -> Result<Self> {
        let valid = |c: &CommittedIndexConstruct| {
            !c.volume_uuid.is_nil()
                && c.generation > 0
                && matches!(c.location.partition, 'a' | 'b')
                && c.location.start_block > 0
        };
        if !valid(&head) || predecessors.len() > 64 || head.incremental == predecessors.is_empty() {
            return Err(invalid());
        }
        let mut previous = head;
        for (i, item) in predecessors.iter().enumerate() {
            if !valid(item)
                || item.volume_uuid != head.volume_uuid
                || item.location.partition != head.location.partition
                || item.location.start_block >= previous.location.start_block
                || item.generation >= previous.generation
                || item.incremental != (i + 1 < predecessors.len())
            {
                return Err(invalid());
            }
            previous = *item;
        }
        Ok(Self { head, predecessors })
    }
    pub fn head(&self) -> CommittedIndexConstruct {
        self.head
    }
    pub fn predecessors(&self) -> &[CommittedIndexConstruct] {
        &self.predecessors
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        fn put(out: &mut Vec<u8>, c: CommittedIndexConstruct) {
            out.extend_from_slice(c.volume_uuid.as_bytes());
            out.extend_from_slice(&c.generation.to_be_bytes());
            out.push(c.location.partition as u8);
            out.extend_from_slice(&c.location.start_block.to_be_bytes());
            out.extend_from_slice(&c.sha256);
            out.push(u8::from(c.incremental));
        }
        let mut out = Vec::with_capacity(68 + 66 * self.predecessors.len());
        out.push(1);
        put(&mut out, self.head);
        out.push(self.predecessors.len() as u8);
        for c in &self.predecessors {
            put(&mut out, *c);
        }
        out
    }
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        fn get(b: &[u8]) -> Result<CommittedIndexConstruct> {
            if b.len() != 66 || b[65] > 1 {
                return Err(invalid());
            }
            Ok(CommittedIndexConstruct {
                volume_uuid: uuid::Uuid::from_slice(&b[..16]).map_err(|_| invalid())?,
                generation: u64::from_be_bytes(b[16..24].try_into().unwrap()),
                location: IndexLocation {
                    partition: b[24] as char,
                    start_block: u64::from_be_bytes(b[25..33].try_into().unwrap()),
                },
                sha256: b[33..65].try_into().unwrap(),
                incremental: b[65] == 1,
            })
        }
        if bytes.len() < 68
            || bytes[0] != 1
            || bytes[67] > 64
            || bytes.len() != 68 + 66 * usize::from(bytes[67])
        {
            return Err(invalid());
        }
        Self::new(
            get(&bytes[1..67])?,
            bytes[68..]
                .as_chunks::<66>()
                .0
                .iter()
                .map(|frame| get(frame))
                .collect::<Result<Vec<_>>>()?,
        )
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"tape-rs/index-source/1");
        hash.update(self.to_bytes());
        hash.finalize().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn construct(
        volume: uuid::Uuid,
        generation: u64,
        incremental: bool,
    ) -> CommittedIndexConstruct {
        CommittedIndexConstruct {
            volume_uuid: volume,
            generation,
            location: IndexLocation {
                partition: 'b',
                start_block: generation * 10,
            },
            sha256: [generation as u8; 32],
            incremental,
        }
    }

    #[test]
    fn identity_codec_binds_kind_every_predecessor_and_exact_length() {
        let volume = uuid::Uuid::new_v4();
        let head = construct(volume, 65, true);
        let chain: Vec<_> = (1..65)
            .rev()
            .map(|n| construct(volume, n, n != 1))
            .collect();
        let identity = IndexSourceIdentity::new(head, chain.clone()).unwrap();
        let bytes = identity.to_bytes();
        assert_eq!(bytes.len(), 68 + 64 * 66);
        assert_eq!(IndexSourceIdentity::from_bytes(&bytes).unwrap(), identity);
        for end in 0..bytes.len() {
            assert!(IndexSourceIdentity::from_bytes(&bytes[..end]).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(IndexSourceIdentity::from_bytes(&extra).is_err());
        for (offset, value) in [(0, 2), (66, 2), (67, 65), (25, b'z'), (133, 0)] {
            let mut changed = bytes.clone();
            changed[offset] = value;
            assert!(
                IndexSourceIdentity::from_bytes(&changed).is_err(),
                "offset {offset}"
            );
        }
        let mut changed = chain.clone();
        changed[1].sha256[0] ^= 1;
        assert_ne!(
            identity.fingerprint(),
            IndexSourceIdentity::new(head, changed)
                .unwrap()
                .fingerprint()
        );
        assert!(IndexSourceIdentity::new(head, Vec::new()).is_err());
        let full = construct(volume, 65, false);
        assert!(IndexSourceIdentity::new(full, chain.clone()).is_err());
        assert!(IndexSourceIdentity::new(full, Vec::new()).is_ok());
        let mut changed = chain.clone();
        changed[0].volume_uuid = uuid::Uuid::new_v4();
        assert!(IndexSourceIdentity::new(head, changed).is_err());
        let mut changed = chain;
        changed[0].generation = head.generation;
        assert!(IndexSourceIdentity::new(head, changed).is_err());
        assert!(
            IndexSourceIdentity::new(
                construct(volume, 66, true),
                (1..66)
                    .rev()
                    .map(|n| construct(volume, n, n != 1))
                    .collect()
            )
            .is_err()
        );
    }
}
