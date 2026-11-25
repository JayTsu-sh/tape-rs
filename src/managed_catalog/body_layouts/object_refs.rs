//! Object lifecycle interlocks. No collector or deletion transition is enabled.
use super::*;

const OBJECT_STATE_VERSION: &[u8; 3] = b"OS1";
const OBJECT_REFERENCE_VERSION: &[u8; 3] = b"OR1";
const STATE_BYTES: usize = 3 + 1 + 8 + MANIFEST_BYTES;
const REFERENCE_BYTES: usize = 3 + 8 + 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObjectState {
    Active,
    Deleting,
    Tombstone,
}

/// An object generation is ephemeral authority only after a checked producer
/// transaction. It is never accepted as caller-supplied serialization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ObjectGeneration {
    manifest: LayoutManifest,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ObjectReference {
    Global {
        file: Uuid,
        copy: Uuid,
    },
    Import {
        original: [u8; 32],
        ordinal: u64,
        source: u32,
    },
}

fn object_state_key(namespace: Uuid, manifest: LayoutManifest) -> Vec<u8> {
    let mut key = namespace_prefix(namespace);
    key.push(OBJECT_REFS_SUBTYPE);
    key.extend_from_slice(&manifest.digest);
    key.push(0);
    key
}

fn reference_key(namespace: Uuid, object: ObjectGeneration, reference: ObjectReference) -> Vec<u8> {
    let mut key = namespace_prefix(namespace);
    key.push(OBJECT_REFS_SUBTYPE);
    key.extend_from_slice(&object.manifest.digest);
    key.push(1);
    match reference {
        ObjectReference::Global { file, copy } => {
            key.push(0);
            key.extend_from_slice(file.as_bytes());
            key.extend_from_slice(copy.as_bytes());
        }
        ObjectReference::Import {
            original,
            ordinal,
            source,
        } => {
            key.push(1);
            key.extend_from_slice(&original);
            key.extend_from_slice(&ordinal.to_be_bytes());
            key.extend_from_slice(&source.to_be_bytes());
        }
    }
    key
}

fn encode_state(object: ObjectGeneration, state: ObjectState) -> [u8; STATE_BYTES] {
    let mut bytes = [0; STATE_BYTES];
    bytes[..3].copy_from_slice(OBJECT_STATE_VERSION);
    bytes[3] = match state {
        ObjectState::Active => 0,
        ObjectState::Deleting => 1,
        ObjectState::Tombstone => 2,
    };
    bytes[4..12].copy_from_slice(&object.generation.to_be_bytes());
    bytes[12..].copy_from_slice(&object.manifest.encode());
    bytes
}

fn decode_state(raw: &[u8], manifest: LayoutManifest) -> Result<(ObjectGeneration, ObjectState)> {
    if raw.len() != STATE_BYTES || &raw[..3] != OBJECT_STATE_VERSION {
        return Err(CatalogError::Corrupt("正文布局对象状态格式"));
    }
    let generation = u64::from_be_bytes(raw[4..12].try_into().expect("checked object state"));
    if generation == 0 || LayoutManifest::decode(&raw[12..])? != manifest {
        return Err(CatalogError::Corrupt("正文布局对象代次或清单不同"));
    }
    let state = match raw[3] {
        0 => ObjectState::Active,
        1 => ObjectState::Deleting,
        2 => ObjectState::Tombstone,
        _ => return Err(CatalogError::Corrupt("正文布局对象状态值")),
    };
    Ok((
        ObjectGeneration {
            manifest,
            generation,
        },
        state,
    ))
}

fn encode_reference(object: ObjectGeneration, owner: [u8; 32]) -> [u8; REFERENCE_BYTES] {
    let mut bytes = [0; REFERENCE_BYTES];
    bytes[..3].copy_from_slice(OBJECT_REFERENCE_VERSION);
    bytes[3..11].copy_from_slice(&object.generation.to_be_bytes());
    bytes[11..].copy_from_slice(&owner);
    bytes
}

/// Called only by an actual-source producer while its original-state gate is
/// held in this transaction. A preexisting state is never reset or resurrected.
pub(super) async fn prepare_at(
    tr: &Transaction,
    namespace: Uuid,
    manifest: LayoutManifest,
) -> Result<ObjectGeneration> {
    manifest.validate()?;
    let key = object_state_key(namespace, manifest);
    if let Some(raw) = tr.get(&key, false).await? {
        let (object, state) = decode_state(&raw, manifest)?;
        if state != ObjectState::Active {
            return Err(CatalogError::Incomplete);
        }
        return Ok(object);
    }
    let object = ObjectGeneration {
        manifest,
        generation: 1,
    };
    tr.set(&key, &encode_state(object, ObjectState::Active));
    Ok(object)
}

pub(super) async fn require_active_at(
    tr: &Transaction,
    namespace: Uuid,
    object: ObjectGeneration,
) -> Result<()> {
    let raw = tr
        .get(&object_state_key(namespace, object.manifest), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let (actual, state) = decode_state(&raw, object.manifest)?;
    if actual != object || state != ObjectState::Active {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

/// The non-snapshot state read conflicts with any future collector state CAS.
/// The exact reference key is independent of all other references and cannot
/// overwrite a different operation's evidence.
pub(super) async fn attach_at(
    tr: &Transaction,
    namespace: Uuid,
    object: ObjectGeneration,
    reference: ObjectReference,
    owner: [u8; 32],
) -> Result<()> {
    require_active_at(tr, namespace, object).await?;
    let key = reference_key(namespace, object, reference);
    let expected = encode_reference(object, owner);
    if let Some(raw) = tr.get(&key, false).await? {
        if raw.as_ref() != expected {
            return Err(CatalogError::Corrupt("正文布局对象引用与原操作不同"));
        }
    } else {
        tr.set(&key, &expected);
    }
    Ok(())
}

pub(super) async fn require_reference_at(
    tr: &Transaction,
    namespace: Uuid,
    object: ObjectGeneration,
    reference: ObjectReference,
    owner: [u8; 32],
) -> Result<()> {
    require_active_at(tr, namespace, object).await?;
    if tr
        .get(&reference_key(namespace, object, reference), false)
        .await?
        .as_deref()
        != Some(encode_reference(object, owner).as_slice())
    {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_state_is_fixed_bounded_and_does_not_resurrect() {
        let layout = BodyLayout::new(Uuid::new_v4(), 2, 0, 524_288, ['a', 'b'], vec![]).unwrap();
        let manifest = layout.manifest().unwrap();
        let object = ObjectGeneration {
            manifest,
            generation: 7,
        };
        for state in [
            ObjectState::Active,
            ObjectState::Deleting,
            ObjectState::Tombstone,
        ] {
            let bytes = encode_state(object, state);
            assert_eq!(bytes.len(), 59);
            assert_eq!(decode_state(&bytes, manifest).unwrap(), (object, state));
            for short in 0..bytes.len() {
                assert!(decode_state(&bytes[..short], manifest).is_err());
            }
        }
        let mut invalid = encode_state(object, ObjectState::Active);
        invalid[4..12].fill(0);
        assert!(decode_state(&invalid, manifest).is_err());
        invalid = encode_state(object, ObjectState::Active);
        invalid[3] = 3;
        assert!(decode_state(&invalid, manifest).is_err());
    }

    #[test]
    fn references_separate_original_stage_and_final_copy() {
        let namespace = Uuid::new_v4();
        let manifest = BodyLayout::new(Uuid::new_v4(), 2, 0, 524_288, ['a', 'b'], vec![])
            .unwrap()
            .manifest()
            .unwrap();
        let object = ObjectGeneration {
            manifest,
            generation: 1,
        };
        let original = [8; 32];
        let first = reference_key(
            namespace,
            object,
            ObjectReference::Import {
                original,
                ordinal: 0,
                source: 0,
            },
        );
        let second = reference_key(
            namespace,
            object,
            ObjectReference::Import {
                original,
                ordinal: 0,
                source: 1,
            },
        );
        let final_copy = reference_key(
            namespace,
            object,
            ObjectReference::Global {
                file: Uuid::new_v4(),
                copy: Uuid::new_v4(),
            },
        );
        assert_ne!(first, second);
        assert_ne!(first, final_copy);
        assert_ne!(first, object_state_key(namespace, manifest));
        assert_eq!(encode_reference(object, [9; 32]).len(), 43);
    }
}
