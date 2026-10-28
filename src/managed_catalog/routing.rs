//! Prepared library-to-namespace mappings. These records never authorize
//! serving or writing; runtime authority needs a separate cutover barrier.

use super::*;
use sha2::{Digest, Sha256};

const CUTOVER_CERTIFICATE_LEN: usize = 313;
const ACTIVE_ROUTE_LEN: usize = 73;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedLibraryRoute {
    pub namespace: Uuid,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrepareRouteOutcome {
    Prepared(PreparedLibraryRoute),
    Unchanged(PreparedLibraryRoute),
    Conflict(Option<PreparedLibraryRoute>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgetRouteOutcome {
    Removed,
    AlreadyAbsent,
    RevisionConflict(PreparedLibraryRoute),
}

/// Three node exports bound to the sealed import and prepared route. This is
/// not the active metadata authority until ltfsd's runtime barrier consumes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutoverCertificate {
    pub route: PreparedLibraryRoute,
    pub frozen_index: u64,
    pub selected_node: u64,
    pub catalog_sha256: [u8; 32],
    pub imported_snapshot_sha256: [u8; 32],
    pub manifest_sha256: [u8; 32],
    pub sealed_projection_sha256: [u8; 32],
    pub nodes: [CutoverNode; 3],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CutoverNode {
    pub node_id: u64,
    pub applied_index: u64,
    pub snapshot_sha256: [u8; 32],
}

/// FDB's per-library authority pointer. The runtime must still verify the
/// old Raft authority is frozen before calling the explicit activation API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActiveLibraryRoute {
    pub route: PreparedLibraryRoute,
    pub frozen_index: u64,
    pub certificate_sha256: [u8; 32],
    pub initial_owner_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActiveOwnerOutcome {
    Advanced,
    AlreadyCurrent,
    Stale,
}

fn active_key(serial: &str) -> Vec<u8> {
    let mut out = key(b'A');
    append_component(&mut out, serial.as_bytes());
    out
}

fn encode_active(value: ActiveLibraryRoute) -> [u8; ACTIVE_ROUTE_LEN] {
    let mut out = [0; ACTIVE_ROUTE_LEN];
    out[0] = 1;
    out[1..17].copy_from_slice(value.route.namespace.as_bytes());
    out[17..25].copy_from_slice(&value.route.revision.to_be_bytes());
    out[25..33].copy_from_slice(&value.frozen_index.to_be_bytes());
    out[33..65].copy_from_slice(&value.certificate_sha256);
    out[65..73].copy_from_slice(&value.initial_owner_epoch.to_be_bytes());
    out
}

fn decode_active(bytes: &[u8]) -> Result<ActiveLibraryRoute> {
    if bytes.len() != ACTIVE_ROUTE_LEN || bytes[0] != 1 {
        return Err(CatalogError::Corrupt("活动带库路由格式"));
    }
    let mut certificate_sha256 = [0; 32];
    certificate_sha256.copy_from_slice(&bytes[33..65]);
    let value = ActiveLibraryRoute {
        route: PreparedLibraryRoute {
            namespace: uuid_from(&bytes[1..17])?,
            revision: read_u64(&bytes[17..25]),
        },
        frozen_index: read_u64(&bytes[25..33]),
        certificate_sha256,
        initial_owner_epoch: read_u64(&bytes[65..73]),
    };
    if value.route.namespace.is_nil()
        || value.route.revision == 0
        || value.frozen_index == 0
        || value.certificate_sha256 == [0; 32]
        || value.initial_owner_epoch == 0
    {
        return Err(CatalogError::Corrupt("活动带库路由内容"));
    }
    Ok(value)
}

fn certificate_key(serial: &str) -> Vec<u8> {
    let mut out = key(b'x');
    append_component(&mut out, serial.as_bytes());
    out
}

fn source_certificate(
    route: PreparedLibraryRoute,
    selected_node: u64,
    evidence: [VerifiedNodeExport; 3],
) -> Result<CutoverCertificate> {
    let mut evidence = evidence;
    evidence.sort_by_key(|node| node.node_id);
    let common = evidence[0].identity;
    if route.namespace.is_nil()
        || route.revision == 0
        || selected_node == 0
        || evidence[0].node_id == 0
        || evidence
            .windows(2)
            .any(|pair| pair[0].node_id == pair[1].node_id)
        || evidence.iter().any(|node| {
            node.identity.frozen_index != common.frozen_index
                || node.identity.catalog_sha256 != common.catalog_sha256
                || node.identity.row_counts != common.row_counts
                || node.identity.applied_index < common.frozen_index
        })
    {
        return Err(CatalogError::Invalid("三个节点的冻结目录来源不一致"));
    }
    let selected = evidence
        .iter()
        .find(|node| node.node_id == selected_node)
        .ok_or(CatalogError::Invalid("导入快照来源节点不在三节点证据中"))?;
    Ok(CutoverCertificate {
        route,
        frozen_index: common.frozen_index,
        selected_node,
        catalog_sha256: common.catalog_sha256,
        imported_snapshot_sha256: selected.identity.snapshot_sha256,
        manifest_sha256: [0; 32],
        sealed_projection_sha256: [0; 32],
        nodes: evidence.map(|node| CutoverNode {
            node_id: node.node_id,
            applied_index: node.identity.applied_index,
            snapshot_sha256: node.identity.snapshot_sha256,
        }),
    })
}

fn encode_certificate(value: CutoverCertificate) -> [u8; CUTOVER_CERTIFICATE_LEN] {
    let mut out = [0; CUTOVER_CERTIFICATE_LEN];
    out[0] = 1;
    out[1..17].copy_from_slice(value.route.namespace.as_bytes());
    out[17..25].copy_from_slice(&value.route.revision.to_be_bytes());
    out[25..33].copy_from_slice(&value.frozen_index.to_be_bytes());
    out[33..41].copy_from_slice(&value.selected_node.to_be_bytes());
    out[41..73].copy_from_slice(&value.catalog_sha256);
    out[73..105].copy_from_slice(&value.imported_snapshot_sha256);
    out[105..137].copy_from_slice(&value.manifest_sha256);
    out[137..169].copy_from_slice(&value.sealed_projection_sha256);
    for (index, node) in value.nodes.into_iter().enumerate() {
        let at = 169 + index * 48;
        out[at..at + 8].copy_from_slice(&node.node_id.to_be_bytes());
        out[at + 8..at + 16].copy_from_slice(&node.applied_index.to_be_bytes());
        out[at + 16..at + 48].copy_from_slice(&node.snapshot_sha256);
    }
    out
}

fn decode_certificate(bytes: &[u8]) -> Result<CutoverCertificate> {
    if bytes.len() != CUTOVER_CERTIFICATE_LEN || bytes[0] != 1 {
        return Err(CatalogError::Corrupt("带库切换来源证明格式"));
    }
    let mut catalog_sha256 = [0; 32];
    catalog_sha256.copy_from_slice(&bytes[41..73]);
    let mut imported_snapshot_sha256 = [0; 32];
    imported_snapshot_sha256.copy_from_slice(&bytes[73..105]);
    let mut manifest_sha256 = [0; 32];
    manifest_sha256.copy_from_slice(&bytes[105..137]);
    let mut sealed_projection_sha256 = [0; 32];
    sealed_projection_sha256.copy_from_slice(&bytes[137..169]);
    let nodes = std::array::from_fn(|index| {
        let at = 169 + index * 48;
        let mut snapshot_sha256 = [0; 32];
        snapshot_sha256.copy_from_slice(&bytes[at + 16..at + 48]);
        CutoverNode {
            node_id: read_u64(&bytes[at..at + 8]),
            applied_index: read_u64(&bytes[at + 8..at + 16]),
            snapshot_sha256,
        }
    });
    let value = CutoverCertificate {
        route: PreparedLibraryRoute {
            namespace: uuid_from(&bytes[1..17])?,
            revision: read_u64(&bytes[17..25]),
        },
        frozen_index: read_u64(&bytes[25..33]),
        selected_node: read_u64(&bytes[33..41]),
        catalog_sha256,
        imported_snapshot_sha256,
        manifest_sha256,
        sealed_projection_sha256,
        nodes,
    };
    if value.route.namespace.is_nil()
        || value.route.revision == 0
        || value.frozen_index == 0
        || value.catalog_sha256 == [0; 32]
        || value.imported_snapshot_sha256 == [0; 32]
        || value.manifest_sha256 == [0; 32]
        || value.sealed_projection_sha256 == [0; 32]
        || value.nodes[0].node_id == 0
        || value
            .nodes
            .windows(2)
            .any(|pair| pair[0].node_id >= pair[1].node_id)
        || value
            .nodes
            .iter()
            .any(|node| node.applied_index < value.frozen_index || node.snapshot_sha256 == [0; 32])
        || !value.nodes.iter().any(|node| {
            node.node_id == value.selected_node
                && node.snapshot_sha256 == value.imported_snapshot_sha256
        })
    {
        return Err(CatalogError::Corrupt("带库切换来源证明内容"));
    }
    Ok(value)
}

fn route_key(serial: &str) -> Vec<u8> {
    let mut out = key(b'z');
    append_component(&mut out, serial.as_bytes());
    out
}

fn decode_route(value: &[u8]) -> Result<PreparedLibraryRoute> {
    if value.len() != 25 || value[0] != 1 {
        return Err(CatalogError::Corrupt("预备带库路由格式"));
    }
    let namespace = uuid_from(&value[1..17])?;
    let revision = read_u64(&value[17..]);
    if revision == 0 {
        return Err(CatalogError::Corrupt("预备带库路由身份"));
    }
    Ok(PreparedLibraryRoute {
        namespace,
        revision,
    })
}

fn encode_route(route: PreparedLibraryRoute) -> [u8; 25] {
    let mut value = [0; 25];
    value[0] = 1;
    value[1..17].copy_from_slice(route.namespace.as_bytes());
    value[17..].copy_from_slice(&route.revision.to_be_bytes());
    value
}

async fn sealed_context(
    tr: &Transaction,
    serial: &str,
    route: PreparedLibraryRoute,
) -> Result<(StagedImportSummary, [u8; 32], [u8; 32])> {
    let current = tr
        .get(&route_key(serial), false)
        .await?
        .as_deref()
        .map(decode_route)
        .transpose()?;
    if current != Some(route) {
        return Err(CatalogError::Incomplete);
    }
    import_materialize::require_sealed_ready(tr, route.namespace).await?;
    if tr
        .get(&control::library_key(route.namespace, serial), false)
        .await?
        .as_deref()
        != Some(&[][..])
    {
        return Err(CatalogError::Incomplete);
    }
    let manifest = tr
        .get(&staged_import::manifest_key(route.namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let staged = StagedImportSummary::decode(&manifest)?;
    if !staged.complete {
        return Err(CatalogError::Incomplete);
    }
    let seal = tr
        .get(
            &import_materialize::copy_verified_key(route.namespace),
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if seal.len() != 41 || seal[0] != 1 {
        return Err(CatalogError::Corrupt("导入封口证明格式"));
    }
    let mut sealed_projection_sha256 = [0; 32];
    sealed_projection_sha256.copy_from_slice(&seal[9..]);
    Ok((
        staged,
        Sha256::digest(&manifest).into(),
        sealed_projection_sha256,
    ))
}

async fn active_context(tr: &Transaction, serial: &str, active: ActiveLibraryRoute) -> Result<()> {
    let certificate = tr
        .get(&certificate_key(serial), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let decoded = decode_certificate(&certificate)?;
    if Sha256::digest(&certificate)[..] != active.certificate_sha256
        || decoded.route != active.route
        || decoded.frozen_index != active.frozen_index
        || tr
            .get(&route_key(serial), false)
            .await?
            .as_deref()
            .map(decode_route)
            .transpose()?
            != Some(active.route)
        || tr
            .get(&namespace_key(active.route.namespace), false)
            .await?
            .as_deref()
            != Some(&[NamespaceState::Active as u8][..])
        || tr
            .get(&control::managed_key(active.route.namespace), false)
            .await?
            .as_deref()
            != Some(&[][..])
        || tr
            .get(&control::library_key(active.route.namespace, serial), false)
            .await?
            .as_deref()
            != Some(&[][..])
    {
        return Err(CatalogError::Corrupt("活动带库路由与切换证明不一致"));
    }
    let manifest = tr
        .get(&staged_import::manifest_key(active.route.namespace), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let staged = StagedImportSummary::decode(&manifest)?;
    let seal = tr
        .get(
            &import_materialize::copy_verified_key(active.route.namespace),
            false,
        )
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let mut projection_key = key(b'q');
    projection_key.extend_from_slice(active.route.namespace.as_bytes());
    let projection = tr
        .get(&projection_key, false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let mut sealed_key = key(b'w');
    sealed_key.extend_from_slice(active.route.namespace.as_bytes());
    if !staged.complete
        || staged.audit.frozen_index != active.frozen_index
        || staged.audit.snapshot_sha256 != decoded.imported_snapshot_sha256
        || Sha256::digest(&manifest)[..] != decoded.manifest_sha256
        || seal.len() != 41
        || seal[0] != 1
        || seal[9..] != Sha256::digest(&projection)[..]
        || seal[9..] != decoded.sealed_projection_sha256
        || tr.get(&sealed_key, false).await?.as_deref() != Some(seal.as_ref())
    {
        return Err(CatalogError::Corrupt("活动带库路由的导入基线已失效"));
    }
    let epoch = tr
        .get(&owner_key(serial), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if epoch.len() != 8 || read_u64(&epoch) < active.initial_owner_epoch {
        return Err(CatalogError::Corrupt("活动带库 owner epoch 无效"));
    }
    Ok(())
}

/// Check the active authority and exact owner fence in the caller's write
/// transaction. Reading A and O non-snapshot makes a concurrent cutover or
/// takeover conflict with an admission that tries to commit under the old view.
pub(super) async fn require_active_owner(
    tr: &Transaction,
    namespace: Uuid,
    serial: &str,
    epoch: u64,
) -> Result<bool> {
    let saved = tr
        .get(&active_key(serial), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let active = decode_active(&saved)?;
    if active.route.namespace != namespace {
        return Err(CatalogError::Invalid("活动带库与请求命名空间不一致"));
    }
    active_context(tr, serial, active).await?;
    let owner = tr
        .get(&owner_key(serial), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if owner.len() != 8 {
        return Err(CatalogError::Corrupt("owner epoch 长度"));
    }
    Ok(read_u64(&owner) == epoch)
}

/// Resolve a runtime namespace from its physical library serial in the same
/// transaction as a subsequent metadata read. A missing or invalid authority
/// is an error, never an empty directory or a missing file.
pub(super) async fn require_active_namespace(tr: &Transaction, serial: &str) -> Result<Uuid> {
    require_expected_active_namespace(tr, serial, None).await
}

pub(super) async fn require_runtime_active_owner(
    tr: &Transaction,
    serial: &str,
    expected_route: ActiveLibraryRoute,
    owner_epoch: u64,
) -> Result<Uuid> {
    if owner_epoch == 0 {
        return Err(CatalogError::Invalid("活动 owner 轮次不能为零"));
    }
    let namespace = require_expected_active_namespace(tr, serial, Some(expected_route)).await?;
    let owner = tr
        .get(&owner_key(serial), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if owner.len() != 8 {
        return Err(CatalogError::Corrupt("活动 owner epoch 长度"));
    }
    if read_u64(&owner) != owner_epoch {
        return Err(CatalogError::OwnerFenced);
    }
    Ok(namespace)
}

pub(super) async fn require_expected_active_namespace(
    tr: &Transaction,
    serial: &str,
    expected: Option<ActiveLibraryRoute>,
) -> Result<Uuid> {
    let saved = tr
        .get(&active_key(serial), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    let active = decode_active(&saved)?;
    if expected.is_some_and(|expected| expected != active) {
        return Err(CatalogError::Incomplete);
    }
    active_context(tr, serial, active).await?;
    Ok(active.route.namespace)
}

impl ManagedCatalog {
    /// Advance an Active library's FDB fence after the caller has confirmed
    /// its Raft execution round and the drive PR. The route and owner row are
    /// read non-snapshot in this transaction so a concurrent takeover cannot
    /// commit an old epoch afterward.
    pub async fn claim_active_owner_epoch(
        &self,
        serial: &str,
        epoch: u64,
    ) -> Result<ActiveOwnerOutcome> {
        control::validate_physical_serial(serial)?;
        if epoch == 0 || epoch > 0xFFFF_FFFF_FFFF {
            return Err(CatalogError::Invalid("活动 owner 轮次超出 PR 键范围"));
        }
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            let saved = tr
                                .get(&active_key(&serial), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            let active = decode_active(&saved)?;
                            active_context(&tr, &serial, active).await?;
                            let key = owner_key(&serial);
                            let saved =
                                tr.get(&key, false).await?.ok_or(CatalogError::Incomplete)?;
                            if saved.len() != 8 {
                                return Err(CatalogError::Corrupt("活动 owner epoch 长度"));
                            }
                            let old = read_u64(&saved);
                            if epoch < old {
                                return Ok(ActiveOwnerOutcome::Stale);
                            }
                            if epoch == old {
                                return Ok(ActiveOwnerOutcome::AlreadyCurrent);
                            }
                            tr.set(&key, &epoch.to_be_bytes());
                            Ok(ActiveOwnerOutcome::Advanced)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Promote an already certified, sealed import in one FDB transaction.
    /// Callers must first establish that all old Raft writers are frozen;
    /// this API cannot observe or freeze the old authority by itself.
    pub async fn activate_prepared_cutover(
        &self,
        serial: &str,
        expected: CutoverCertificate,
    ) -> Result<ActiveLibraryRoute> {
        control::validate_physical_serial(serial)?;
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            if let Some(saved) = tr.get(&active_key(&serial), false).await? {
                                let active = decode_active(&saved)?;
                                active_context(&tr, &serial, active).await?;
                                if active.route == expected.route
                                    && active.frozen_index == expected.frozen_index
                                    && active.certificate_sha256
                                        == Sha256::digest(encode_certificate(expected))[..]
                                {
                                    return Ok(active);
                                }
                                return Err(CatalogError::Invalid("带库已由不同来源证明激活"));
                            }
                            let saved = tr
                                .get(&certificate_key(&serial), false)
                                .await?
                                .ok_or(CatalogError::Incomplete)?;
                            if decode_certificate(&saved)? != expected {
                                return Err(CatalogError::Invalid("带库切换来源证明已变化"));
                            }
                            let (staged, manifest_sha256, sealed_projection_sha256) =
                                sealed_context(&tr, &serial, expected.route).await?;
                            if staged.audit.frozen_index != expected.frozen_index
                                || staged.audit.snapshot_sha256 != expected.imported_snapshot_sha256
                                || manifest_sha256 != expected.manifest_sha256
                                || sealed_projection_sha256 != expected.sealed_projection_sha256
                            {
                                return Err(CatalogError::Corrupt("切换证明与已封口目录不一致"));
                            }
                            let owner = tr.get(&owner_key(&serial), false).await?;
                            let previous_epoch = match owner.as_deref() {
                                Some(bytes) if bytes.len() == 8 => read_u64(bytes),
                                Some(_) => return Err(CatalogError::Corrupt("owner epoch 长度")),
                                None => return Err(CatalogError::Incomplete),
                            };
                            let initial_owner_epoch = previous_epoch
                                .checked_add(1)
                                .ok_or(CatalogError::Corrupt("owner epoch 溢出"))?;
                            let active = ActiveLibraryRoute {
                                route: expected.route,
                                frozen_index: expected.frozen_index,
                                certificate_sha256: Sha256::digest(&saved).into(),
                                initial_owner_epoch,
                            };
                            tr.set(&owner_key(&serial), &initial_owner_epoch.to_be_bytes());
                            tr.set(
                                &namespace_key(expected.route.namespace),
                                &[NamespaceState::Active as u8],
                            );
                            tr.set(&active_key(&serial), &encode_active(active));
                            Ok(active)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Discovery fails closed if the route, certificate, import baseline, or
    /// owner epoch no longer matches the activation transaction.
    pub async fn active_library_route(&self, serial: &str) -> Result<Option<ActiveLibraryRoute>> {
        control::validate_physical_serial(serial)?;
        let tr = self.db.create_trx()?;
        let Some(saved) = tr.get(&active_key(serial), false).await? else {
            return Ok(None);
        };
        let active = decode_active(&saved)?;
        active_context(&tr, serial, active).await?;
        Ok(Some(active))
    }

    /// Read the active route and current owner fence from one FDB snapshot.
    /// Runtime owners use this to stop serving if another round supersedes
    /// them, even when their local Raft status has not caught up yet.
    pub async fn active_library_owner(
        &self,
        serial: &str,
    ) -> Result<Option<(ActiveLibraryRoute, u64)>> {
        control::validate_physical_serial(serial)?;
        let tr = self.db.create_trx()?;
        let Some(saved) = tr.get(&active_key(serial), false).await? else {
            return Ok(None);
        };
        let active = decode_active(&saved)?;
        active_context(&tr, serial, active).await?;
        let owner = tr
            .get(&owner_key(serial), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        if owner.len() != 8 {
            return Err(CatalogError::Corrupt("活动 owner epoch 长度"));
        }
        Ok(Some((active, read_u64(&owner))))
    }

    /// Bind three node exports to the prepared, sealed import in one FDB
    /// transaction. This certificate alone never enables runtime FDB routing.
    pub async fn certify_prepared_cutover(
        &self,
        serial: &str,
        route: PreparedLibraryRoute,
        selected_node: u64,
        evidence: [VerifiedNodeExport; 3],
    ) -> Result<bool> {
        control::validate_physical_serial(serial)?;
        let empty_exports = evidence
            .iter()
            .all(|proof| proof.identity().row_counts == [0; 4]);
        let source = source_certificate(route, selected_node, evidence)?;
        let key = certificate_key(serial);
        let serial = serial.to_owned();
        self.db
            .run(|tr, _| {
                let key = key.clone();
                let serial = serial.clone();
                async move {
                    requests::retry_result(
                        async {
                            if tr.get(&active_key(&serial), false).await?.is_some() {
                                return Err(CatalogError::Invalid("带库已激活，不能改写预备证明"));
                            }
                            let (staged, manifest_sha256, sealed_projection_sha256) =
                                sealed_context(&tr, &serial, source.route).await?;
                            if let ImportOrigin::ExternalMedia { library_sha256 } =
                                staged.audit.origin
                                && (!empty_exports
                                    || library_sha256[..] != Sha256::digest(serial.as_bytes())[..])
                            {
                                return Err(CatalogError::Invalid(
                                    "外部介质导入须绑定同库及三个真实空目录冻结导出",
                                ));
                            }
                            if staged.audit.frozen_index != source.frozen_index
                                || staged.audit.snapshot_sha256 != source.imported_snapshot_sha256
                            {
                                return Err(CatalogError::Invalid(
                                    "受管导入快照与三节点冻结来源不一致",
                                ));
                            }
                            let certificate = CutoverCertificate {
                                manifest_sha256,
                                sealed_projection_sha256,
                                ..source
                            };
                            let encoded = encode_certificate(certificate);
                            if let Some(saved) = tr.get(&key, false).await? {
                                let old = decode_certificate(&saved)?;
                                if old == certificate {
                                    return Ok(false);
                                }
                                if old.route.revision >= route.revision {
                                    return Err(CatalogError::Invalid(
                                        "当前预备路由已有不同的切换来源证明",
                                    ));
                                }
                            }
                            tr.set(&key, &encoded);
                            Ok(true)
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// A stale route revision, changed manifest, or broken q/s/w seal makes
    /// the certificate unusable. No caller may infer active authority here.
    pub async fn certified_prepared_cutover(
        &self,
        serial: &str,
    ) -> Result<Option<CutoverCertificate>> {
        control::validate_physical_serial(serial)?;
        let tr = self.db.create_trx()?;
        if tr.get(&active_key(serial), false).await?.is_some() {
            return Ok(None);
        }
        let Some(saved) = tr.get(&certificate_key(serial), false).await? else {
            return Ok(None);
        };
        let certificate = decode_certificate(&saved)?;
        let route = tr
            .get(&route_key(serial), false)
            .await?
            .as_deref()
            .map(decode_route)
            .transpose()?;
        if route != Some(certificate.route) {
            return Ok(None);
        }
        let (staged, manifest_sha256, sealed_projection_sha256) =
            sealed_context(&tr, serial, certificate.route).await?;
        if staged.audit.frozen_index != certificate.frozen_index
            || staged.audit.snapshot_sha256 != certificate.imported_snapshot_sha256
            || manifest_sha256 != certificate.manifest_sha256
            || sealed_projection_sha256 != certificate.sealed_projection_sha256
        {
            return Err(CatalogError::Corrupt("带库切换来源证明已失效"));
        }
        Ok(Some(certificate))
    }

    /// A discovery record only. It does not authorize ltfsd to stop reading
    /// Raft/SQLite, nor permit two metadata writers during a cutover.
    pub async fn prepared_library_route(
        &self,
        serial: &str,
    ) -> Result<Option<PreparedLibraryRoute>> {
        control::validate_physical_serial(serial)?;
        let tr = self.db.create_trx()?;
        if tr.get(&active_key(serial), false).await?.is_some() {
            return Ok(None);
        }
        let route = tr
            .get(&route_key(serial), false)
            .await?
            .as_deref()
            .map(decode_route)
            .transpose()
            .map(|route| route.filter(|route| !route.namespace.is_nil()))?;
        if let Some(route) = route {
            import_materialize::require_sealed_ready(&tr, route.namespace).await?;
            if tr
                .get(&control::library_key(route.namespace, serial), false)
                .await?
                .as_deref()
                != Some(&[][..])
            {
                return Err(CatalogError::Incomplete);
            }
        }
        Ok(route)
    }

    /// CAS a physical library's proposed sealed namespace. A library can
    /// prepare a new generation only after an exact revision read.
    pub async fn prepare_library_route(
        &self,
        serial: &str,
        namespace: Uuid,
        expected_revision: Option<u64>,
    ) -> Result<PrepareRouteOutcome> {
        control::validate_physical_serial(serial)?;
        if namespace.is_nil() || expected_revision == Some(0) {
            return Err(CatalogError::Invalid("预备带库路由身份或修订号无效"));
        }
        let key = route_key(serial);
        let library = control::library_key(namespace, serial);
        let active = active_key(serial);
        self.db
            .run(|tr, _| {
                let key = key.clone();
                let library = library.clone();
                let active = active.clone();
                async move {
                    requests::retry_result(
                        async {
                            if tr.get(&active, false).await?.is_some() {
                                return Err(CatalogError::Invalid("活动带库不能改写预备路由"));
                            }
                            import_materialize::require_sealed_ready(&tr, namespace).await?;
                            if tr.get(&library, false).await?.as_deref() != Some(&[][..]) {
                                return Err(CatalogError::Incomplete);
                            }
                            let current = tr
                                .get(&key, false)
                                .await?
                                .as_deref()
                                .map(decode_route)
                                .transpose()?;
                            if current.map(|route| route.revision) != expected_revision
                                && !(expected_revision.is_none()
                                    && current.is_some_and(|route| route.namespace.is_nil()))
                            {
                                if expected_revision.is_none()
                                    && let Some(route) = current
                                    && route.namespace == namespace
                                {
                                    return Ok(PrepareRouteOutcome::Unchanged(route));
                                }
                                return Ok(PrepareRouteOutcome::Conflict(
                                    current.filter(|route| !route.namespace.is_nil()),
                                ));
                            }
                            if let Some(route) = current
                                && route.namespace == namespace
                            {
                                return Ok(PrepareRouteOutcome::Unchanged(route));
                            }
                            let revision = current
                                .map_or(Some(1), |route| route.revision.checked_add(1))
                                .ok_or(CatalogError::Corrupt("预备带库路由修订号溢出"))?;
                            let route = PreparedLibraryRoute {
                                namespace,
                                revision,
                            };
                            tr.set(&key, &encode_route(route));
                            Ok(PrepareRouteOutcome::Prepared(route))
                        }
                        .await,
                    )
                }
            })
            .await?
    }

    /// Remove only a still-prepared record with the exact observed revision.
    pub async fn forget_prepared_library_route(
        &self,
        serial: &str,
        expected_revision: u64,
    ) -> Result<ForgetRouteOutcome> {
        control::validate_physical_serial(serial)?;
        if expected_revision == 0 {
            return Err(CatalogError::Invalid("预备带库路由修订号无效"));
        }
        let key = route_key(serial);
        let active = active_key(serial);
        self.db
            .run(|tr, _| {
                let key = key.clone();
                let active = active.clone();
                async move {
                    requests::retry_result(
                        async {
                            if tr.get(&active, false).await?.is_some() {
                                return Err(CatalogError::Invalid("活动带库不能撤销预备路由"));
                            }
                            let Some(current) = tr.get(&key, false).await? else {
                                return Ok(ForgetRouteOutcome::AlreadyAbsent);
                            };
                            let current = decode_route(&current)?;
                            if current.namespace.is_nil() {
                                return Ok(ForgetRouteOutcome::AlreadyAbsent);
                            }
                            if current.revision != expected_revision {
                                return Ok(ForgetRouteOutcome::RevisionConflict(current));
                            }
                            let revision = current
                                .revision
                                .checked_add(1)
                                .ok_or(CatalogError::Corrupt("预备带库路由修订号溢出"))?;
                            tr.set(
                                &key,
                                &encode_route(PreparedLibraryRoute {
                                    namespace: Uuid::nil(),
                                    revision,
                                }),
                            );
                            Ok(ForgetRouteOutcome::Removed)
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_route_encoding_rejects_corruption() {
        let value = ActiveLibraryRoute {
            route: PreparedLibraryRoute {
                namespace: Uuid::new_v4(),
                revision: 7,
            },
            frozen_index: 90,
            certificate_sha256: [17; 32],
            initial_owner_epoch: 11,
        };
        let encoded = encode_active(value);
        assert_eq!(decode_active(&encoded).unwrap(), value);
        assert!(decode_active(&encoded[..72]).is_err());
        let mut corrupt = encoded;
        corrupt[0] = 2;
        assert!(decode_active(&corrupt).is_err());
        corrupt = encoded;
        corrupt[65..73].fill(0);
        assert!(decode_active(&corrupt).is_err());
    }
}
