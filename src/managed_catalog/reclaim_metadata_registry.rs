//! One bounded parent checkpoint. Missing indexes never mean "idle" while an
//! original attempt is retained. Historical terminal reads remain independent.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u8,
    uid: u64,
    attempt: String,
    task_sha256: [u8; 32],
}

pub(super) fn key(reservation: &ReclaimReservation) -> Vec<u8> {
    ledger::own_key(reservation, b'Z')
}
fn checksum(parent: &str, raw: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"tape-rs/reclaim-metadata-registry/1");
    digest.update(parent.as_bytes());
    digest.update(raw);
    digest.finalize().into()
}
fn task_digest(task: &ReclaimMetadataWrite) -> Result<[u8; 32]> {
    let mut hash = Sha256::new();
    hash.update(b"tape-rs/reclaim-metadata-task/4");
    hash.update(task.encode()?);
    Ok(hash.finalize().into())
}
pub(super) fn encode(
    reservation: &ReclaimReservation,
    task: &ReclaimMetadataWrite,
) -> Result<Vec<u8>> {
    encode_header(
        &reservation.fingerprint(),
        &Header {
            version: 1,
            uid: task.0.plan.source.uid,
            attempt: task.id().to_string(),
            task_sha256: task_digest(task)?,
        },
    )
}
fn encode_header(parent: &str, value: &Header) -> Result<Vec<u8>> {
    let mut raw = serde_json::to_vec(value).map_err(cache)?;
    if raw.len() > 480 {
        return Err(CatalogError::Invalid("元数据活跃检查点超出预算"));
    }
    raw.extend_from_slice(&checksum(parent, &raw));
    Ok(raw)
}
fn decode(parent: &str, raw: &[u8]) -> Result<Header> {
    if !(32..=512).contains(&raw.len()) {
        return Err(CatalogError::Corrupt("元数据活跃检查点长度无效"));
    }
    let (body, sha) = raw.split_at(raw.len() - 32);
    if checksum(parent, body) != sha {
        return Err(CatalogError::Corrupt("元数据活跃检查点完整性校验失败"));
    }
    let value: Header = serde_json::from_slice(body)
        .map_err(|_| CatalogError::Corrupt("元数据活跃检查点载荷损坏"))?;
    node_mutation::id(&value.attempt)
        .map_err(|_| CatalogError::Corrupt("元数据活跃检查点 UUID 无效"))?;
    if value.version != 1 || value.uid == 0 || serde_json::to_vec(&value).map_err(cache)? != body {
        return Err(CatalogError::Corrupt("元数据活跃检查点编码或身份无效"));
    }
    Ok(value)
}

pub(super) fn edge_key(reservation: &ReclaimReservation, uid: u64, previous: Uuid) -> Vec<u8> {
    let mut key = ledger::own_key(reservation, b'U');
    key.extend_from_slice(&uid.to_be_bytes());
    key.extend_from_slice(previous.as_bytes());
    key
}
pub(super) async fn require_previous_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    task: &ReclaimMetadataWrite,
) -> Result<()> {
    let Some(previous) = task.previous_attempt() else {
        return Ok(());
    };
    let uid = task.0.plan.source.uid;
    let old = read_body_at(tr, reservation, uid, previous)
        .await?
        .ok_or(CatalogError::Corrupt("新元数据尝试缺少原已关闭前身"))?;
    if !old.closed()
        || old.first_attempt() != task.first_attempt()
        || old.plan() != task.plan()
        || old.0.scan != task.0.scan
        || tr
            .get(&edge_key(reservation, uid, previous), false)
            .await?
            .as_deref()
            != Some(task.id().as_bytes())
    {
        return Err(CatalogError::Corrupt("元数据后继与原关闭尝试或幂等边不同"));
    }
    if previous != task.first_attempt() {
        let first = read_body_at(tr, reservation, uid, task.first_attempt())
            .await?
            .ok_or(CatalogError::Corrupt("元数据后继缺少原首次尝试"))?;
        if !first.closed()
            || first.first_attempt() != first.id()
            || first.plan() != task.plan()
            || first.0.scan != task.0.scan
        {
            return Err(CatalogError::Corrupt("元数据首次尝试与原后继根身份不同"));
        }
    }
    Ok(())
}

/// A constant number of point reads plus three bounded first-row probes when
/// absent. It never walks the inventory or the whole attempt history.
pub(super) fn read_at<'a>(
    tr: &'a Transaction,
    reservation: &'a ReclaimReservation,
) -> std::pin::Pin<Box<impl std::future::Future<Output = Result<Option<ReclaimMetadataWrite>>> + 'a>>
{
    Box::pin(async move {
        let Some(raw) = tr.get(&key(reservation), false).await? else {
            if requests::prefix_has_rows(tr, &ledger::own_key(reservation, b'W')).await?
                || requests::prefix_has_rows(tr, &ledger::own_key(reservation, b'T')).await?
                || requests::prefix_has_rows(tr, &ledger::own_key(reservation, b'U')).await?
                || tr
                    .get(&ledger::own_key(reservation, b'A'), false)
                    .await?
                    .is_some()
            {
                return Err(CatalogError::Corrupt(
                    "原元数据行或占用存在但活跃检查点丢失",
                ));
            }
            return Ok(None);
        };
        let header = decode(&reservation.fingerprint(), &raw)?;
        let id = node_mutation::id(&header.attempt)?;
        let task = raw_at(tr, reservation, header.uid, id)
            .await?
            .ok_or(CatalogError::Corrupt("元数据活跃检查点缺少原最后尝试"))?;
        if task_digest(&task)? != header.task_sha256
            || tr
                .get(&latest_key(reservation, header.uid), false)
                .await?
                .as_deref()
                != Some(id.as_bytes())
            || (!task.holds_claim()
                && tr
                    .get(&ledger::own_key(reservation, b'A'), false)
                    .await?
                    .is_some())
        {
            return Err(CatalogError::Corrupt(
                "元数据活跃检查点与阶段、最新索引或占用不同",
            ));
        }
        require_claims_at(tr, reservation, &task).await?;
        if task.completed() {
            Box::pin(publication::require_task_at(tr, reservation, &task)).await?;
        }
        Ok(Some(task))
    })
}
pub(super) async fn require_current_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    task: &ReclaimMetadataWrite,
) -> Result<()> {
    if read_at(tr, reservation).await?.as_ref() != Some(task) {
        return Err(CatalogError::Corrupt("元数据执行尝试不是原活跃检查点"));
    }
    Ok(())
}

/// Owner changes may touch a historical terminal that is no longer last.
/// Such a claim must not replace the current attempt's checkpoint.
pub(super) async fn encode_if_current_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    task: &ReclaimMetadataWrite,
) -> Result<Option<Vec<u8>>> {
    let mut current = read_at(tr, reservation)
        .await?
        .ok_or(CatalogError::Corrupt("元数据领取缺少原活跃检查点"))?;
    if current.0.plan.source.uid == task.0.plan.source.uid && current.id() == task.id() {
        current.0.owner = task.owner();
        if current != *task {
            return Err(CatalogError::Corrupt("元数据领取改变 owner 以外的原状态"));
        }
        return encode(reservation, task).map(Some);
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_registry_codec_binds_parent_and_every_payload_byte() {
        let parent = "a".repeat(64);
        let value = Header {
            version: 1,
            uid: 7,
            attempt: Uuid::new_v4().to_string(),
            task_sha256: [9; 32],
        };
        let raw = encode_header(&parent, &value).unwrap();
        assert_eq!(decode(&parent, &raw).unwrap(), value);
        assert!(decode(&"b".repeat(64), &raw).is_err());
        for end in 0..raw.len() {
            assert!(decode(&parent, &raw[..end]).is_err());
        }
        for i in 0..raw.len() {
            let mut corrupt = raw.clone();
            corrupt[i] ^= 1;
            assert!(decode(&parent, &corrupt).is_err(), "未检出损坏字节 {i}");
        }
        assert!(decode(&parent, &[0; 513]).is_err());
    }

    #[test]
    fn metadata_registry_codec_rejects_valid_checksum_with_invalid_or_noncanonical_state() {
        let parent = "a".repeat(64);
        let value = Header {
            version: 1,
            uid: 7,
            attempt: Uuid::new_v4().to_string(),
            task_sha256: [9; 32],
        };
        for invalid in [
            Header {
                version: 2,
                ..value.clone()
            },
            Header {
                uid: 0,
                ..value.clone()
            },
            Header {
                attempt: Uuid::nil().to_string(),
                ..value.clone()
            },
        ] {
            assert!(decode(&parent, &encode_header(&parent, &invalid).unwrap()).is_err());
        }
        let mut extra = serde_json::to_vec(&value).unwrap();
        assert_eq!(extra.pop(), Some(b'}'));
        extra.extend_from_slice(b",\"extra\":true}");
        extra.extend_from_slice(&checksum(&parent, &extra));
        assert!(decode(&parent, &extra).is_err());
        let mut noncanonical = serde_json::to_vec(&value).unwrap();
        noncanonical.push(b' ');
        noncanonical.extend_from_slice(&checksum(&parent, &noncanonical));
        assert!(decode(&parent, &noncanonical).is_err());
    }
}
