//! Small, shared placement metadata. The production control plane has not switched to this store.

use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolRecord {
    pub id: Uuid,
    /// Immutable physical library failure domain, identified by changer serial.
    pub library_serial: String,
    pub name: String,
    pub file_limit: u64,
    pub policy: PoolPolicy,
    /// Monotonic CAS token; starts at one and advances on every update.
    pub revision: u64,
}

/// Pool placement and warning policy shared by every owner of a logical library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolPolicy {
    pub media_restriction: String,
    pub mount_limit: u16,
    pub node_group: String,
    pub low_space_warning_enabled: bool,
    pub low_space_warning_threshold: u16,
    pub no_space_warning_enabled: bool,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            media_restriction: "none".into(),
            mount_limit: 0,
            node_group: "G0".into(),
            low_space_warning_enabled: true,
            low_space_warning_threshold: 0,
            no_space_warning_enabled: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlOutcome {
    Created,
    Updated,
    Unassigned,
    AlreadyExists,
    AlreadyAbsent,
    StaleVersion,
    StaleOwner,
    NameConflict,
    IdConflict,
    VolumeConflict,
    BarcodeConflict,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumePlacement {
    pub volume_id: Uuid,
    pub namespace: Uuid,
    pub pool_id: Uuid,
    pub library_serial: String,
    pub barcode: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TapeAssignment {
    pub namespace: Uuid,
    pub pool_id: Uuid,
    pub library_serial: String,
    pub barcode: String,
    /// None until the owner has read and verified the LTFS volume identity.
    pub volume_id: Option<Uuid>,
}

pub(super) fn managed_key(namespace: Uuid) -> Vec<u8> {
    let mut k = key(b'a');
    k.extend_from_slice(namespace.as_bytes());
    k
}

fn placement_key(volume: Uuid) -> Vec<u8> {
    let mut k = key(b'b');
    k.extend_from_slice(volume.as_bytes());
    k
}

fn barcode_key(serial: &str, barcode: &str) -> Vec<u8> {
    let mut k = key(b'c');
    append_component(&mut k, serial.as_bytes());
    append_component(&mut k, barcode.as_bytes());
    k
}

fn assignment_key(serial: &str, barcode: &str) -> Vec<u8> {
    let mut k = key(b'd');
    append_component(&mut k, serial.as_bytes());
    append_component(&mut k, barcode.as_bytes());
    k
}

pub(super) fn namespace_assignment_key(namespace: Uuid, serial: &str, barcode: &str) -> Vec<u8> {
    let mut k = key(b'e');
    k.extend_from_slice(namespace.as_bytes());
    append_component(&mut k, serial.as_bytes());
    append_component(&mut k, barcode.as_bytes());
    k
}

fn encode_namespace_assignment(serial: &str, barcode: &str) -> Vec<u8> {
    let mut value = Vec::with_capacity(2 + serial.len() + barcode.len());
    value.push(serial.len() as u8);
    value.extend_from_slice(serial.as_bytes());
    value.push(barcode.len() as u8);
    value.extend_from_slice(barcode.as_bytes());
    value
}

pub(super) fn decode_namespace_assignment(
    namespace: Uuid,
    key: &[u8],
    bytes: &[u8],
) -> Result<(String, String)> {
    let serial_len = *bytes
        .first()
        .ok_or(CatalogError::Corrupt("磁带归属索引长度"))? as usize;
    let barcode_len_at = 1 + serial_len;
    let barcode_len = *bytes
        .get(barcode_len_at)
        .ok_or(CatalogError::Corrupt("磁带归属索引长度"))? as usize;
    if bytes.len() != barcode_len_at + 1 + barcode_len {
        return Err(CatalogError::Corrupt("磁带归属索引长度"));
    }
    let serial = std::str::from_utf8(&bytes[1..barcode_len_at])
        .map_err(|_| CatalogError::Corrupt("磁带归属索引序列号"))?;
    let barcode = std::str::from_utf8(&bytes[barcode_len_at + 1..])
        .map_err(|_| CatalogError::Corrupt("磁带归属索引条码"))?;
    validate_physical_serial(serial).map_err(|_| CatalogError::Corrupt("磁带归属索引序列号"))?;
    validate_barcode(barcode).map_err(|_| CatalogError::Corrupt("磁带归属索引条码"))?;
    if namespace_assignment_key(namespace, serial, barcode) != key {
        return Err(CatalogError::Corrupt("磁带归属索引键值不一致"));
    }
    Ok((serial.to_owned(), barcode.to_owned()))
}

fn encode_assignment(namespace: Uuid, pool: Uuid, volume: Option<Uuid>) -> Vec<u8> {
    let mut value = vec![0; if volume.is_some() { 49 } else { 33 }];
    value[0] = if volume.is_some() { 2 } else { 1 };
    value[1..17].copy_from_slice(namespace.as_bytes());
    value[17..33].copy_from_slice(pool.as_bytes());
    if let Some(volume) = volume {
        value[33..49].copy_from_slice(volume.as_bytes());
    }
    value
}

fn decode_assignment(serial: &str, barcode: &str, bytes: &[u8]) -> Result<TapeAssignment> {
    if !matches!(
        (bytes.first().copied(), bytes.len()),
        (Some(1), 33) | (Some(2), 49)
    ) {
        return Err(CatalogError::Corrupt("磁带分配记录版本或长度"));
    }
    let assignment = TapeAssignment {
        namespace: uuid_from(&bytes[1..17])?,
        pool_id: uuid_from(&bytes[17..33])?,
        library_serial: serial.to_owned(),
        barcode: barcode.to_owned(),
        volume_id: (bytes.len() == 49)
            .then(|| uuid_from(&bytes[33..49]))
            .transpose()?,
    };
    if assignment.namespace.is_nil()
        || assignment.pool_id.is_nil()
        || assignment.volume_id.is_some_and(|id| id.is_nil())
    {
        return Err(CatalogError::Corrupt("磁带分配记录标识"));
    }
    Ok(assignment)
}

async fn require_assignment_index(tr: &Transaction, assignment: &TapeAssignment) -> Result<()> {
    let key = namespace_assignment_key(
        assignment.namespace,
        &assignment.library_serial,
        &assignment.barcode,
    );
    let expected = encode_namespace_assignment(&assignment.library_serial, &assignment.barcode);
    match tr.get(&key, false).await? {
        Some(value) if value.as_ref() == expected.as_slice() => Ok(()),
        _ => Err(CatalogError::Corrupt("磁带归属命名空间索引缺失或损坏")),
    }
}

fn validate_barcode(barcode: &str) -> Result<()> {
    if barcode.is_empty()
        || barcode.len() > 64
        || !barcode
            .bytes()
            .all(|b| (b'!'..=b'~').contains(&b) && b != b'/')
    {
        return Err(CatalogError::Invalid("磁带条码为空、过长或包含非法字符"));
    }
    Ok(())
}

pub(super) fn validate_physical_serial(serial: &str) -> Result<()> {
    validate_serial(serial)?;
    if serial.len() > 64
        || !serial
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(CatalogError::Invalid("带库序列号不符合集群配置格式"));
    }
    Ok(())
}

fn validate_placement(p: &VolumePlacement) -> Result<()> {
    if p.volume_id.is_nil() || p.namespace.is_nil() || p.pool_id.is_nil() {
        return Err(CatalogError::Invalid("卷、命名空间和 pool ID 不能为 nil"));
    }
    validate_physical_serial(&p.library_serial)?;
    validate_barcode(&p.barcode)
}

fn encode_placement(p: &VolumePlacement) -> Vec<u8> {
    let mut v = Vec::with_capacity(35 + p.library_serial.len() + p.barcode.len());
    v.push(1);
    v.extend_from_slice(p.namespace.as_bytes());
    v.extend_from_slice(p.pool_id.as_bytes());
    v.push(p.library_serial.len() as u8);
    v.extend_from_slice(p.library_serial.as_bytes());
    v.push(p.barcode.len() as u8);
    v.extend_from_slice(p.barcode.as_bytes());
    v
}

fn decode_placement(volume_id: Uuid, bytes: &[u8]) -> Result<VolumePlacement> {
    if bytes.len() < 36 || bytes[0] != 1 {
        return Err(CatalogError::Corrupt("卷放置记录版本或长度"));
    }
    let serial_end = 34 + bytes[33] as usize;
    if bytes.len() <= serial_end || bytes.len() != serial_end + 1 + bytes[serial_end] as usize {
        return Err(CatalogError::Corrupt("卷放置记录字段长度"));
    }
    let p = VolumePlacement {
        volume_id,
        namespace: uuid_from(&bytes[1..17])?,
        pool_id: uuid_from(&bytes[17..33])?,
        library_serial: std::str::from_utf8(&bytes[34..serial_end])
            .map_err(|_| CatalogError::Corrupt("带库序列号编码"))?
            .to_owned(),
        barcode: std::str::from_utf8(&bytes[serial_end + 1..])
            .map_err(|_| CatalogError::Corrupt("磁带条码编码"))?
            .to_owned(),
    };
    validate_placement(&p).map_err(|_| CatalogError::Corrupt("卷放置记录字段"))?;
    Ok(p)
}

/// True for legacy namespaces; managed namespaces require a matching immutable placement.
pub(super) async fn receipt_placement_matches(tr: &Transaction, r: &CommitReceipt) -> Result<bool> {
    match tr.get(&managed_key(r.namespace), false).await? {
        None => {
            if r.pool_id.is_some() {
                return Ok(false);
            }
            return Ok(tr.get(&placement_key(r.volume_id), false).await?.is_none());
        }
        Some(value) if value.is_empty() => {}
        Some(_) => return Err(CatalogError::Corrupt("受管命名空间标记")),
    }
    let Some(bytes) = tr.get(&placement_key(r.volume_id), false).await? else {
        return Ok(false);
    };
    let p = decode_placement(r.volume_id, &bytes)?;
    if p.namespace != r.namespace
        || p.library_serial != r.library_serial
        || Some(p.pool_id) != r.pool_id
    {
        return Ok(false);
    }
    let reverse = tr
        .get(&barcode_key(&p.library_serial, &p.barcode), false)
        .await?;
    if reverse.as_deref() != Some(r.volume_id.as_bytes()) {
        return Err(CatalogError::Corrupt("磁带条码反向索引"));
    }
    let assignment = tr
        .get(&assignment_key(&p.library_serial, &p.barcode), false)
        .await?
        .ok_or(CatalogError::Corrupt("卷放置缺少磁带分配"))?;
    let assignment = decode_assignment(&p.library_serial, &p.barcode, &assignment)?;
    if assignment.namespace != r.namespace
        || assignment.pool_id != p.pool_id
        || assignment.volume_id != Some(r.volume_id)
    {
        return Err(CatalogError::Corrupt("磁带分配与卷放置不一致"));
    }
    require_assignment_index(tr, &assignment).await?;
    require_pool_library(tr, p.namespace, p.pool_id, &p.library_serial).await?;
    Ok(true)
}

pub(super) async fn volume_serial_matches(
    tr: &Transaction,
    volume: Uuid,
    serial: &str,
) -> Result<bool> {
    let Some(bytes) = tr.get(&placement_key(volume), false).await? else {
        return Ok(true);
    };
    Ok(decode_placement(volume, &bytes)?.library_serial == serial)
}

pub(super) async fn volume_is_managed(tr: &Transaction, volume: Uuid) -> Result<bool> {
    Ok(managed_volume_namespace(tr, volume).await?.is_some())
}

pub(super) async fn managed_volume_namespace(
    tr: &Transaction,
    volume: Uuid,
) -> Result<Option<Uuid>> {
    let Some(bytes) = tr.get(&placement_key(volume), false).await? else {
        return Ok(None);
    };
    let placement = decode_placement(volume, &bytes)?;
    if tr
        .get(&managed_key(placement.namespace), false)
        .await?
        .as_deref()
        != Some(&[][..])
    {
        return Err(CatalogError::Corrupt("受管卷缺少命名空间标记"));
    }
    Ok(Some(placement.namespace))
}

/// Resolve and validate a volume's physical placement in the caller's FDB
/// read transaction, so path, copy, readiness and barcode cannot be combined
/// from different database versions.
pub(super) async fn verified_placement(
    tr: &Transaction,
    volume_id: Uuid,
) -> Result<Option<VolumePlacement>> {
    verified_placement_in_state(tr, volume_id, NamespaceState::Ready).await
}

pub(super) async fn verified_import_placement(
    tr: &Transaction,
    volume_id: Uuid,
) -> Result<Option<VolumePlacement>> {
    let Some(bytes) = tr.get(&placement_key(volume_id), false).await? else {
        return Ok(None);
    };
    let placement = decode_placement(volume_id, &bytes)?;
    let state = tr.get(&namespace_key(placement.namespace), false).await?;
    let required = match state.as_deref() {
        Some([value]) if *value == NamespaceState::Building as u8 => NamespaceState::Building,
        Some([value]) if *value == NamespaceState::Verified as u8 => NamespaceState::Verified,
        _ => return Err(CatalogError::Incomplete),
    };
    verified_placement_in_state(tr, volume_id, required).await
}

async fn verified_placement_in_state(
    tr: &Transaction,
    volume_id: Uuid,
    required: NamespaceState,
) -> Result<Option<VolumePlacement>> {
    let Some(bytes) = tr.get(&placement_key(volume_id), false).await? else {
        return Ok(None);
    };
    let p = decode_placement(volume_id, &bytes)?;
    match required {
        // This helper is read-only. Sealed and Active imports remain readable,
        // while all mutation entry points still require exactly Ready.
        NamespaceState::Ready => require_namespace_readable(tr, p.namespace).await?,
        NamespaceState::Building | NamespaceState::Verified => {
            if tr.get(&namespace_key(p.namespace), false).await?.as_deref()
                != Some(&[required as u8][..])
            {
                return Err(CatalogError::Incomplete);
            }
        }
        NamespaceState::Discarded
        | NamespaceState::Rejected
        | NamespaceState::Verifying
        | NamespaceState::Sealed
        | NamespaceState::Active => {
            return Err(CatalogError::Invalid("卷放置校验状态无效"));
        }
    }
    if tr
        .get(&barcode_key(&p.library_serial, &p.barcode), false)
        .await?
        .as_deref()
        != Some(volume_id.as_bytes())
    {
        return Err(CatalogError::Corrupt("磁带条码反向索引"));
    }
    let assigned = tr
        .get(&assignment_key(&p.library_serial, &p.barcode), false)
        .await?
        .ok_or(CatalogError::Corrupt("卷放置缺少磁带分配"))?;
    let assigned = decode_assignment(&p.library_serial, &p.barcode, &assigned)?;
    if assigned.namespace != p.namespace
        || assigned.pool_id != p.pool_id
        || assigned.volume_id != Some(volume_id)
    {
        return Err(CatalogError::Corrupt("磁带分配与卷放置不一致"));
    }
    require_assignment_index(tr, &assigned).await?;
    require_pool_library(tr, p.namespace, p.pool_id, &p.library_serial).await?;
    Ok(Some(p))
}

fn library_prefix(namespace: Uuid) -> Vec<u8> {
    let mut k = key(b'U');
    k.extend_from_slice(namespace.as_bytes());
    k
}

pub(super) fn library_key(namespace: Uuid, serial: &str) -> Vec<u8> {
    let mut k = library_prefix(namespace);
    append_component(&mut k, serial.as_bytes());
    k
}

fn pool_prefix(namespace: Uuid) -> Vec<u8> {
    let mut k = key(b'T');
    k.extend_from_slice(namespace.as_bytes());
    k
}

fn pool_key(namespace: Uuid, id: Uuid) -> Vec<u8> {
    let mut k = pool_prefix(namespace);
    k.extend_from_slice(id.as_bytes());
    k
}

fn pool_name_key(namespace: Uuid, name: &str) -> Vec<u8> {
    let mut k = key(b'X');
    k.extend_from_slice(namespace.as_bytes());
    append_component(&mut k, name.as_bytes());
    k
}

fn node_group_prefix(namespace: Uuid) -> Vec<u8> {
    let mut k = key(b'Y');
    k.extend_from_slice(namespace.as_bytes());
    k
}

pub(super) fn node_group_key(namespace: Uuid, name: &str) -> Vec<u8> {
    let mut k = node_group_prefix(namespace);
    append_component(&mut k, name.as_bytes());
    k
}

fn group_pool_prefix(namespace: Uuid, name: &str) -> Vec<u8> {
    let mut k = key(b'y');
    k.extend_from_slice(namespace.as_bytes());
    append_component(&mut k, name.as_bytes());
    k
}

fn group_pool_key(namespace: Uuid, name: &str, pool: Uuid) -> Vec<u8> {
    let mut k = group_pool_prefix(namespace, name);
    k.extend_from_slice(pool.as_bytes());
    k
}

fn validate_node_group(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 16
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(CatalogError::Invalid(
            "节点组名称须为 1–16 个 ASCII 字母、数字或下划线",
        ));
    }
    Ok(())
}

async fn require_node_group(tr: &Transaction, namespace: Uuid, name: &str) -> Result<()> {
    match tr.get(&node_group_key(namespace, name), false).await? {
        Some(value) if value.is_empty() => Ok(()),
        Some(_) => Err(CatalogError::Corrupt("节点组记录损坏")),
        None => Err(CatalogError::Invalid("pool 节点组不存在")),
    }
}

async fn require_group_pool_index(
    tr: &Transaction,
    namespace: Uuid,
    pool: &PoolRecord,
) -> Result<()> {
    match tr
        .get(
            &group_pool_key(namespace, &pool.policy.node_group, pool.id),
            false,
        )
        .await?
    {
        Some(value) if value.is_empty() => Ok(()),
        _ => Err(CatalogError::Corrupt("pool 节点组引用索引缺失或损坏")),
    }
}

fn validate_pool(pool: &PoolRecord) -> Result<()> {
    validate_physical_serial(&pool.library_serial)?;
    if pool.id.is_nil() || pool.file_limit == 0 || pool.revision == 0 {
        return Err(CatalogError::Invalid(
            "pool ID、revision 和文件数上限必须有效",
        ));
    }
    if pool.name.is_empty()
        || pool.name.len() > MAX_NAME
        || pool.name.as_bytes().contains(&0)
        || pool.name.as_bytes().contains(&b'/')
    {
        return Err(CatalogError::Invalid("pool 名称为空、过长或含非法字符"));
    }
    let policy = &pool.policy;
    if ![
        "none", "L5", "L6", "L7", "L8", "L9", "M8", "JB", "JC", "JD", "JE", "JY", "JZ", "JV", "JK",
        "JL", "JM", "JF",
    ]
    .contains(&policy.media_restriction.as_str())
    {
        return Err(CatalogError::Invalid("pool 介质限制非法"));
    }
    if policy.mount_limit > 32767 || policy.low_space_warning_threshold > 32767 {
        return Err(CatalogError::Invalid("pool 挂载或低空间阈值超出范围"));
    }
    validate_node_group(&policy.node_group)?;
    Ok(())
}

fn encode_pool(pool: &PoolRecord) -> Vec<u8> {
    let policy = &pool.policy;
    let mut value = Vec::with_capacity(
        27 + pool.name.len()
            + policy.media_restriction.len()
            + policy.node_group.len()
            + pool.library_serial.len(),
    );
    value.push(2);
    value.extend_from_slice(&pool.revision.to_be_bytes());
    value.extend_from_slice(&pool.file_limit.to_be_bytes());
    value.extend_from_slice(&policy.mount_limit.to_be_bytes());
    value.extend_from_slice(&policy.low_space_warning_threshold.to_be_bytes());
    value.push(
        u8::from(policy.low_space_warning_enabled)
            | (u8::from(policy.no_space_warning_enabled) << 1),
    );
    value.extend_from_slice(&(pool.name.len() as u16).to_be_bytes());
    value.push(policy.media_restriction.len() as u8);
    value.push(policy.node_group.len() as u8);
    value.push(pool.library_serial.len() as u8);
    value.extend_from_slice(pool.name.as_bytes());
    value.extend_from_slice(policy.media_restriction.as_bytes());
    value.extend_from_slice(policy.node_group.as_bytes());
    value.extend_from_slice(pool.library_serial.as_bytes());
    value
}

fn decode_pool(id: Uuid, value: &[u8]) -> Result<PoolRecord> {
    if value.first() != Some(&2) {
        return Err(CatalogError::Corrupt(
            "pool 记录版本不支持；旧候选需重新导入明确的带库归属",
        ));
    }
    if value.len() < 27 || value[21] & !0b11 != 0 {
        return Err(CatalogError::Corrupt("pool 记录长度"));
    }
    let name_len = u16::from_be_bytes([value[22], value[23]]) as usize;
    let media_len = value[24] as usize;
    let group_len = value[25] as usize;
    let serial_len = value[26] as usize;
    if value.len() != 27 + name_len + media_len + group_len + serial_len {
        return Err(CatalogError::Corrupt("pool 可变字段长度"));
    }
    let name = std::str::from_utf8(&value[27..27 + name_len])
        .map_err(|_| CatalogError::Corrupt("pool 名称编码"))?
        .to_owned();
    let media = std::str::from_utf8(&value[27 + name_len..27 + name_len + media_len])
        .map_err(|_| CatalogError::Corrupt("pool 介质限制编码"))?
        .to_owned();
    let group = std::str::from_utf8(
        &value[27 + name_len + media_len..27 + name_len + media_len + group_len],
    )
    .map_err(|_| CatalogError::Corrupt("pool 节点组编码"))?
    .to_owned();
    let library_serial = std::str::from_utf8(&value[27 + name_len + media_len + group_len..])
        .map_err(|_| CatalogError::Corrupt("pool 带库序列号编码"))?
        .to_owned();
    let pool = PoolRecord {
        id,
        library_serial,
        name,
        file_limit: read_u64(&value[9..17]),
        policy: PoolPolicy {
            media_restriction: media,
            mount_limit: u16::from_be_bytes([value[17], value[18]]),
            node_group: group,
            low_space_warning_enabled: value[21] & 1 != 0,
            low_space_warning_threshold: u16::from_be_bytes([value[19], value[20]]),
            no_space_warning_enabled: value[21] & 2 != 0,
        },
        revision: read_u64(&value[1..9]),
    };
    validate_pool(&pool).map_err(|_| CatalogError::Corrupt("pool 字段"))?;
    Ok(pool)
}

async fn require_control_namespace(tr: &Transaction, namespace: Uuid) -> Result<()> {
    let state = tr.get(&namespace_key(namespace), false).await?;
    match state.as_deref().map(decode_namespace_state).transpose()? {
        Some(NamespaceState::Ready) => Ok(()),
        _ => Err(CatalogError::Incomplete),
    }
}

async fn require_control_building_or_ready(tr: &Transaction, namespace: Uuid) -> Result<()> {
    let state = tr.get(&namespace_key(namespace), false).await?;
    match state.as_deref().map(decode_namespace_state).transpose()? {
        Some(NamespaceState::Building | NamespaceState::Ready) => Ok(()),
        _ => Err(CatalogError::Incomplete),
    }
}

async fn require_managed_pool_library(
    tr: &Transaction,
    namespace: Uuid,
    pool_id: Uuid,
    serial: &str,
) -> Result<()> {
    require_control_building_or_ready(tr, namespace).await?;
    match tr.get(&managed_key(namespace), false).await?.as_deref() {
        Some([]) => {}
        Some(_) => return Err(CatalogError::Corrupt("受管命名空间标记")),
        None => return Err(CatalogError::Invalid("磁带只能分配到受管命名空间")),
    }
    require_pool_library(tr, namespace, pool_id, serial).await
}

async fn require_pool_library(
    tr: &Transaction,
    namespace: Uuid,
    pool_id: Uuid,
    serial: &str,
) -> Result<()> {
    let pool = tr
        .get(&pool_key(namespace, pool_id), false)
        .await?
        .ok_or(CatalogError::Invalid("pool 不存在"))?;
    let pool = decode_pool(pool_id, &pool)?;
    if pool.library_serial != serial {
        return Err(CatalogError::Invalid(
            "pool 不属于目标物理带库，跨库操作必须使用另一个池",
        ));
    }
    require_node_group(tr, namespace, &pool.policy.node_group).await?;
    require_group_pool_index(tr, namespace, &pool).await?;
    let library = tr
        .get(&library_key(namespace, serial), false)
        .await?
        .ok_or(CatalogError::Invalid("带库不属于逻辑命名空间"))?;
    if !library.is_empty() {
        return Err(CatalogError::Corrupt("物理带库归属记录"));
    }
    Ok(())
}

impl ManagedCatalog {
    pub(super) async fn import_pool_exists(&self, namespace: Uuid, id: Uuid) -> Result<bool> {
        if id.is_nil() {
            return Err(CatalogError::Invalid("pool ID 不能为 nil"));
        }
        let tr = self.db.create_trx()?;
        require_control_building_or_ready(&tr, namespace).await?;
        tr.get(&pool_key(namespace, id), false)
            .await?
            .map(|value| decode_pool(id, &value).map(|_| true))
            .transpose()
            .map(|value| value.unwrap_or(false))
    }

    /// Node groups are namespace-scoped. G0 is created with the namespace.
    pub async fn register_node_group(&self, namespace: Uuid, name: &str) -> Result<bool> {
        validate_node_group(name)?;
        let record = node_group_key(namespace, name);
        self.db
            .run(|tr, _| {
                let record = record.clone();
                async move {
                    requests::retry_result(
                        async {
                            require_control_building_or_ready(&tr, namespace).await?;
                            match tr.get(&record, false).await? {
                                Some(value) if value.is_empty() => Ok(false),
                                Some(_) => Err(CatalogError::Corrupt("节点组记录损坏")),
                                None => {
                                    tr.set(&record, b"");
                                    Ok(true)
                                }
                            }
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Reject deleting a group while any pool still references it.
    pub async fn remove_node_group(&self, namespace: Uuid, name: &str) -> Result<bool> {
        validate_node_group(name)?;
        if name == "G0" {
            return Err(CatalogError::Invalid("默认节点组 G0 不能删除"));
        }
        let record = node_group_key(namespace, name);
        let prefix = group_pool_prefix(namespace, name);
        self.db
            .run(|tr, _| {
                let record = record.clone();
                let prefix = prefix.clone();
                async move {
                    requests::retry_result(
                        async {
                            require_control_namespace(&tr, namespace).await?;
                            let Some(value) = tr.get(&record, false).await? else {
                                return Ok(false);
                            };
                            if !value.is_empty() {
                                return Err(CatalogError::Corrupt("节点组记录损坏"));
                            }
                            let range = RangeOption {
                                begin: KeySelector::first_greater_or_equal(prefix.clone()),
                                end: KeySelector::first_greater_or_equal(prefix_end(&prefix)),
                                limit: Some(1),
                                ..RangeOption::default()
                            };
                            if !tr.get_range(&range, 1, false).await?.is_empty() {
                                return Err(CatalogError::Invalid("节点组仍被 pool 引用"));
                            }
                            tr.clear(&record);
                            Ok(true)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    pub async fn list_node_groups(
        &self,
        namespace: Uuid,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<String>> {
        let prefix = node_group_prefix(namespace);
        if after.is_some_and(|cursor| {
            !cursor.starts_with(&prefix)
                || cursor.len() > prefix.len() + 18
                || decode_name(&cursor[prefix.len()..]).is_err()
        }) {
            return Err(CatalogError::Invalid("节点组分页游标无效"));
        }
        let rows = self
            .page_range(&prefix, after, limit, Some(namespace), None)
            .await?;
        let items = rows
            .items
            .into_iter()
            .map(|(k, v)| {
                if !v.is_empty() {
                    return Err(CatalogError::Corrupt("节点组记录损坏"));
                }
                let name = String::from_utf8(decode_name(&k[prefix.len()..])?)
                    .map_err(|_| CatalogError::Corrupt("节点组名称编码"))?;
                validate_node_group(&name).map_err(|_| CatalogError::Corrupt("节点组名称非法"))?;
                Ok(name)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Page {
            items,
            next: rows.next,
        })
    }

    /// Declare a physical changer by its real serial within one logical namespace.
    pub async fn register_physical_library(
        &self,
        namespace: Uuid,
        serial: &str,
    ) -> Result<ControlOutcome> {
        validate_physical_serial(serial)?;
        let record = library_key(namespace, serial);
        self.db
            .run(|tr, _| {
                let record = record.clone();
                async move {
                    if let Err(error) = require_control_building_or_ready(&tr, namespace).await {
                        return Ok(Err(error));
                    }
                    match tr.get(&record, false).await? {
                        Some(value) if value.is_empty() => Ok(Ok(ControlOutcome::AlreadyExists)),
                        Some(_) => Ok(Err(CatalogError::Corrupt("物理带库归属记录"))),
                        None => {
                            tr.set(&record, b"");
                            Ok(Ok(ControlOutcome::Created))
                        }
                    }
                }
            })
            .await?
    }

    /// Pool names are unique only within one logical namespace.
    pub async fn create_pool(&self, namespace: Uuid, pool: &PoolRecord) -> Result<ControlOutcome> {
        validate_pool(pool)?;
        if pool.revision != 1 {
            return Err(CatalogError::Invalid("新 pool 的 revision 必须为 1"));
        }
        let record = pool_key(namespace, pool.id);
        let name = pool_name_key(namespace, &pool.name);
        let group_index = group_pool_key(namespace, &pool.policy.node_group, pool.id);
        let value = encode_pool(pool);
        self.db
            .run(|tr, _| {
                let (record, name, group_index, value) = (
                    record.clone(),
                    name.clone(),
                    group_index.clone(),
                    value.clone(),
                );
                async move {
                    requests::retry_result(
                        async {
                            require_control_building_or_ready(&tr, namespace).await?;
                            match tr
                                .get(&library_key(namespace, &pool.library_serial), false)
                                .await?
                            {
                                Some(value) if value.is_empty() => {}
                                Some(_) => return Err(CatalogError::Corrupt("物理带库归属记录")),
                                None => {
                                    return Err(CatalogError::Invalid(
                                        "pool 创建前必须注册所属物理带库",
                                    ));
                                }
                            }
                            require_node_group(&tr, namespace, &pool.policy.node_group).await?;
                            let old = tr.get(&record, false).await?;
                            let named = tr.get(&name, false).await?;
                            if let Some(ref bytes) = old {
                                let saved = decode_pool(pool.id, bytes)?;
                                require_group_pool_index(&tr, namespace, &saved).await?;
                            }
                            if let Some(ref bytes) = named {
                                uuid_from(bytes)?;
                            }
                            match (old.as_deref(), named.as_deref()) {
                                (Some(previous), Some(id))
                                    if previous == value && id == pool.id.as_bytes() =>
                                {
                                    Ok(ControlOutcome::AlreadyExists)
                                }
                                (Some(previous), None) if previous == value => {
                                    Err(CatalogError::Corrupt("pool 名称索引缺失"))
                                }
                                (None, Some(id)) if id == pool.id.as_bytes() => {
                                    Err(CatalogError::Corrupt("pool 主记录缺失"))
                                }
                                (Some(_), _) => Ok(ControlOutcome::IdConflict),
                                (_, Some(_)) => Ok(ControlOutcome::NameConflict),
                                (None, None) => {
                                    if tr.get(&group_index, false).await?.is_some() {
                                        return Err(CatalogError::Corrupt("pool 节点组索引孤儿"));
                                    }
                                    tr.set(&record, &value);
                                    tr.set(&name, pool.id.as_bytes());
                                    tr.set(&group_index, b"");
                                    Ok(ControlOutcome::Created)
                                }
                            }
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Replace a pool's mutable attributes using the previously observed value.
    /// The name index changes in the same transaction as the primary record.
    pub async fn update_pool(
        &self,
        namespace: Uuid,
        expected: &PoolRecord,
        replacement: &PoolRecord,
    ) -> Result<ControlOutcome> {
        validate_pool(expected)?;
        validate_pool(replacement)?;
        if expected.library_serial != replacement.library_serial {
            return Err(CatalogError::Invalid("不能改变 pool 所属物理带库"));
        }
        if expected.id != replacement.id {
            return Err(CatalogError::Invalid("不能改变 pool ID"));
        }
        if expected.revision.checked_add(1) != Some(replacement.revision) {
            return Err(CatalogError::Invalid("pool revision 必须递增 1"));
        }
        let record = pool_key(namespace, expected.id);
        let old_name = pool_name_key(namespace, &expected.name);
        let new_name = pool_name_key(namespace, &replacement.name);
        let old_group = group_pool_key(namespace, &expected.policy.node_group, expected.id);
        let new_group = group_pool_key(namespace, &replacement.policy.node_group, replacement.id);
        let replacement_value = encode_pool(replacement);
        self.db
            .run(|tr, _| {
                let (record, old_name, new_name, old_group, new_group, replacement_value) = (
                    record.clone(),
                    old_name.clone(),
                    new_name.clone(),
                    old_group.clone(),
                    new_group.clone(),
                    replacement_value.clone(),
                );
                async move {
                    requests::retry_result(
                        async {
                            require_control_namespace(&tr, namespace).await?;
                            let Some(current) = tr.get(&record, false).await? else {
                                return Ok(ControlOutcome::IdConflict);
                            };
                            let current = decode_pool(expected.id, &current)?;
                            require_node_group(&tr, namespace, &current.policy.node_group).await?;
                            require_group_pool_index(&tr, namespace, &current).await?;
                            let actual_name = pool_name_key(namespace, &current.name);
                            match tr.get(&actual_name, false).await? {
                                Some(id) if id.as_ref() == current.id.as_bytes() => {}
                                _ => return Err(CatalogError::Corrupt("pool 名称索引缺失")),
                            }
                            if current == *replacement {
                                return Ok(ControlOutcome::AlreadyExists);
                            }
                            if current != *expected {
                                return Ok(ControlOutcome::StaleVersion);
                            }
                            require_node_group(&tr, namespace, &replacement.policy.node_group)
                                .await?;
                            if old_name != new_name {
                                if let Some(id) = tr.get(&new_name, false).await? {
                                    if uuid_from(&id)? == expected.id {
                                        return Err(CatalogError::Corrupt("pool 重复名称索引"));
                                    }
                                    return Ok(ControlOutcome::NameConflict);
                                }
                                tr.clear(&old_name);
                                tr.set(&new_name, expected.id.as_bytes());
                            }
                            if old_group != new_group {
                                if tr.get(&new_group, false).await?.is_some() {
                                    return Err(CatalogError::Corrupt("pool 目标节点组索引孤儿"));
                                }
                                tr.clear(&old_group);
                                tr.set(&new_group, b"");
                            }
                            tr.set(&record, &replacement_value);
                            Ok(ControlOutcome::Updated)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Reserve a barcode and pool before loading the tape and learning its LTFS UUID.
    pub async fn assign_tape(
        &self,
        namespace: Uuid,
        pool_id: Uuid,
        serial: &str,
        barcode: &str,
    ) -> Result<ControlOutcome> {
        if pool_id.is_nil() {
            return Err(CatalogError::Invalid("pool ID 不能为 nil"));
        }
        validate_physical_serial(serial)?;
        validate_barcode(barcode)?;
        let serial = serial.to_owned();
        let barcode = barcode.to_owned();
        let record = assignment_key(&serial, &barcode);
        let reverse = barcode_key(&serial, &barcode);
        let index = namespace_assignment_key(namespace, &serial, &barcode);
        let value = encode_assignment(namespace, pool_id, None);
        let index_value = encode_namespace_assignment(&serial, &barcode);
        self.db
            .run(|tr, _| {
                let (serial, barcode, record, reverse, index, value, index_value) = (
                    serial.clone(),
                    barcode.clone(),
                    record.clone(),
                    reverse.clone(),
                    index.clone(),
                    value.clone(),
                    index_value.clone(),
                );
                async move {
                    requests::retry_result(
                        async {
                            require_managed_pool_library(&tr, namespace, pool_id, &serial).await?;
                            let old = tr.get(&record, false).await?;
                            let bound = tr.get(&reverse, false).await?;
                            if let Some(old) = old {
                                let assignment = decode_assignment(&serial, &barcode, &old)?;
                                if assignment.namespace != namespace
                                    || assignment.pool_id != pool_id
                                {
                                    return Ok(ControlOutcome::BarcodeConflict);
                                }
                                require_assignment_index(&tr, &assignment).await?;
                                if let Some(bound) = bound {
                                    let volume_id = uuid_from(&bound)?;
                                    if assignment.volume_id != Some(volume_id) {
                                        return Err(CatalogError::Corrupt(
                                            "磁带分配与卷放置不一致",
                                        ));
                                    }
                                    let placement = tr
                                        .get(&placement_key(volume_id), false)
                                        .await?
                                        .ok_or(CatalogError::Corrupt("卷放置主记录缺失"))?;
                                    let placement = decode_placement(volume_id, &placement)?;
                                    if placement.namespace != namespace
                                        || placement.pool_id != pool_id
                                        || placement.library_serial != serial
                                        || placement.barcode != barcode
                                    {
                                        return Err(CatalogError::Corrupt(
                                            "磁带分配与卷放置不一致",
                                        ));
                                    }
                                } else if assignment.volume_id.is_some() {
                                    return Err(CatalogError::Corrupt(
                                        "已绑定磁带缺少条码反向索引",
                                    ));
                                }
                                return Ok(ControlOutcome::AlreadyExists);
                            }
                            if bound.is_some() {
                                return Err(CatalogError::Corrupt("卷放置缺少磁带分配"));
                            }
                            if tr.get(&index, false).await?.is_some() {
                                return Err(CatalogError::Corrupt("孤立磁带归属命名空间索引"));
                            }
                            tr.set(&record, &value);
                            tr.set(&index, &index_value);
                            Ok(ControlOutcome::Created)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Remove only an unbound reservation. A bound tape requires a separate
    /// media and live-copy reconciliation protocol before it can be released.
    pub async fn unassign_tape(
        &self,
        namespace: Uuid,
        pool_id: Uuid,
        serial: &str,
        barcode: &str,
    ) -> Result<ControlOutcome> {
        if pool_id.is_nil() {
            return Err(CatalogError::Invalid("pool ID 不能为 nil"));
        }
        validate_physical_serial(serial)?;
        validate_barcode(barcode)?;
        let serial = serial.to_owned();
        let barcode = barcode.to_owned();
        let record = assignment_key(&serial, &barcode);
        let reverse = barcode_key(&serial, &barcode);
        let index = namespace_assignment_key(namespace, &serial, &barcode);
        self.db
            .run(|tr, _| {
                let (serial, barcode, record, reverse, index) = (
                    serial.clone(),
                    barcode.clone(),
                    record.clone(),
                    reverse.clone(),
                    index.clone(),
                );
                async move {
                    requests::retry_result(
                        async {
                            require_control_namespace(&tr, namespace).await?;
                            let old = tr.get(&record, false).await?;
                            let bound = tr.get(&reverse, false).await?;
                            let Some(old) = old else {
                                return if bound.is_some() {
                                    Err(CatalogError::Corrupt("卷放置缺少磁带分配"))
                                } else if tr.get(&index, false).await?.is_some() {
                                    Err(CatalogError::Corrupt("孤立磁带归属命名空间索引"))
                                } else {
                                    Ok(ControlOutcome::AlreadyAbsent)
                                };
                            };
                            let assignment = decode_assignment(&serial, &barcode, &old)?;
                            if assignment.namespace != namespace || assignment.pool_id != pool_id {
                                return Ok(ControlOutcome::BarcodeConflict);
                            }
                            require_assignment_index(&tr, &assignment).await?;
                            if assignment.volume_id.is_some() || bound.is_some() {
                                return Ok(ControlOutcome::VolumeConflict);
                            }
                            tr.clear(&record);
                            tr.clear(&index);
                            Ok(ControlOutcome::Unassigned)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Retire the exact ready scan of an import that was never made visible.
    /// This only changes FDB metadata; the owner must already have fenced
    /// physical media operations before abandoning the candidate.
    pub async fn retire_discarded_volume_scan(
        &self,
        namespace: Uuid,
        serial: &str,
        epoch: u64,
        volume: Uuid,
        scan: VolumeScan,
    ) -> Result<VolumeOutcome> {
        validate_physical_serial(serial)?;
        validate_scan(volume, scan)?;
        if namespace.is_nil() || epoch == 0 {
            return Err(CatalogError::Invalid("废弃卷退役目标或 owner epoch 无效"));
        }
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            if tr.get(&namespace_key(namespace), false).await?.as_deref()
                                != Some(&[NamespaceState::Discarded as u8][..])
                            {
                                return Err(CatalogError::Invalid("只能退役已废弃候选的就绪扫描"));
                            }
                            match tr.get(&owner_key(&serial), false).await? {
                                Some(owner) if owner.len() == 8 => {
                                    if read_u64(&owner) != epoch {
                                        return Ok(VolumeOutcome::StaleOwner);
                                    }
                                }
                                Some(_) => return Err(CatalogError::Corrupt("owner epoch 长度")),
                                None => return Ok(VolumeOutcome::StaleOwner),
                            }
                            let placement = tr
                                .get(&placement_key(volume), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let placement = decode_placement(volume, &placement)?;
                            if placement.namespace != namespace
                                || placement.library_serial != serial
                            {
                                return Ok(VolumeOutcome::SerialConflict);
                            }
                            let assignment = tr
                                .get(&assignment_key(&serial, &placement.barcode), false)
                                .await?
                                .ok_or(CatalogError::Corrupt("卷放置缺少磁带分配"))?;
                            let assignment =
                                decode_assignment(&serial, &placement.barcode, &assignment)?;
                            require_assignment_index(&tr, &assignment).await?;
                            if assignment.namespace != namespace
                                || assignment.pool_id != placement.pool_id
                                || assignment.volume_id != Some(volume)
                                || tr
                                    .get(&barcode_key(&serial, &placement.barcode), false)
                                    .await?
                                    .as_deref()
                                    != Some(volume.as_bytes())
                            {
                                return Err(CatalogError::Corrupt("废弃卷放置与磁带分配不一致"));
                            }
                            if let Some(pending) =
                                tr.get(&reconcile_volume_key(volume), false).await?
                            {
                                let (pending_scan, pending_serial) = decode_ready_volume(&pending)?;
                                if pending_serial != serial
                                    || tr
                                        .get(&scan_key(volume, pending_scan.id), false)
                                        .await?
                                        .as_deref()
                                        != Some(
                                            encode_scan_manifest(
                                                pending_scan,
                                                &serial,
                                                ScanState::Retired,
                                            )
                                            .as_slice(),
                                        )
                                {
                                    return Ok(VolumeOutcome::ScanConflict);
                                }
                            }
                            let scan_key = scan_key(volume, scan.id);
                            let saved = tr.get(&scan_key, false).await?;
                            let state = saved.as_deref().map(decode_scan_manifest).transpose()?;
                            let ready_key = ready_volume_key(volume);
                            let ready = tr.get(&ready_key, false).await?;
                            if ready.is_none()
                                && state == Some((scan, serial.clone(), ScanState::Retired))
                            {
                                return Ok(VolumeOutcome::AlreadyRetired);
                            }
                            if ready.as_deref()
                                != Some(encode_ready_volume(scan, &serial).as_slice())
                                || state != Some((scan, serial.clone(), ScanState::Ready))
                            {
                                return Ok(VolumeOutcome::ScanConflict);
                            }
                            tr.set(
                                &scan_key,
                                &encode_scan_manifest(scan, &serial, ScanState::Retired),
                            );
                            tr.clear(&ready_key);
                            tr.set(&unavailable_volume_key(namespace, volume), &[]);
                            Ok(VolumeOutcome::Retired)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Release a discarded import's exclusive barcode claim. The caller must
    /// independently confirm that no media write was started by this candidate
    /// and that the current owner has fenced its predecessor. All FDB copies
    /// and volume scan references must already have been retired and purged.
    pub async fn release_discarded_tape(
        &self,
        namespace: Uuid,
        pool_id: Uuid,
        serial: &str,
        barcode: &str,
        expected_volume: Option<Uuid>,
        owner_epoch: u64,
    ) -> Result<ControlOutcome> {
        if namespace.is_nil()
            || pool_id.is_nil()
            || owner_epoch == 0
            || expected_volume.is_some_and(|id| id.is_nil())
        {
            return Err(CatalogError::Invalid("候选释放目标或 owner epoch 无效"));
        }
        validate_physical_serial(serial)?;
        validate_barcode(barcode)?;
        let serial = serial.to_owned();
        let barcode = barcode.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                let barcode = barcode.clone();
                async move {
                    requests::retry_result(
                        async {
                            let state = tr.get(&namespace_key(namespace), false).await?;
                            if state.as_deref().map(decode_namespace_state).transpose()?
                                != Some(NamespaceState::Discarded)
                            {
                                return Err(CatalogError::Invalid("只能释放废弃候选的磁带归属"));
                            }
                            match tr.get(&owner_key(&serial), false).await? {
                                Some(epoch) if epoch.len() == 8 => {
                                    if read_u64(&epoch) != owner_epoch {
                                        return Ok(ControlOutcome::StaleOwner);
                                    }
                                }
                                Some(_) => return Err(CatalogError::Corrupt("owner epoch 长度")),
                                None => return Ok(ControlOutcome::StaleOwner),
                            }
                            let record = assignment_key(&serial, &barcode);
                            let index = namespace_assignment_key(namespace, &serial, &barcode);
                            let reverse = barcode_key(&serial, &barcode);
                            let assigned = tr.get(&record, false).await?;
                            let indexed = tr.get(&index, false).await?;
                            let named = tr.get(&reverse, false).await?;
                            let Some(assigned) = assigned else {
                                return if indexed.is_some() || named.is_some() {
                                    Err(CatalogError::Corrupt("磁带归属记录缺失"))
                                } else {
                                    Ok(ControlOutcome::AlreadyAbsent)
                                };
                            };
                            let assigned = decode_assignment(&serial, &barcode, &assigned)?;
                            if assigned.namespace != namespace || assigned.pool_id != pool_id {
                                return Ok(ControlOutcome::BarcodeConflict);
                            }
                            if indexed.as_deref()
                                != Some(encode_namespace_assignment(&serial, &barcode).as_slice())
                            {
                                return Err(CatalogError::Corrupt(
                                    "磁带归属命名空间索引缺失或损坏",
                                ));
                            }
                            if assigned.volume_id != expected_volume {
                                return Ok(ControlOutcome::VolumeConflict);
                            }
                            if !read_page(&tr, &copy_prefix(namespace), None, 1, false)
                                .await?
                                .items
                                .is_empty()
                            {
                                return Err(CatalogError::Invalid("候选副本尚未清空"));
                            }
                            if let Some(volume) = assigned.volume_id {
                                if named.as_deref() != Some(volume.as_bytes()) {
                                    return Err(CatalogError::Corrupt("磁带条码反向索引"));
                                }
                                let placement = tr
                                    .get(&placement_key(volume), false)
                                    .await?
                                    .ok_or(CatalogError::Corrupt("卷放置主记录缺失"))?;
                                let placement = decode_placement(volume, &placement)?;
                                if placement.namespace != namespace
                                    || placement.pool_id != pool_id
                                    || placement.library_serial != serial
                                    || placement.barcode != barcode
                                {
                                    return Err(CatalogError::Corrupt("磁带分配与卷放置不一致"));
                                }
                                if tr.get(&ready_volume_key(volume), false).await?.is_some() {
                                    return Err(CatalogError::Invalid("卷仍处于就绪状态"));
                                }
                                if !read_page(
                                    &tr,
                                    &{
                                        let mut prefix = key(b'V');
                                        prefix.extend_from_slice(volume.as_bytes());
                                        prefix
                                    },
                                    None,
                                    1,
                                    false,
                                )
                                .await?
                                .items
                                .is_empty()
                                {
                                    return Err(CatalogError::Invalid("卷反向索引尚未清空"));
                                }
                                let mut scans = key(b'G');
                                scans.extend_from_slice(volume.as_bytes());
                                let scan_page = read_page(&tr, &scans, None, 100, false).await?;
                                if scan_page.next.is_some() {
                                    return Err(CatalogError::Invalid(
                                        "卷扫描记录超过单次释放核验上限",
                                    ));
                                }
                                for (scan_key, bytes) in scan_page.items {
                                    let (scan, saved_serial, state) = decode_scan_manifest(&bytes)?;
                                    if scan_key != self::scan_key(volume, scan.id)
                                        || saved_serial != serial
                                        || state != ScanState::Retired
                                    {
                                        return Err(CatalogError::Invalid("卷扫描尚未全部退役"));
                                    }
                                }
                                if let Some(pending) =
                                    tr.get(&reconcile_volume_key(volume), false).await?
                                {
                                    let (scan, saved_serial) = decode_ready_volume(&pending)?;
                                    if saved_serial != serial
                                        || tr
                                            .get(&self::scan_key(volume, scan.id), false)
                                            .await?
                                            .as_deref()
                                            != Some(
                                                encode_scan_manifest(
                                                    scan,
                                                    &serial,
                                                    ScanState::Retired,
                                                )
                                                .as_slice(),
                                            )
                                    {
                                        return Err(CatalogError::Invalid("卷扫描候选尚未退役"));
                                    }
                                }
                                tr.clear(&placement_key(volume));
                                tr.clear(&reverse);
                                tr.clear(&unavailable_volume_key(namespace, volume));
                            } else if named.is_some() {
                                return Err(CatalogError::Corrupt("未绑定磁带有卷反向索引"));
                            }
                            tr.clear(&record);
                            tr.clear(&index);
                            Ok(ControlOutcome::Unassigned)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    pub async fn tape_assignment(
        &self,
        serial: &str,
        barcode: &str,
    ) -> Result<Option<TapeAssignment>> {
        validate_physical_serial(serial)?;
        validate_barcode(barcode)?;
        let tr = self.db.create_trx()?;
        Self::verified_assignment(&tr, serial, barcode).await
    }

    pub(super) async fn verified_assignment(
        tr: &Transaction,
        serial: &str,
        barcode: &str,
    ) -> Result<Option<TapeAssignment>> {
        validate_physical_serial(serial)?;
        validate_barcode(barcode)?;
        let record = tr.get(&assignment_key(serial, barcode), false).await?;
        let reverse = tr.get(&barcode_key(serial, barcode), false).await?;
        let Some(record) = record else {
            return if reverse.is_some() {
                Err(CatalogError::Corrupt("卷放置缺少磁带分配"))
            } else {
                Ok(None)
            };
        };
        let assignment = decode_assignment(serial, barcode, &record)?;
        require_assignment_index(tr, &assignment).await?;
        if let Some(reverse) = reverse {
            let volume_id = uuid_from(&reverse)?;
            if assignment.volume_id != Some(volume_id) {
                return Err(CatalogError::Corrupt("磁带分配与卷放置不一致"));
            }
            let placement = tr
                .get(&placement_key(volume_id), true)
                .await?
                .ok_or(CatalogError::Corrupt("卷放置主记录缺失"))?;
            let placement = decode_placement(volume_id, &placement)?;
            if placement.namespace != assignment.namespace
                || placement.pool_id != assignment.pool_id
                || placement.library_serial != serial
                || placement.barcode != barcode
            {
                return Err(CatalogError::Corrupt("磁带分配与卷放置不一致"));
            }
        } else if assignment.volume_id.is_some() {
            return Err(CatalogError::Corrupt("已绑定磁带缺少条码反向索引"));
        }
        require_namespace_readable(tr, assignment.namespace).await?;
        require_pool_library(tr, assignment.namespace, assignment.pool_id, serial).await?;
        Ok(Some(assignment))
    }

    /// Bind a physical tape once, after the owner verifies its LTFS identity.
    /// The global barcode key prevents two logical namespaces from claiming the
    /// same cartridge in one physical changer.
    pub async fn register_volume_placement(
        &self,
        placement: &VolumePlacement,
        owner_epoch: u64,
    ) -> Result<ControlOutcome> {
        validate_placement(placement)?;
        if owner_epoch == 0 {
            return Err(CatalogError::Invalid("owner epoch 必须大于零"));
        }
        let p = placement.clone();
        let record = placement_key(p.volume_id);
        let reverse = barcode_key(&p.library_serial, &p.barcode);
        let value = encode_placement(&p);
        self.db
            .run(|tr, _| {
                let (p, record, reverse, value) =
                    (p.clone(), record.clone(), reverse.clone(), value.clone());
                async move {
                    requests::retry_result(
                        async {
                            require_managed_pool_library(
                                &tr,
                                p.namespace,
                                p.pool_id,
                                &p.library_serial,
                            )
                            .await?;
                            let assigned = tr
                                .get(&assignment_key(&p.library_serial, &p.barcode), false)
                                .await?
                                .ok_or(CatalogError::Invalid("磁带尚未分配"))?;
                            let assigned =
                                decode_assignment(&p.library_serial, &p.barcode, &assigned)?;
                            if assigned.namespace != p.namespace || assigned.pool_id != p.pool_id {
                                return Err(CatalogError::Invalid("磁带分配与卷放置不一致"));
                            }
                            require_assignment_index(&tr, &assigned).await?;
                            if assigned.volume_id.is_some_and(|id| id != p.volume_id) {
                                return Ok(ControlOutcome::BarcodeConflict);
                            }
                            let old = tr.get(&record, false).await?;
                            let named = tr.get(&reverse, false).await?;
                            if let Some(ref old) = old {
                                decode_placement(p.volume_id, old)?;
                            }
                            if let Some(ref named) = named {
                                uuid_from(named)?;
                            }
                            match (old.as_deref(), named.as_deref()) {
                                (Some(saved), Some(id))
                                    if saved == value && id == p.volume_id.as_bytes() =>
                                {
                                    if assigned.volume_id != Some(p.volume_id) {
                                        return Err(CatalogError::Corrupt("磁带分配绑定标记缺失"));
                                    }
                                    return Ok(ControlOutcome::AlreadyExists);
                                }
                                (Some(saved), None) if saved == value => {
                                    return Err(CatalogError::Corrupt("磁带条码反向索引缺失"));
                                }
                                (None, Some(id)) if id == p.volume_id.as_bytes() => {
                                    return Err(CatalogError::Corrupt("卷放置主记录缺失"));
                                }
                                (Some(_), _) => return Ok(ControlOutcome::VolumeConflict),
                                (_, Some(_)) => return Ok(ControlOutcome::BarcodeConflict),
                                (None, None) => {}
                            }
                            if assigned.volume_id.is_some() {
                                return Err(CatalogError::Corrupt("已绑定磁带缺少卷放置记录"));
                            }
                            if tr
                                .get(&ready_volume_key(p.volume_id), false)
                                .await?
                                .is_some()
                                || tr
                                    .get(&reconcile_volume_key(p.volume_id), false)
                                    .await?
                                    .is_some()
                            {
                                return Err(CatalogError::Invalid(
                                    "已有扫描的卷不能补登记放置关系",
                                ));
                            }
                            match tr.get(&owner_key(&p.library_serial), false).await? {
                                Some(bytes) if bytes.len() == 8 => {
                                    if read_u64(&bytes) != owner_epoch {
                                        return Ok(ControlOutcome::StaleOwner);
                                    }
                                }
                                Some(_) => return Err(CatalogError::Corrupt("owner epoch 长度")),
                                None => return Ok(ControlOutcome::StaleOwner),
                            }
                            tr.set(&record, &value);
                            tr.set(&reverse, p.volume_id.as_bytes());
                            tr.set(&unavailable_volume_key(p.namespace, p.volume_id), &[]);
                            tr.set(
                                &assignment_key(&p.library_serial, &p.barcode),
                                &encode_assignment(p.namespace, p.pool_id, Some(p.volume_id)),
                            );
                            Ok(ControlOutcome::Created)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    pub async fn volume_placement(&self, volume_id: Uuid) -> Result<Option<VolumePlacement>> {
        if volume_id.is_nil() {
            return Err(CatalogError::Invalid("卷 ID 不能为 nil"));
        }
        let tr = self.db.create_trx()?;
        verified_placement(&tr, volume_id).await
    }

    pub async fn pool(&self, namespace: Uuid, id: Uuid) -> Result<Option<PoolRecord>> {
        if id.is_nil() {
            return Err(CatalogError::Invalid("pool ID 不能为 nil"));
        }
        let tr = self.db.create_trx()?;
        require_control_namespace(&tr, namespace).await?;
        tr.get(&pool_key(namespace, id), true)
            .await?
            .map(|value| decode_pool(id, &value))
            .transpose()
    }

    pub async fn list_pools(
        &self,
        namespace: Uuid,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<PoolRecord>> {
        let prefix = pool_prefix(namespace);
        if after
            .is_some_and(|cursor| !cursor.starts_with(&prefix) || cursor.len() != prefix.len() + 16)
        {
            return Err(CatalogError::Invalid("pool 分页游标无效"));
        }
        let rows = self
            .page_range(&prefix, after, limit, Some(namespace), None)
            .await?;
        let items = rows
            .items
            .into_iter()
            .map(|(k, v)| decode_pool(uuid_from(&k[prefix.len()..])?, &v))
            .collect::<Result<Vec<_>>>()?;
        Ok(Page {
            items,
            next: rows.next,
        })
    }

    pub async fn list_physical_libraries(
        &self,
        namespace: Uuid,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Page<String>> {
        let prefix = library_prefix(namespace);
        if after.is_some_and(|cursor| {
            !cursor.starts_with(&prefix)
                || cursor.len() > prefix.len() + 258
                || decode_name(&cursor[prefix.len()..]).is_err()
        }) {
            return Err(CatalogError::Invalid("带库分页游标无效"));
        }
        let rows = self
            .page_range(&prefix, after, limit, Some(namespace), None)
            .await?;
        let items = rows
            .items
            .into_iter()
            .map(|(k, v)| {
                if !v.is_empty() {
                    return Err(CatalogError::Corrupt("物理带库归属记录"));
                }
                let name = decode_name(&k[prefix.len()..])?;
                let serial =
                    String::from_utf8(name).map_err(|_| CatalogError::Corrupt("带库序列号编码"))?;
                validate_physical_serial(&serial)
                    .map_err(|_| CatalogError::Corrupt("带库序列号"))?;
                Ok(serial)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Page {
            items,
            next: rows.next,
        })
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn pool_codec_rejects_bad_lengths_and_values() {
        let id = Uuid::new_v4();
        let record = PoolRecord {
            id,
            library_serial: "SERIAL_A".into(),
            name: "归档".into(),
            file_limit: 200_000,
            policy: PoolPolicy {
                media_restriction: "L9".into(),
                mount_limit: 7,
                node_group: "G2".into(),
                low_space_warning_enabled: false,
                low_space_warning_threshold: 32767,
                no_space_warning_enabled: true,
            },
            revision: 1,
        };
        let encoded = encode_pool(&record);
        let mut legacy = encoded.clone();
        legacy[0] = 1;
        assert!(
            decode_pool(id, &legacy).is_err(),
            "没有明确带库归属的旧池记录不得被默认为可读"
        );
        for serial in ["", "wrong/serial"] {
            assert!(
                validate_pool(&PoolRecord {
                    library_serial: serial.into(),
                    ..record.clone()
                })
                .is_err()
            );
        }
        assert_eq!(decode_pool(id, &encoded).unwrap(), record);
        for n in 0..encoded.len() {
            assert!(decode_pool(id, &encoded[..n]).is_err());
        }
        let mut broken = encoded.clone();
        broken[1..9].fill(0);
        assert!(decode_pool(id, &broken).is_err());
        broken = encoded.clone();
        broken[21] = 0b100;
        assert!(decode_pool(id, &broken).is_err());
        broken = encoded.clone();
        broken[24] = 255;
        assert!(decode_pool(id, &broken).is_err());
        assert!(
            validate_pool(&PoolRecord {
                policy: PoolPolicy {
                    media_restriction: "L99".into(),
                    ..record.policy.clone()
                },
                ..record.clone()
            })
            .is_err()
        );
        assert!(
            validate_pool(&PoolRecord {
                policy: PoolPolicy {
                    mount_limit: 32768,
                    ..record.policy.clone()
                },
                ..record.clone()
            })
            .is_err()
        );
        assert!(
            validate_pool(&PoolRecord {
                name: "x/y".into(),
                ..record
            })
            .is_err()
        );
    }

    #[test]
    fn placement_codec_rejects_truncation_and_identity_changes() {
        let p = VolumePlacement {
            volume_id: Uuid::new_v4(),
            namespace: Uuid::new_v4(),
            pool_id: Uuid::new_v4(),
            library_serial: "SERIAL_A".into(),
            barcode: "AA0001L8".into(),
        };
        let bytes = encode_placement(&p);
        assert_eq!(decode_placement(p.volume_id, &bytes).unwrap(), p);
        for end in 0..bytes.len() {
            assert!(decode_placement(p.volume_id, &bytes[..end]).is_err());
        }
        let mut wrong = bytes.clone();
        wrong[0] = 2;
        assert!(decode_placement(p.volume_id, &wrong).is_err());
        wrong = bytes;
        wrong[33] = 127;
        assert!(decode_placement(p.volume_id, &wrong).is_err());
    }

    #[test]
    fn assignment_codec_preserves_pending_and_bound_identity() {
        let namespace = Uuid::new_v4();
        let pool = Uuid::new_v4();
        let volume = Uuid::new_v4();
        for expected in [None, Some(volume)] {
            let encoded = encode_assignment(namespace, pool, expected);
            let assignment = decode_assignment("SERIAL_A", "AA0001L8", &encoded).unwrap();
            assert_eq!(assignment.namespace, namespace);
            assert_eq!(assignment.pool_id, pool);
            assert_eq!(assignment.volume_id, expected);
            for end in 0..encoded.len() {
                assert!(decode_assignment("SERIAL_A", "AA0001L8", &encoded[..end]).is_err());
            }
            let mut invalid = encoded;
            invalid[0] = 3;
            assert!(decode_assignment("SERIAL_A", "AA0001L8", &invalid).is_err());
        }
        let mut nil_volume = encode_assignment(namespace, pool, Some(volume));
        nil_volume[33..49].fill(0);
        assert!(decode_assignment("SERIAL_A", "AA0001L8", &nil_volume).is_err());
        let index = namespace_assignment_key(namespace, "SERIAL_A", "AA0001L8");
        let value = encode_namespace_assignment("SERIAL_A", "AA0001L8");
        assert_eq!(
            decode_namespace_assignment(namespace, &index, &value).unwrap(),
            ("SERIAL_A".into(), "AA0001L8".into())
        );
        for end in 0..value.len() {
            assert!(decode_namespace_assignment(namespace, &index, &value[..end]).is_err());
        }
        assert!(decode_namespace_assignment(Uuid::new_v4(), &index, &value).is_err());
        let mut wrong = value;
        *wrong.last_mut().unwrap() = b'9';
        assert!(decode_namespace_assignment(namespace, &index, &wrong).is_err());
    }

    async fn managed_placement_exercise(catalog: &ManagedCatalog, cluster_file: &str) {
        let namespace = Uuid::new_v4();
        assert!(catalog.create_managed_namespace(namespace).await.unwrap());
        assert!(!catalog.create_managed_namespace(namespace).await.unwrap());
        assert!(catalog.create_namespace(namespace).await.is_err());
        let serial = format!("TEST-{}", Uuid::new_v4());
        let other_serial = format!("TEST-{}", Uuid::new_v4());
        assert_eq!(
            catalog
                .register_physical_library(namespace, &serial)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .register_physical_library(namespace, &other_serial)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
        assert!(
            catalog
                .advance_owner_epoch(&other_serial, 10)
                .await
                .unwrap()
        );
        let pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "单库归档".into(),
            file_limit: 200_000,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        assert_eq!(
            catalog.create_pool(namespace, &pool).await.unwrap(),
            ControlOutcome::Created
        );
        let p = VolumePlacement {
            volume_id: Uuid::new_v4(),
            namespace,
            pool_id: pool.id,
            library_serial: serial.clone(),
            barcode: "AA0001L8".into(),
        };
        assert!(catalog.register_volume_placement(&p, 10).await.is_err());
        assert_eq!(
            catalog
                .assign_tape(namespace, pool.id, &serial, &p.barcode)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .tape_assignment(&serial, &p.barcode)
                .await
                .unwrap()
                .unwrap()
                .volume_id,
            None
        );
        assert_eq!(
            catalog
                .assign_tape(namespace, pool.id, &serial, "PENDINGL8")
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .unassign_tape(namespace, pool.id, &serial, "PENDINGL8")
                .await
                .unwrap(),
            ControlOutcome::Unassigned
        );
        assert!(
            catalog
                .tape_assignment(&serial, "PENDINGL8")
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            catalog.register_volume_placement(&p, 9).await.unwrap(),
            ControlOutcome::StaleOwner
        );
        assert_eq!(
            catalog.register_volume_placement(&p, 10).await.unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog.register_volume_placement(&p, 9).await.unwrap(),
            ControlOutcome::AlreadyExists
        );
        assert_eq!(
            catalog
                .tape_assignment(&serial, &p.barcode)
                .await
                .unwrap()
                .unwrap()
                .volume_id,
            Some(p.volume_id)
        );
        assert_eq!(
            catalog
                .unassign_tape(namespace, pool.id, &serial, &p.barcode)
                .await
                .unwrap(),
            ControlOutcome::VolumeConflict
        );
        let reverse = barcode_key(&serial, &p.barcode);
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&reverse);
        tr.commit().await.unwrap();
        assert_eq!(
            catalog
                .unassign_tape(namespace, pool.id, &serial, &p.barcode)
                .await
                .unwrap(),
            ControlOutcome::VolumeConflict
        );
        assert!(catalog.tape_assignment(&serial, &p.barcode).await.is_err());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&reverse, p.volume_id.as_bytes());
        tr.commit().await.unwrap();
        assert_eq!(
            ManagedCatalog::open(Some(cluster_file))
                .unwrap()
                .volume_placement(p.volume_id)
                .await
                .unwrap(),
            Some(p.clone())
        );
        assert_eq!(
            catalog
                .assign_tape(namespace, pool.id, &serial, "AA0002L8")
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .register_volume_placement(
                    &VolumePlacement {
                        barcode: "AA0002L8".into(),
                        ..p.clone()
                    },
                    10
                )
                .await
                .unwrap(),
            ControlOutcome::VolumeConflict
        );
        assert_eq!(
            catalog
                .register_volume_placement(
                    &VolumePlacement {
                        volume_id: Uuid::new_v4(),
                        ..p.clone()
                    },
                    10
                )
                .await
                .unwrap(),
            ControlOutcome::BarcodeConflict
        );
        let other_library = VolumePlacement {
            volume_id: Uuid::new_v4(),
            library_serial: other_serial.clone(),
            ..p.clone()
        };
        assert!(
            catalog
                .assign_tape(namespace, pool.id, &other_serial, &other_library.barcode)
                .await
                .is_err()
        );
        assert!(
            catalog
                .register_volume_placement(&other_library, 10)
                .await
                .is_err()
        );
        assert!(
            catalog
                .update_pool(
                    namespace,
                    &pool,
                    &PoolRecord {
                        library_serial: other_serial.clone(),
                        revision: pool.revision + 1,
                        ..pool.clone()
                    }
                )
                .await
                .is_err()
        );
        let other_pool = PoolRecord {
            id: Uuid::new_v4(),
            name: "另一带库的归档池".into(),
            library_serial: other_serial.clone(),
            ..pool.clone()
        };
        assert_eq!(
            catalog.create_pool(namespace, &other_pool).await.unwrap(),
            ControlOutcome::Created
        );
        let other_library = VolumePlacement {
            pool_id: other_pool.id,
            ..other_library
        };
        assert_eq!(
            catalog
                .assign_tape(
                    namespace,
                    other_pool.id,
                    &other_serial,
                    &other_library.barcode
                )
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .register_volume_placement(&other_library, 10)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        // Read-side proofs also reject a corrupted binding, independently of admission.
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &pool_key(namespace, pool.id),
            &encode_pool(&PoolRecord {
                library_serial: other_serial.clone(),
                ..pool.clone()
            }),
        );
        tr.commit().await.unwrap();
        assert!(catalog.tape_assignment(&serial, &p.barcode).await.is_err());
        let tr = catalog.db.create_trx().unwrap();
        assert!(verified_placement(&tr, p.volume_id).await.is_err());
        drop(tr);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&pool_key(namespace, pool.id), &encode_pool(&pool));
        tr.commit().await.unwrap();
        let another_namespace = Uuid::new_v4();
        assert!(
            catalog
                .create_managed_namespace(another_namespace)
                .await
                .unwrap()
        );
        assert_eq!(
            catalog
                .register_physical_library(another_namespace, &serial)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog.create_pool(another_namespace, &pool).await.unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .assign_tape(another_namespace, pool.id, &serial, &p.barcode)
                .await
                .unwrap(),
            ControlOutcome::BarcodeConflict
        );
        assert!(
            catalog
                .register_volume_placement(
                    &VolumePlacement {
                        volume_id: Uuid::new_v4(),
                        namespace: another_namespace,
                        ..p.clone()
                    },
                    10
                )
                .await
                .is_err()
        );
        assert!(
            catalog
                .register_volume_placement(
                    &VolumePlacement {
                        pool_id: Uuid::new_v4(),
                        volume_id: Uuid::new_v4(),
                        barcode: "AA0003L8".into(),
                        ..p.clone()
                    },
                    10
                )
                .await
                .is_err()
        );
        let race_a = VolumePlacement {
            volume_id: Uuid::new_v4(),
            barcode: "AA0005L8".into(),
            ..p.clone()
        };
        let race_b = VolumePlacement {
            volume_id: Uuid::new_v4(),
            ..race_a.clone()
        };
        assert_eq!(
            catalog
                .assign_tape(namespace, pool.id, &serial, &race_a.barcode)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        let (a, b) = tokio::join!(
            catalog.register_volume_placement(&race_a, 10),
            catalog.register_volume_placement(&race_b, 10)
        );
        assert!(matches!(
            (a.unwrap(), b.unwrap()),
            (ControlOutcome::Created, ControlOutcome::BarcodeConflict)
                | (ControlOutcome::BarcodeConflict, ControlOutcome::Created)
        ));

        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        let make = |volume_id: Uuid, pool_id: Option<Uuid>, library_serial: String| {
            test_receipt(CommitReceipt {
                namespace,
                parent: Uuid::nil(),
                name: b"file".to_vec(),
                file_id: Uuid::new_v4(),
                expected_version: None,
                operation_id: Uuid::new_v4(),
                request_epoch: 1,
                copy_id: Uuid::new_v4(),
                library_serial,
                pool_id,
                owner_epoch: 10,
                volume_id,
                volume_generation: 1,
                volume_scan_id: scan.id,
                ltfs_file_uid: 1,
                size: 7,
                content_digest: ContentDigest::Sha256([3; 32]),
                metadata_digest: None,
            })
        };
        let missing_pool = make(p.volume_id, None, serial.clone());
        assert_eq!(
            catalog.register_request(&missing_pool).await.unwrap(),
            PublishOutcome::PlacementMismatch
        );
        let wrong_pool = make(p.volume_id, Some(Uuid::new_v4()), serial.clone());
        assert_eq!(
            catalog.register_request(&wrong_pool).await.unwrap(),
            PublishOutcome::PlacementMismatch
        );
        let wrong_serial = make(p.volume_id, Some(pool.id), other_serial.clone());
        assert_eq!(
            catalog.register_request(&wrong_serial).await.unwrap(),
            PublishOutcome::PlacementMismatch
        );
        let unknown_volume = make(Uuid::new_v4(), Some(pool.id), serial.clone());
        assert_eq!(
            catalog.register_request(&unknown_volume).await.unwrap(),
            PublishOutcome::PlacementMismatch
        );
        let valid = make(p.volume_id, Some(pool.id), serial.clone());
        let legacy = Uuid::new_v4();
        assert!(catalog.create_namespace(legacy).await.unwrap());
        let mut legacy_on_managed_volume = valid.clone();
        legacy_on_managed_volume.namespace = legacy;
        legacy_on_managed_volume.pool_id = None;
        legacy_on_managed_volume.operation_id = Uuid::new_v4();
        legacy_on_managed_volume.copy_id = derive_copy_id(&legacy_on_managed_volume);
        assert_eq!(
            catalog
                .register_request(&legacy_on_managed_volume)
                .await
                .unwrap(),
            PublishOutcome::PlacementMismatch
        );
        assert_eq!(
            catalog.register_request(&valid).await.unwrap(),
            PublishOutcome::Registered
        );
        assert_eq!(
            catalog
                .pending_requests(namespace, None, 10)
                .await
                .unwrap()
                .items,
            vec![valid.clone()]
        );
        let altered = VolumePlacement {
            pool_id: Uuid::new_v4(),
            ..p.clone()
        };
        for value in [encode_placement(&altered), encode_placement(&p)] {
            let record = placement_key(p.volume_id);
            catalog
                .db
                .run(|tr, _| {
                    let (record, value) = (record.clone(), value.clone());
                    async move {
                        tr.set(&record, &value);
                        Ok(())
                    }
                })
                .await
                .unwrap();
            if value == encode_placement(&altered) {
                assert_eq!(
                    catalog.register_request(&valid).await.unwrap(),
                    PublishOutcome::PlacementMismatch
                );
                assert_eq!(
                    catalog.publish(&valid).await.unwrap(),
                    PublishOutcome::PlacementMismatch
                );
            }
        }
        assert_eq!(
            catalog
                .begin_volume_reconcile(&other_serial, 10, p.volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::SerialConflict
        );
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, p.volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        super::super::tests::verify_scan(catalog, &serial, 10, p.volume_id, scan, &[]).await;
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, p.volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        assert_eq!(
            catalog.publish(&valid).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        // A managed serving copy survives history compaction and a complete
        // maintenance cycle, including driver takeover on a reopened client.
        let before = catalog
            .physical_copy(namespace, valid.file_id, valid.copy_id)
            .await
            .unwrap()
            .unwrap();
        let page = catalog
            .compact_superseded_copies(namespace, None, 100)
            .await
            .unwrap();
        assert_eq!((page.scanned, page.compacted), (1, 0));
        let mut driver = catalog.maintenance(namespace).await.unwrap();
        driver.poll().await.unwrap();
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        let mut successor = reopened.maintenance(namespace).await.unwrap();
        assert!(matches!(
            driver.poll().await,
            Err(CatalogError::MaintenanceFenced)
        ));
        for _ in 0..5 {
            successor.poll().await.unwrap();
        }
        assert_eq!(
            reopened
                .physical_copy(namespace, valid.file_id, valid.copy_id)
                .await
                .unwrap(),
            Some(before)
        );
        let source_key = source_key(p.volume_id, scan.id);
        let tr = catalog.db.create_trx().unwrap();
        let saved_source = tr.get(&source_key, false).await.unwrap().unwrap();
        tr.clear(&source_key);
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .physical_copy(namespace, valid.file_id, valid.copy_id)
                .await,
            Err(CatalogError::Incomplete)
        ));
        let mut path = vec![b'/'];
        path.extend_from_slice(&valid.name);
        assert!(matches!(
            catalog.resolve_path(namespace, &path).await,
            Err(CatalogError::Incomplete)
        ));
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, p.volume_id, scan)
                .await
                .unwrap(),
            VolumeOutcome::NotVerified
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&source_key, &saved_source);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .physical_copy(namespace, valid.file_id, valid.copy_id)
                .await
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            catalog.resolve_path(namespace, &path).await.unwrap(),
            Some(ResolvedPath::File { id, .. }) if id == valid.file_id
        ));
        assert_eq!(
            ManagedCatalog::open(Some(cluster_file))
                .unwrap()
                .register_request(&valid)
                .await
                .unwrap(),
            PublishOutcome::AlreadyApplied(1)
        );

        let late = Uuid::new_v4();
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, late, scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        assert_eq!(
            catalog
                .assign_tape(namespace, pool.id, &serial, "AA0004L8")
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert!(
            catalog
                .register_volume_placement(
                    &VolumePlacement {
                        volume_id: late,
                        barcode: "AA0004L8".into(),
                        ..p
                    },
                    10
                )
                .await
                .is_err()
        );
    }

    async fn managed_import_exercise(catalog: &ManagedCatalog) {
        let namespace = Uuid::new_v4();
        let serial = format!("IMPORT-{}", Uuid::new_v4());
        let pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "受管导入池".into(),
            file_limit: 200_000,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        let volume = Uuid::new_v4();
        let scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        let placement = VolumePlacement {
            volume_id: volume,
            namespace,
            pool_id: pool.id,
            library_serial: serial.clone(),
            barcode: "IMPORTL8".into(),
        };
        let receipt = test_receipt(CommitReceipt {
            namespace,
            parent: Uuid::nil(),
            name: b"uid-1".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            copy_id: Uuid::new_v4(),
            library_serial: serial.clone(),
            pool_id: Some(pool.id),
            owner_epoch: 10,
            volume_id: volume,
            volume_generation: scan.generation,
            volume_scan_id: scan.id,
            ltfs_file_uid: 1,
            size: 17,
            content_digest: ContentDigest::Sha256([5; 32]),
            metadata_digest: None,
        });
        assert!(catalog.begin_managed_import(namespace).await.unwrap());
        assert!(!catalog.begin_managed_import(namespace).await.unwrap());
        assert!(catalog.begin_import(namespace).await.is_err());
        assert_eq!(
            catalog
                .register_physical_library(namespace, &serial)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog.create_pool(namespace, &pool).await.unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .assign_tape(namespace, pool.id, &serial, &placement.barcode)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert!(catalog.advance_owner_epoch(&serial, 10).await.unwrap());
        assert_eq!(
            catalog
                .register_volume_placement(&placement, 10)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, volume, scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        assert_eq!(
            catalog.register_request(&receipt).await.unwrap(),
            PublishOutcome::Registered
        );
        assert_eq!(
            catalog.publish_imported(&receipt).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        assert!(matches!(
            catalog
                .resolve_child(namespace, Uuid::nil(), &receipt.name)
                .await,
            Err(CatalogError::Incomplete)
        ));
        let mut path = vec![b'/'];
        path.extend_from_slice(&receipt.name);
        assert!(matches!(
            catalog.resolve_path(namespace, &path).await,
            Err(CatalogError::Incomplete)
        ));
        let source_cache = super::super::tests::verify_scan_keep_cache(
            catalog,
            &serial,
            10,
            volume,
            scan,
            std::slice::from_ref(&receipt),
        )
        .await
        .expect("受管卷来源缓存");
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, volume, scan)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        let binding = ImportSourceBinding {
            placement: placement.clone(),
            scan,
            source: catalog.volume_source(volume, scan).await.unwrap(),
        };
        catalog
            .verify_import_source_binding(&binding)
            .await
            .unwrap();
        let mut wrong = binding.clone();
        wrong.source.sha256[0] ^= 1;
        assert!(catalog.verify_import_source_binding(&wrong).await.is_err());
        wrong = binding.clone();
        wrong.scan.id = Uuid::new_v4();
        assert!(catalog.verify_import_source_binding(&wrong).await.is_err());
        wrong = binding.clone();
        wrong.placement.barcode = "WRONGL8".into();
        assert!(catalog.verify_import_source_binding(&wrong).await.is_err());
        super::super::tests::verify_import_namespace(
            catalog,
            namespace,
            std::slice::from_ref(&receipt),
            &[],
        )
        .await;
        assert!(catalog.seal_import(namespace).await.unwrap());
        assert!(
            catalog
                .verify_import_source_binding(&binding)
                .await
                .is_err()
        );
        assert_eq!(
            catalog.pool(namespace, pool.id).await.unwrap(),
            Some(pool.clone())
        );
        assert_eq!(
            catalog
                .resolve_child(namespace, Uuid::nil(), &receipt.name)
                .await
                .unwrap(),
            Some(receipt.file_id)
        );
        assert!(matches!(
            catalog.resolve_path(namespace, &path).await.unwrap(),
            Some(ResolvedPath::File {
                id,
                head,
                copy,
                placement: found,
                ..
            }) if id == receipt.file_id
                && head.size == receipt.size
                && copy.ltfs_file_uid == receipt.ltfs_file_uid
                && found == placement
        ));
        let read = catalog
            .resolve_read(namespace, &path, &source_cache)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.id, receipt.file_id);
        assert_eq!(read.physical.path, "uid-1");
        assert_eq!(read.physical.length, receipt.size);
        let copy_key = copy_key(namespace, receipt.file_id, receipt.copy_id);
        let tr = catalog.db.create_trx().unwrap();
        let original_copy = tr.get(&copy_key, false).await.unwrap().unwrap();
        let mut wrong_uid = original_copy.to_vec();
        let uid_offset = wrong_uid.len() - 16;
        wrong_uid[uid_offset..uid_offset + 8].copy_from_slice(&2u64.to_be_bytes());
        tr.set(&copy_key, &wrong_uid);
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog.resolve_read(namespace, &path, &source_cache).await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&copy_key, &original_copy);
        tr.commit().await.unwrap();
        std::fs::remove_file(source_cache).unwrap();
        assert_eq!(
            catalog.resolve_path(namespace, b"/missing").await.unwrap(),
            None
        );
        assert_eq!(
            catalog.resolve_path(namespace, b"/").await.unwrap(),
            Some(ResolvedPath::Directory {
                id: Uuid::nil(),
                attributes: None
            })
        );
        let new_volume = Uuid::new_v4();
        let new_barcode = "IMPORT2L8";
        assert_eq!(
            catalog
                .assign_tape(namespace, pool.id, &serial, new_barcode)
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .register_volume_placement(
                    &VolumePlacement {
                        volume_id: new_volume,
                        barcode: new_barcode.into(),
                        ..placement.clone()
                    },
                    10,
                )
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert!(matches!(
            catalog.resolve_path(namespace, b"/missing").await,
            Err(CatalogError::Incomplete)
        ));
        assert!(matches!(
            catalog.resolve_path(namespace, &path).await.unwrap(),
            Some(ResolvedPath::File { .. })
        ));
        let abandoned_scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, new_volume, abandoned_scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        assert_eq!(
            catalog
                .cancel_volume_reconcile(&serial, 10, new_volume, abandoned_scan)
                .await
                .unwrap(),
            VolumeOutcome::Cancelled
        );
        assert!(matches!(
            catalog.resolve_path(namespace, b"/missing").await,
            Err(CatalogError::Incomplete)
        ));
        let ready_scan = VolumeScan {
            generation: 2,
            id: Uuid::new_v4(),
        };
        assert_eq!(
            catalog
                .begin_volume_reconcile(&serial, 10, new_volume, ready_scan)
                .await
                .unwrap(),
            VolumeOutcome::Reconciling
        );
        super::super::tests::verify_scan(catalog, &serial, 10, new_volume, ready_scan, &[]).await;
        assert_eq!(
            catalog
                .mark_volume_ready(&serial, 10, new_volume, ready_scan)
                .await
                .unwrap(),
            VolumeOutcome::Ready
        );
        assert_eq!(
            catalog.resolve_path(namespace, b"/missing").await.unwrap(),
            None
        );
        assert_eq!(
            catalog
                .tape_assignment(&serial, &placement.barcode)
                .await
                .unwrap()
                .unwrap()
                .volume_id,
            Some(volume)
        );

        let abandoned = Uuid::new_v4();
        let abandoned_serial = format!("IMPORT-{}", Uuid::new_v4());
        let abandoned_pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: abandoned_serial.clone(),
            name: "废弃候选池".into(),
            file_limit: 100,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        let abandoned_volume = Uuid::new_v4();
        let abandoned_scan = VolumeScan {
            generation: 1,
            id: Uuid::new_v4(),
        };
        catalog.begin_managed_import(abandoned).await.unwrap();
        catalog
            .register_physical_library(abandoned, &abandoned_serial)
            .await
            .unwrap();
        catalog
            .create_pool(abandoned, &abandoned_pool)
            .await
            .unwrap();
        catalog
            .assign_tape(abandoned, abandoned_pool.id, &abandoned_serial, "ABANDONL8")
            .await
            .unwrap();
        catalog
            .advance_owner_epoch(&abandoned_serial, 10)
            .await
            .unwrap();
        let abandoned_placement = VolumePlacement {
            volume_id: abandoned_volume,
            namespace: abandoned,
            pool_id: abandoned_pool.id,
            library_serial: abandoned_serial.clone(),
            barcode: "ABANDONL8".into(),
        };
        catalog
            .register_volume_placement(&abandoned_placement, 10)
            .await
            .unwrap();
        catalog
            .begin_volume_reconcile(&abandoned_serial, 10, abandoned_volume, abandoned_scan)
            .await
            .unwrap();
        let abandoned_file = test_receipt(CommitReceipt {
            namespace: abandoned,
            parent: Uuid::nil(),
            name: b"discarded-file".to_vec(),
            file_id: Uuid::new_v4(),
            expected_version: None,
            operation_id: Uuid::new_v4(),
            request_epoch: 1,
            copy_id: Uuid::new_v4(),
            library_serial: abandoned_serial.clone(),
            pool_id: Some(abandoned_pool.id),
            owner_epoch: 10,
            volume_id: abandoned_volume,
            volume_generation: abandoned_scan.generation,
            volume_scan_id: abandoned_scan.id,
            ltfs_file_uid: 1,
            size: 3,
            content_digest: ContentDigest::Sha256([9; 32]),
            metadata_digest: None,
        });
        assert_eq!(
            catalog.register_request(&abandoned_file).await.unwrap(),
            PublishOutcome::Registered
        );
        assert_eq!(
            catalog.publish_imported(&abandoned_file).await.unwrap(),
            PublishOutcome::Applied(1)
        );
        catalog
            .cancel_volume_reconcile(&abandoned_serial, 10, abandoned_volume, abandoned_scan)
            .await
            .unwrap();
        assert!(catalog.discard_import(abandoned).await.unwrap());
        assert!(
            catalog
                .cancel_volume_reconcile(&abandoned_serial, 10, abandoned_volume, abandoned_scan)
                .await
                .is_err(),
            "废弃后不能绕过专用退役流程修改扫描"
        );
        assert_eq!(
            catalog
                .release_discarded_tape(
                    abandoned,
                    abandoned_pool.id,
                    &abandoned_serial,
                    "ABANDONL8",
                    Some(abandoned_volume),
                    9
                )
                .await
                .unwrap(),
            ControlOutcome::StaleOwner
        );
        assert!(
            catalog
                .release_discarded_tape(
                    abandoned,
                    abandoned_pool.id,
                    &abandoned_serial,
                    "ABANDONL8",
                    Some(abandoned_volume),
                    10
                )
                .await
                .is_err()
        );

        for _ in 0..10 {
            match catalog.purge_discarded_namespace(abandoned, 100).await {
                Ok(progress) if !progress.complete => continue,
                Err(CatalogError::Invalid(_)) => break,
                result => panic!("unexpected managed candidate purge: {result:?}"),
            }
        }
        assert_eq!(
            catalog
                .release_discarded_tape(
                    abandoned,
                    abandoned_pool.id,
                    &abandoned_serial,
                    "ABANDONL8",
                    Some(abandoned_volume),
                    10
                )
                .await
                .unwrap(),
            ControlOutcome::Unassigned
        );
        loop {
            if catalog
                .purge_discarded_namespace(abandoned, 100)
                .await
                .unwrap()
                .complete
            {
                break;
            }
        }
        assert!(
            catalog
                .tape_assignment(&abandoned_serial, "ABANDONL8")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER 指向隔离 FDB；验证池的单库归属及并发控制，不接触设备"]
    async fn isolated_pool_library_boundaries() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        exercise(&catalog, &cluster).await;
    }

    pub(crate) async fn exercise(catalog: &ManagedCatalog, cluster_file: &str) {
        let ns = Uuid::new_v4();
        let other = Uuid::new_v4();
        assert!(catalog.create_namespace(ns).await.unwrap());
        assert!(catalog.create_namespace(other).await.unwrap());
        assert_eq!(
            catalog.list_node_groups(ns, None, 1).await.unwrap().items,
            vec!["G0"]
        );
        assert!(!catalog.register_node_group(ns, "G0").await.unwrap());
        assert!(catalog.register_node_group(ns, "G1").await.unwrap());
        assert!(catalog.register_node_group(ns, "G2").await.unwrap());
        assert!(!catalog.register_node_group(ns, "G2").await.unwrap());
        assert!(catalog.register_node_group(ns, "bad/group").await.is_err());
        assert!(catalog.remove_node_group(ns, "G0").await.is_err());
        let groups = catalog.list_node_groups(ns, None, 2).await.unwrap();
        assert_eq!(groups.items, vec!["G0", "G1"]);
        assert_eq!(
            catalog
                .list_node_groups(ns, groups.next.as_deref(), 2)
                .await
                .unwrap()
                .items,
            vec!["G2"]
        );
        assert!(
            catalog
                .list_node_groups(other, groups.next.as_deref(), 1)
                .await
                .is_err()
        );
        assert!(catalog.remove_node_group(ns, "G1").await.unwrap());
        assert!(!catalog.remove_node_group(ns, "G1").await.unwrap());
        assert_eq!(
            catalog
                .register_physical_library(ns, "SERIAL_A")
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .register_physical_library(ns, "SERIAL_A")
                .await
                .unwrap(),
            ControlOutcome::AlreadyExists
        );
        assert_eq!(
            catalog
                .register_physical_library(ns, "SERIAL_B")
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog
                .register_physical_library(other, "SERIAL_A")
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert!(
            catalog
                .register_physical_library(ns, "bad/serial")
                .await
                .is_err()
        );
        let first = catalog.list_physical_libraries(ns, None, 1).await.unwrap();
        assert_eq!(first.items, vec!["SERIAL_A"]);
        let second = catalog
            .list_physical_libraries(ns, first.next.as_deref(), 1)
            .await
            .unwrap();
        assert_eq!(second.items, vec!["SERIAL_B"]);
        assert!(
            catalog
                .list_physical_libraries(other, first.next.as_deref(), 1)
                .await
                .is_err()
        );

        let p = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: "SERIAL_A".into(),
            name: "Archive".into(),
            file_limit: 200_000,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        assert!(
            catalog
                .create_pool(
                    ns,
                    &PoolRecord {
                        policy: PoolPolicy {
                            node_group: "G3".into(),
                            ..p.policy.clone()
                        },
                        ..p.clone()
                    }
                )
                .await
                .is_err()
        );
        assert!(
            catalog
                .create_pool(
                    ns,
                    &PoolRecord {
                        library_serial: "UNREGISTERED_LIBRARY".into(),
                        ..p.clone()
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(
            catalog.create_pool(ns, &p).await.unwrap(),
            ControlOutcome::Created
        );
        assert_eq!(
            catalog.create_pool(ns, &p).await.unwrap(),
            ControlOutcome::AlreadyExists
        );
        assert_eq!(
            catalog
                .create_pool(
                    ns,
                    &PoolRecord {
                        id: Uuid::new_v4(),
                        ..p.clone()
                    }
                )
                .await
                .unwrap(),
            ControlOutcome::NameConflict
        );
        assert_eq!(
            catalog
                .create_pool(
                    ns,
                    &PoolRecord {
                        name: "Other".into(),
                        ..p.clone()
                    }
                )
                .await
                .unwrap(),
            ControlOutcome::IdConflict
        );
        assert_eq!(
            catalog.create_pool(other, &p).await.unwrap(),
            ControlOutcome::Created
        );
        let reopened = ManagedCatalog::open(Some(cluster_file)).unwrap();
        assert_eq!(reopened.pool(ns, p.id).await.unwrap(), Some(p.clone()));
        assert_eq!(
            reopened.list_pools(other, None, 1).await.unwrap().items,
            vec![p.clone()]
        );

        let mut generated = Vec::new();
        for n in 0..103 {
            let item = PoolRecord {
                id: Uuid::new_v4(),
                library_serial: "SERIAL_A".into(),
                name: format!("P-{n}"),
                file_limit: 1,
                policy: PoolPolicy::default(),
                revision: 1,
            };
            assert_eq!(
                catalog.create_pool(ns, &item).await.unwrap(),
                ControlOutcome::Created
            );
            generated.push(item);
        }
        let mut seen = Vec::new();
        let mut cursor = None;
        loop {
            let page = reopened
                .list_pools(ns, cursor.as_deref(), 17)
                .await
                .unwrap();
            seen.extend(page.items);
            match page.next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        assert_eq!(seen.len(), 104);
        assert!(reopened.list_pools(ns, Some(b"wrong"), 1).await.is_err());
        assert!(reopened.list_pools(ns, None, 1001).await.is_err());
        let changed = PoolRecord {
            name: "Archive2".into(),
            file_limit: 300_000,
            policy: PoolPolicy {
                media_restriction: "L8".into(),
                mount_limit: 10,
                node_group: "G2".into(),
                low_space_warning_threshold: 12,
                ..PoolPolicy::default()
            },
            revision: 2,
            ..p.clone()
        };
        assert_eq!(
            catalog
                .update_pool(
                    ns,
                    &p,
                    &PoolRecord {
                        name: generated[0].name.clone(),
                        ..changed.clone()
                    },
                )
                .await
                .unwrap(),
            ControlOutcome::NameConflict
        );
        assert_eq!(reopened.pool(ns, p.id).await.unwrap(), Some(p.clone()));
        assert_eq!(
            catalog.update_pool(ns, &p, &changed).await.unwrap(),
            ControlOutcome::Updated
        );
        assert_eq!(
            reopened.update_pool(ns, &p, &changed).await.unwrap(),
            ControlOutcome::AlreadyExists
        );
        assert_eq!(
            reopened
                .update_pool(
                    ns,
                    &p,
                    &PoolRecord {
                        file_limit: 400_000,
                        ..changed.clone()
                    },
                )
                .await
                .unwrap(),
            ControlOutcome::StaleVersion
        );
        assert!(reopened.update_pool(ns, &changed, &changed).await.is_err());
        assert_eq!(
            reopened.pool(ns, p.id).await.unwrap(),
            Some(changed.clone())
        );
        assert!(catalog.remove_node_group(ns, "G2").await.is_err());
        assert_eq!(
            catalog
                .create_pool(
                    ns,
                    &PoolRecord {
                        id: Uuid::new_v4(),
                        ..p.clone()
                    },
                )
                .await
                .unwrap(),
            ControlOutcome::Created
        );
        assert!(
            catalog
                .update_pool(
                    ns,
                    &changed,
                    &PoolRecord {
                        id: Uuid::new_v4(),
                        ..changed.clone()
                    },
                )
                .await
                .is_err()
        );
        let original = generated[1].clone();
        let away = PoolRecord {
            file_limit: 2,
            revision: 2,
            ..original.clone()
        };
        let back = PoolRecord {
            file_limit: 1,
            revision: 3,
            ..original.clone()
        };
        assert_eq!(
            catalog.update_pool(ns, &original, &away).await.unwrap(),
            ControlOutcome::Updated
        );
        assert_eq!(
            catalog.update_pool(ns, &away, &back).await.unwrap(),
            ControlOutcome::Updated
        );
        assert_eq!(
            reopened
                .update_pool(
                    ns,
                    &original,
                    &PoolRecord {
                        file_limit: 3,
                        revision: 2,
                        ..original.clone()
                    },
                )
                .await
                .unwrap(),
            ControlOutcome::StaleVersion
        );
        managed_placement_exercise(catalog, cluster_file).await;
        managed_import_exercise(catalog).await;
    }
}
