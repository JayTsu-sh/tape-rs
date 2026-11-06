//! Stable logical requests, separate from a volume-specific physical attempt.
//! Inert bytes and a matching baseline do not grant admission or media access.
use super::*;
use crate::ltfs::index::MetadataChange;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

const DOMAIN: &[u8] = b"tape-rs/node-request/1";
const BASELINE_DOMAIN: &[u8] = b"tape-rs/node-request-baseline/1";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationRequestContext {
    pub library_serial: String,
    pub namespace: Uuid,
    pub pool: Uuid,
    pub operation: Uuid,
    pub request_epoch: u64,
    pub path: String,
    pub parent: Uuid,
    pub parent_version: u64,
    pub node: Uuid,
    pub expected_version: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    library: String,
    namespace: String,
    pool: String,
    operation: String,
    request_epoch: u64,
    path: String,
    parent: String,
    parent_version: u64,
    node: String,
    expected_version: Option<u64>,
    baseline_sha256: [u8; 32],
    time: String,
    mutation: NodeMutation,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMutationRequest {
    context: NodeMutationRequestContext,
    wire: Wire,
}

pub(super) fn resolved_value(value: Option<&ResolvedPath>) -> serde_json::Value {
    match value {
        None => serde_json::Value::Null,
        Some(ResolvedPath::Directory {
            id,
            version,
            attributes,
        }) => json!({
            "kind":"directory","id":id.to_string(),"version":version,
            "attributes":attributes.as_ref().map(NodeAttributes::digest),
        }),
        Some(ResolvedPath::File {
            id,
            head,
            copy,
            placement,
            attributes,
            ..
        }) => json!({
            "kind":"file","id":id.to_string(),
            "head":{"version":head.version,"size":head.size,"copy":head.copy_id.to_string()},
            "copy":{"version":copy.version,"origin":copy.origin.bytes(),
                "volume":copy.volume_id.to_string(),"generation":copy.volume_generation,
                "scan":copy.volume_scan_id.to_string(),"uid":copy.ltfs_file_uid,"size":copy.size,
                "digest_kind":copy.content_digest.tag(),"digest":copy.content_digest.payload(),
                "media":copy.media.as_ref().map(|m|json!({"version":m.version,
                    "logical_uid":m.logical_file_uid,"path":m.physical_path,
                    "ee_alias":m.ee_alias,"native_symlink":m.native_symlink}))},
            "placement":{"namespace":placement.namespace.to_string(),
                "library":placement.library_serial,"pool":placement.pool_id.to_string(),
                "barcode":placement.barcode},
            "attributes":attributes.as_ref().map(NodeAttributes::digest),
        }),
    }
}

fn baseline_digest(value: &NodeMutationBaseline) -> [u8; 32] {
    let current = resolved_value(value.current.as_ref());
    let bytes = json!({"version":1,"library":value.library_serial,
        "namespace":value.namespace.to_string(),"pool":value.pool.to_string(),
        "operation":value.operation.to_string(),"epoch":value.request_epoch,"path":value.path,
        "parent":value.parent.id.to_string(),"parent_version":value.parent.version,
        "parent_attributes":value.parent.attributes.as_ref().map(NodeAttributes::digest),
        "node":value.node.to_string(),"current":current})
    .to_string();
    Sha256::new()
        .chain_update(BASELINE_DOMAIN)
        .chain_update(bytes.as_bytes())
        .finalize()
        .into()
}

impl NodeMutationRequest {
    fn from_baseline(
        value: &NodeMutationBaseline,
        spec: &NodeMutationSpec,
        time: &str,
    ) -> Result<Self> {
        node_baseline::validate_spec(spec)?;
        if value.pool != spec.pool || value.operation != spec.operation || value.path != spec.path {
            return Err(CatalogError::Invalid("节点请求与逻辑预检作用域不符"));
        }
        let node = node_baseline::target_id(spec, value.current.as_ref())?.unwrap_or_else(|| {
            node_baseline::new_inode(value.namespace, value.request_epoch, value.operation)
        });
        if node != value.node {
            return Err(CatalogError::Invalid("节点请求与逻辑目标身份不符"));
        }
        Self::from_wire(Wire {
            version: 1,
            library: value.library_serial.clone(),
            namespace: value.namespace.to_string(),
            pool: value.pool.to_string(),
            operation: value.operation.to_string(),
            request_epoch: value.request_epoch,
            path: value.path.clone(),
            parent: value.parent.id.to_string(),
            parent_version: value.parent.version,
            node: value.node.to_string(),
            expected_version: spec.expected_version,
            baseline_sha256: baseline_digest(value),
            time: time.into(),
            mutation: spec.mutation.clone(),
        })
    }

    fn from_wire(wire: Wire) -> Result<Self> {
        let context = NodeMutationRequestContext {
            library_serial: wire.library.clone(),
            namespace: node_mutation::id(&wire.namespace)?,
            pool: node_mutation::id(&wire.pool)?,
            operation: node_mutation::id(&wire.operation)?,
            request_epoch: wire.request_epoch,
            path: wire.path.clone(),
            parent: node_mutation::id(&wire.parent)?,
            parent_version: wire.parent_version,
            node: node_mutation::id(&wire.node)?,
            expected_version: wire.expected_version,
        };
        control::validate_physical_serial(&context.library_serial)?;
        let request = Self { context, wire };
        node_baseline::validate_spec(&request.spec())?;
        let c = &request.context;
        if request.wire.version != 1
            || c.request_epoch == 0
            || c.parent_version == 0
            || c.node == c.parent
            || (c.parent_version == u64::MAX && request.mutation().changes_parent_attributes())
            || (c.expected_version.is_none()
                && c.node != node_baseline::new_inode(c.namespace, c.request_epoch, c.operation))
        {
            return Err(CatalogError::Invalid("节点请求版本、目标或父目录身份非法"));
        }
        MetadataChange {
            modify_time: Some(request.wire.time.clone()),
            ..Default::default()
        }
        .validate()
        .map_err(|_| CatalogError::Invalid("节点请求时间不是规范 LTFS 时间"))?;
        Ok(request)
    }

    pub fn context(&self) -> &NodeMutationRequestContext {
        &self.context
    }
    pub fn mutation(&self) -> &NodeMutation {
        &self.wire.mutation
    }
    pub fn time(&self) -> &str {
        &self.wire.time
    }
    pub fn spec(&self) -> NodeMutationSpec {
        NodeMutationSpec {
            pool: self.context.pool,
            operation: self.context.operation,
            path: self.context.path.clone(),
            expected_version: self.context.expected_version,
            mutation: self.wire.mutation.clone(),
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        node_mutation::canonical(&self.wire)
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        Sha256::new()
            .chain_update(DOMAIN)
            .chain_update(self.encode().expect("validated logical request"))
            .finalize()
            .into()
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let wire: Wire =
            serde_json::from_slice(bytes).map_err(|_| CatalogError::Invalid("节点请求格式非法"))?;
        let request = Self::from_wire(wire)?;
        if request.encode()? != bytes {
            return Err(CatalogError::Invalid("节点请求须使用规范 JSON"));
        }
        Ok(request)
    }
    pub(in crate::managed_catalog) fn matches_baseline(
        &self,
        value: &NodeMutationBaseline,
    ) -> Result<bool> {
        Ok(Self::from_baseline(value, &self.spec(), self.time())? == *self)
    }
}

/// Recompute within the caller's transaction, using the saved retained epoch.
/// A future admission transaction must install its persistent holds afterward.
pub(super) async fn revalidate_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    request: &NodeMutationRequest,
) -> Result<NodeMutationBaseline> {
    let c = request.context();
    let value = node_baseline::baseline_for_epoch_at(
        tr,
        &c.library_serial,
        route,
        owner,
        &request.spec(),
        Some(c.request_epoch),
    )
    .await?;
    if !request.matches_baseline(&value)? {
        return Err(CatalogError::Invalid("节点请求的逻辑前提已改变"));
    }
    Ok(value)
}

impl ManagedCatalog {
    pub async fn prepare_node_mutation_request(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        spec: &NodeMutationSpec,
        time: &str,
    ) -> Result<NodeMutationRequest> {
        let baseline = self
            .prepare_node_mutation_baseline(serial, route, owner, spec)
            .await?;
        NodeMutationRequest::from_baseline(&baseline, spec, time)
    }
    /// Validation only. It neither admits work nor preserves this precondition
    /// after the transaction ends. The original request bytes remain unchanged.
    pub async fn check_node_mutation_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &NodeMutationRequest,
    ) -> Result<NodeMutationBaseline> {
        revalidate_at(&self.db.create_trx()?, route, owner, request).await
    }
    /// Store opaque request bytes using the existing bounded chunk store.
    /// The request fingerprint has its own domain, distinct from object hashes.
    pub async fn upload_node_mutation_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &NodeMutationRequest,
        max_bytes: usize,
    ) -> Result<NodePlanObject> {
        let bytes = request.encode()?;
        let object = NodePlanObject::for_bytes(
            &request.context.library_serial,
            request.context.namespace,
            &bytes,
            max_bytes,
        )?;
        self.upload_node_plan_object(route, owner, &object, &mut bytes.as_slice())
            .await?;
        Ok(object)
    }
    pub async fn load_node_mutation_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        object: &NodePlanObject,
        max_bytes: usize,
    ) -> Result<NodeMutationRequest> {
        let mut bytes = Vec::new();
        self.node_plan_object_to(route, owner, object, max_bytes, &mut bytes)
            .await?;
        let request = NodeMutationRequest::decode(&bytes)?;
        if request.context.library_serial != object.library()
            || request.context.namespace != object.namespace()
        {
            return Err(CatalogError::Corrupt("节点请求内容与对象作用域不符"));
        }
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIME: &str = "2026-10-31T08:00:00.000000000Z";

    fn fixture() -> (NodeMutationBaseline, NodeMutationSpec) {
        let namespace = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let spec = NodeMutationSpec {
            pool: Uuid::new_v4(),
            operation,
            path: "/child".into(),
            expected_version: None,
            mutation: NodeMutation::CreateDirectory,
        };
        let baseline = NodeMutationBaseline {
            library_serial: "LOGICAL-REQUEST".into(),
            namespace,
            pool: spec.pool,
            operation,
            owner_epoch: 100,
            request_epoch: 1,
            path: spec.path.clone(),
            parent: NodeMutationParent {
                id: Uuid::new_v4(),
                version: 7,
                attributes: None,
            },
            node: node_baseline::new_inode(namespace, 1, operation),
            current: None,
        };
        (baseline, spec)
    }

    #[test]
    fn logical_request_codec_is_canonical_strict_and_owner_independent() {
        let (baseline, spec) = fixture();
        let request = NodeMutationRequest::from_baseline(&baseline, &spec, TIME).unwrap();
        let bytes = request.encode().unwrap();
        assert_eq!(NodeMutationRequest::decode(&bytes).unwrap(), request);
        for cut in 0..bytes.len() {
            assert!(NodeMutationRequest::decode(&bytes[..cut]).is_err());
        }
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        for field in value.as_object().unwrap().keys() {
            let mut bad = value.clone();
            bad.as_object_mut().unwrap().remove(field);
            assert!(
                NodeMutationRequest::decode(bad.to_string().as_bytes()).is_err(),
                "missing {field}"
            );
        }
        for (field, replacement) in [
            ("version", json!(0)),
            ("version", json!(2)),
            ("request_epoch", json!(0)),
            ("parent_version", json!(0)),
            ("parent_version", json!(u64::MAX)),
            ("expected_version", json!(0)),
            ("expected_version", json!(1)),
            ("expected_version", json!(u64::MAX)),
            ("namespace", json!(Uuid::nil().to_string())),
            ("pool", json!(Uuid::nil().to_string())),
            ("operation", json!(Uuid::nil().to_string())),
            ("parent", json!(Uuid::nil().to_string())),
            ("node", json!(baseline.parent.id.to_string())),
            ("node", json!(Uuid::new_v4().to_string())),
            ("path", json!("/a/../child")),
            ("time", json!("2026-10-31T08:00:00Z")),
            ("baseline_sha256", json!([0, 1])),
            ("extra", json!(true)),
        ] {
            let mut bad = value.clone();
            bad[field] = replacement;
            assert!(
                NodeMutationRequest::decode(bad.to_string().as_bytes()).is_err(),
                "bad {field}"
            );
        }
        let mut padded = bytes.clone();
        padded.push(b' ');
        assert!(NodeMutationRequest::decode(&padded).is_err());
        assert!(NodeMutationRequest::decode(&serde_json::to_vec_pretty(&value).unwrap()).is_err());
        assert!(NodeMutationPlan::decode(&bytes).is_err());
        let object = NodePlanObject::for_bytes(
            &baseline.library_serial,
            baseline.namespace,
            &bytes,
            bytes.len(),
        )
        .unwrap();
        assert_ne!(object.digest(), request.fingerprint());
        let mut adopted = baseline.clone();
        adopted.owner_epoch = 101;
        assert!(request.matches_baseline(&adopted).unwrap());
        let mut metadata_changed = adopted.clone();
        metadata_changed.parent.attributes = Some(
            NodeAttributes::new(
                crate::ltfs::index::NodeMeta {
                    file_uid: 7,
                    readonly: true,
                    creation_time: TIME.into(),
                    change_time: TIME.into(),
                    modify_time: TIME.into(),
                    access_time: TIME.into(),
                    backup_time: TIME.into(),
                },
                vec![crate::ltfs::index::Xattr {
                    key: "unknown".into(),
                    value: "preserve".into(),
                    base64: false,
                }],
                None,
            )
            .unwrap(),
        );
        assert!(!request.matches_baseline(&metadata_changed).unwrap());
        assert_eq!(
            NodeMutationRequest::from_baseline(&adopted, &spec, TIME)
                .unwrap()
                .encode()
                .unwrap(),
            bytes
        );
        adopted.parent.version += 1;
        assert!(!request.matches_baseline(&adopted).unwrap());
        let later =
            NodeMutationRequest::from_baseline(&baseline, &spec, "2026-10-31T08:00:01.000000000Z")
                .unwrap();
        assert_ne!(later.fingerprint(), request.fingerprint());
        assert_eq!(request.time(), TIME);
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；逻辑请求分块保存、保留代次和接任，不访问介质"]
    async fn logical_request_keeps_its_retained_epoch_time_and_payload_across_takeover() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("SIM-NODE-REQUEST-{ns}");
        let challenge = Uuid::new_v4();
        let replies=(1..=3).map(|node|json!({"version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":node,"term":7,"leader":2,"role":if node==2 {"Leader"}else{"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let route = catalog
            .initialize_native_library(
                ns,
                &verify_native_barriers(&serial, challenge, &replies).unwrap(),
            )
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 100)
            .await
            .unwrap();
        let pool = PoolRecord {
            id: Uuid::new_v4(),
            library_serial: serial.clone(),
            name: "requests".into(),
            file_limit: 200_000,
            created_at_millis: 1_798_000_000_000,
            policy: PoolPolicy::default(),
            revision: 1,
        };
        catalog
            .create_active_pool(&serial, route, 100, &pool)
            .await
            .unwrap();
        let spec = NodeMutationSpec {
            pool: pool.id,
            operation: Uuid::new_v4(),
            path: "/link".into(),
            expected_version: None,
            mutation: NodeMutation::CreateSymlink {
                target: "large-target".repeat(20_000),
            },
        };
        let request = catalog
            .prepare_node_mutation_request(&serial, route, 100, &spec, TIME)
            .await
            .unwrap();
        let original = request.encode().unwrap();
        let fingerprint = request.fingerprint();
        catalog
            .check_node_mutation_request(route, 100, &request)
            .await
            .unwrap();
        let object = catalog
            .upload_node_mutation_request(route, 100, &request, 1 << 20)
            .await
            .unwrap();
        assert!(object.chunks() > 3);
        assert_ne!(object.digest(), fingerprint);
        assert_eq!(
            catalog
                .upload_node_mutation_request(route, 100, &request, 1 << 20)
                .await
                .unwrap(),
            object
        );
        assert_eq!(
            catalog
                .load_node_mutation_request(route, 100, &object, 1 << 20)
                .await
                .unwrap(),
            request
        );
        assert!(
            catalog
                .load_node_mutation_plan(route, 100, &object, 1 << 20)
                .await
                .is_err()
        );
        assert!(
            catalog
                .load_node_mutation_request(route, 100, &object, 1)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &management::prefix(ns, b'w'),
            &requests::encode_window(RequestWindow {
                first_retained: 1,
                accepting: 2,
            }),
        );
        tr.set(
            &requests::window_key(ns),
            &requests::encode_window(RequestWindow {
                first_retained: 1,
                accepting: 9,
            }),
        );
        tr.commit().await.unwrap();
        // The saved epoch is still retained. Refreshing from accepting would
        // invent a new inode and break the original operation's identity.
        let retained = catalog
            .check_node_mutation_request(route, 100, &request)
            .await
            .unwrap();
        assert_eq!(retained.request_epoch, 1);
        assert_eq!(retained.node, request.context().node);
        let fresh = catalog
            .prepare_node_mutation_request(&serial, route, 100, &spec, TIME)
            .await
            .unwrap();
        assert_eq!(fresh.context().request_epoch, 2);
        assert_ne!(fresh.context().node, request.context().node);
        assert_ne!(fresh.fingerprint(), fingerprint);
        catalog
            .claim_active_owner_epoch(&serial, 101)
            .await
            .unwrap();
        assert!(
            catalog
                .check_node_mutation_request(route, 100, &request)
                .await
                .is_err()
        );
        assert!(
            catalog
                .load_node_mutation_request(route, 100, &object, 1 << 20)
                .await
                .is_err()
        );
        assert!(
            catalog
                .upload_node_mutation_request(route, 100, &request, 1 << 20)
                .await
                .is_err()
        );
        let adopted = catalog
            .load_node_mutation_request(route, 101, &object, 1 << 20)
            .await
            .unwrap();
        assert_eq!(adopted.encode().unwrap(), original);
        assert_eq!(adopted.time(), TIME);
        assert_eq!(adopted.fingerprint(), fingerprint);
        catalog
            .check_node_mutation_request(route, 101, &adopted)
            .await
            .unwrap();
        let reopened = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert_eq!(
            reopened
                .load_node_mutation_request(route, 101, &object, 1 << 20)
                .await
                .unwrap(),
            request
        );
        let mut wrong: serde_json::Value = serde_json::from_slice(&original).unwrap();
        wrong["namespace"] = json!(Uuid::new_v4().to_string());
        wrong["node"] = json!(
            node_baseline::new_inode(
                Uuid::parse_str(wrong["namespace"].as_str().unwrap()).unwrap(),
                1,
                spec.operation
            )
            .to_string()
        );
        let wrong = wrong.to_string().into_bytes();
        NodeMutationRequest::decode(&wrong).unwrap();
        let spoof = NodePlanObject::for_bytes(&serial, ns, &wrong, 1 << 20).unwrap();
        catalog
            .upload_node_plan_object(route, 101, &spoof, &mut wrong.as_slice())
            .await
            .unwrap();
        assert!(matches!(
            catalog
                .load_node_mutation_request(route, 101, &spoof, 1 << 20)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let root = active_pools::root_id(ns, pool.id);
        let tr = catalog.db.create_trx().unwrap();
        let raw = tr.get(&node_key(ns, root), false).await.unwrap().unwrap();
        let mut changed = decode_node(&raw).unwrap();
        changed.directory_version = Some(2);
        tr.set(&node_key(ns, root), &encode_node(&changed));
        tr.commit().await.unwrap();
        assert!(
            catalog
                .check_node_mutation_request(route, 101, &request)
                .await
                .is_err()
        );
        // Inert storage remains readable even when its precondition is stale.
        assert_eq!(
            catalog
                .load_node_mutation_request(route, 101, &object, 1 << 20)
                .await
                .unwrap()
                .encode()
                .unwrap(),
            original
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&node_key(ns, root), &raw);
        tr.set(
            &management::prefix(ns, b'w'),
            &requests::encode_window(RequestWindow {
                first_retained: 2,
                accepting: 2,
            }),
        );
        tr.commit().await.unwrap();
        assert!(
            catalog
                .check_node_mutation_request(route, 101, &request)
                .await
                .is_err()
        );
        assert!(
            !requests::has_namespace_holds(&catalog.db.create_trx().unwrap(), ns)
                .await
                .unwrap()
        );
        assert!(
            catalog
                .db
                .create_trx()
                .unwrap()
                .get(&node_key(ns, request.context().node), false)
                .await
                .unwrap()
                .is_none()
        );
    }
}
