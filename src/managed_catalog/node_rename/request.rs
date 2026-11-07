//! Canonical original rename intent; execution owner is deliberately excluded.
use super::*;
use crate::ltfs::index::MetadataChange;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const LIMIT: usize = 8 << 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevisionWire {
    namespace: String,
    inode: String,
    version: u64,
}
impl From<NodeRenameRevision> for RevisionWire {
    fn from(value: NodeRenameRevision) -> Self {
        Self {
            namespace: value.namespace.to_string(),
            inode: value.inode.to_string(),
            version: value.version,
        }
    }
}
impl RevisionWire {
    fn revision(&self) -> Result<NodeRenameRevision> {
        Ok(NodeRenameRevision {
            namespace: node_mutation::id(&self.namespace)?,
            inode: node_mutation::id(&self.inode)?,
            version: self.version,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    library: String,
    pool: String,
    operation: String,
    request_epoch: u64,
    from: String,
    to: String,
    source: RevisionWire,
    source_parent: RevisionWire,
    destination_parent: RevisionWire,
    target: Option<RevisionWire>,
    no_replace: bool,
    time: String,
    baseline_sha256: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeRenameRequest {
    spec: NodeRenameSpec,
    wire: Wire,
}

fn parent_value(value: &NodeMutationParent) -> Value {
    json!({"inode":value.id.to_string(),"version":value.version,"attributes":value.attributes.as_ref().map(NodeAttributes::digest)})
}
fn object_metadata(value: Option<&ResolvedPath>) -> Value {
    match value {
        Some(ResolvedPath::File {
            object_metadata: Some(metadata),
            ..
        }) => json!({"modified":metadata.modified,"headers":metadata.headers}),
        _ => Value::Null,
    }
}
fn baseline_digest(value: &NodeRenameBaseline) -> [u8; 32] {
    let bytes=json!({"source_parent":parent_value(&value.source_parent),"destination_parent":parent_value(&value.destination_parent),
        "source":node_request::resolved_value(Some(&value.source)),"target":node_request::resolved_value(value.target.as_ref()),
        "source_object_metadata":object_metadata(Some(&value.source)),"target_object_metadata":object_metadata(value.target.as_ref())}).to_string();
    Sha256::new()
        .chain_update(b"tape-rs/node-rename-baseline/1")
        .chain_update(bytes.as_bytes())
        .finalize()
        .into()
}

impl NodeRenameRequest {
    pub(super) fn from_baseline(value: &NodeRenameBaseline, time: &str) -> Result<Self> {
        let spec = &value.spec;
        validate(spec)?;
        if revision(spec.source.namespace, &value.source) != spec.source
            || value
                .target
                .as_ref()
                .map(|node| revision(spec.source.namespace, node))
                != spec.target
            || value.source_parent.id != spec.source_parent.inode
            || value.source_parent.version != spec.source_parent.version
            || value.destination_parent.id != spec.destination_parent.inode
            || value.destination_parent.version != spec.destination_parent.version
        {
            return Err(invalid("rename 请求与原逻辑快照不符"));
        }
        Self::from_wire(Wire {
            version: 1,
            library: value.library_serial.clone(),
            pool: spec.pool.to_string(),
            operation: spec.operation.to_string(),
            request_epoch: value.request_epoch,
            from: spec.from.clone(),
            to: spec.to.clone(),
            source: spec.source.into(),
            source_parent: spec.source_parent.into(),
            destination_parent: spec.destination_parent.into(),
            target: spec.target.map(Into::into),
            no_replace: spec.no_replace,
            time: time.into(),
            baseline_sha256: baseline_digest(value),
        })
    }

    fn from_wire(wire: Wire) -> Result<Self> {
        control::validate_physical_serial(&wire.library)?;
        if wire.version != 1 || wire.request_epoch == 0 {
            return Err(invalid("rename 请求版本或代次无效"));
        }
        MetadataChange {
            modify_time: Some(wire.time.clone()),
            ..Default::default()
        }
        .validate()
        .map_err(|_| invalid("rename 请求时间不是规范 LTFS UTC 时间"))?;
        let spec = NodeRenameSpec {
            pool: node_mutation::id(&wire.pool)?,
            operation: node_mutation::id(&wire.operation)?,
            from: wire.from.clone(),
            to: wire.to.clone(),
            source: wire.source.revision()?,
            source_parent: wire.source_parent.revision()?,
            destination_parent: wire.destination_parent.revision()?,
            target: wire
                .target
                .as_ref()
                .map(RevisionWire::revision)
                .transpose()?,
            no_replace: wire.no_replace,
        };
        validate(&spec)?;
        let request = Self { spec, wire };
        request.encode()?;
        Ok(request)
    }
    pub fn spec(&self) -> &NodeRenameSpec {
        &self.spec
    }
    pub fn library(&self) -> &str {
        &self.wire.library
    }
    pub fn request_epoch(&self) -> u64 {
        self.wire.request_epoch
    }
    pub fn time(&self) -> &str {
        &self.wire.time
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        let bytes = node_mutation::canonical(&self.wire)?;
        if bytes.len() > LIMIT {
            return Err(invalid("rename 请求超过字节预算"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > LIMIT {
            return Err(invalid("rename 请求超过字节预算"));
        }
        let wire: Wire =
            serde_json::from_slice(bytes).map_err(|_| invalid("rename 请求格式无效"))?;
        let request = Self::from_wire(wire)?;
        if request.encode()? != bytes {
            return Err(invalid("rename 请求必须使用规范 JSON"));
        }
        Ok(request)
    }
    pub fn fingerprint(&self) -> [u8; 32] {
        Sha256::new()
            .chain_update(b"tape-rs/node-rename-request/1")
            .chain_update(self.encode().expect("validated rename request"))
            .finalize()
            .into()
    }
    pub(in crate::managed_catalog) fn matches_baseline(
        &self,
        value: &NodeRenameBaseline,
    ) -> Result<bool> {
        Ok(Self::from_baseline(value, self.time())? == *self)
    }
}

pub(in crate::managed_catalog::control) async fn revalidate_at(
    tr: &Transaction,
    route: ActiveLibraryRoute,
    owner: u64,
    request: &NodeRenameRequest,
) -> Result<NodeRenameBaseline> {
    let value = baseline_at(
        tr,
        request.library(),
        route,
        owner,
        request.spec(),
        Some(request.request_epoch()),
    )
    .await?;
    if !request.matches_baseline(&value)? {
        return Err(invalid("rename 原请求的逻辑前提已改变"));
    }
    Ok(value)
}

impl ManagedCatalog {
    pub async fn prepare_node_rename_request(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        spec: &NodeRenameSpec,
        time: &str,
    ) -> Result<NodeRenameRequest> {
        let value = self
            .prepare_node_rename_baseline(serial, route, owner, spec)
            .await?;
        NodeRenameRequest::from_baseline(&value, time)
    }
    /// Pure revalidation. A successful result grants no hold or media permission.
    pub async fn check_node_rename_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &NodeRenameRequest,
    ) -> Result<NodeRenameBaseline> {
        revalidate_at(&self.db.create_trx()?, route, owner, request).await
    }
    pub async fn upload_node_rename_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        request: &NodeRenameRequest,
        max_bytes: usize,
    ) -> Result<NodePlanObject> {
        let bytes = request.encode()?;
        let object = NodePlanObject::for_bytes(
            request.library(),
            request.spec.source.namespace,
            &bytes,
            max_bytes,
        )?;
        self.upload_node_plan_object(route, owner, &object, &mut bytes.as_slice())
            .await?;
        Ok(object)
    }
    pub async fn load_node_rename_request(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        object: &NodePlanObject,
        max_bytes: usize,
    ) -> Result<NodeRenameRequest> {
        let mut bytes = Vec::new();
        self.node_plan_object_to(route, owner, object, max_bytes.min(LIMIT), &mut bytes)
            .await?;
        let request = NodeRenameRequest::decode(&bytes)?;
        if request.library() != object.library()
            || request.spec.source.namespace != object.namespace()
        {
            return Err(CatalogError::Corrupt("rename 请求与保存对象作用域不符"));
        }
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::NodeMeta;
    const TIME: &str = "2026-11-02T10:00:00.000000000Z";

    fn fixture() -> NodeRenameBaseline {
        let namespace = Uuid::new_v4();
        let parent = NodeRenameRevision {
            namespace,
            inode: Uuid::new_v4(),
            version: 7,
        };
        let source = NodeRenameRevision {
            namespace,
            inode: Uuid::new_v4(),
            version: 3,
        };
        let attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 4,
                ..Default::default()
            },
            vec![],
            None,
        )
        .unwrap();
        let parent_attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 2,
                ..Default::default()
            },
            vec![],
            None,
        )
        .unwrap();
        let parent_value = NodeMutationParent {
            id: parent.inode,
            version: parent.version,
            attributes: Some(parent_attributes),
        };
        NodeRenameBaseline {
            library_serial: "RENAME".into(),
            owner_epoch: 100,
            request_epoch: 1,
            spec: NodeRenameSpec {
                pool: Uuid::new_v4(),
                operation: Uuid::new_v4(),
                from: "/source".into(),
                to: "/destination".into(),
                source,
                source_parent: parent,
                destination_parent: parent,
                target: None,
                no_replace: true,
            },
            source_parent: parent_value.clone(),
            destination_parent: parent_value,
            source: ResolvedPath::Directory {
                id: source.inode,
                version: source.version,
                attributes: Some(attributes),
            },
            target: None,
        }
    }

    #[test]
    fn rename_original_request_is_canonical_and_owner_independent() {
        let mut baseline = fixture();
        let request = NodeRenameRequest::from_baseline(&baseline, TIME).unwrap();
        let bytes = request.encode().unwrap();
        assert_eq!(NodeRenameRequest::decode(&bytes).unwrap(), request);
        baseline.owner_epoch = 101;
        assert_eq!(
            NodeRenameRequest::from_baseline(&baseline, TIME).unwrap(),
            request
        );
        assert!(request.matches_baseline(&baseline).unwrap());
        let fingerprint = request.fingerprint();
        baseline.spec.operation = Uuid::new_v4();
        assert_ne!(
            NodeRenameRequest::from_baseline(&baseline, TIME)
                .unwrap()
                .fingerprint(),
            fingerprint
        );
        assert_ne!(
            NodeRenameRequest::from_baseline(&fixture(), TIME)
                .unwrap()
                .fingerprint(),
            fingerprint
        );
    }

    #[test]
    fn rename_requires_original_namespaces_inodes_and_incrementable_versions() {
        let baseline = fixture();
        let original = baseline.spec;
        validate(&original).unwrap();
        for role in 0..3 {
            for bad in [
                NodeRenameRevision {
                    namespace: Uuid::nil(),
                    ..original.source
                },
                NodeRenameRevision {
                    namespace: Uuid::new_v4(),
                    ..original.source
                },
                NodeRenameRevision {
                    inode: Uuid::nil(),
                    ..original.source
                },
                NodeRenameRevision {
                    version: 0,
                    ..original.source
                },
                NodeRenameRevision {
                    version: u64::MAX,
                    ..original.source
                },
            ] {
                let mut spec = original.clone();
                match role {
                    0 => spec.source = bad,
                    1 => spec.source_parent = bad,
                    _ => spec.destination_parent = bad,
                };
                assert!(validate(&spec).is_err(), "role={role} {bad:?}");
            }
        }
        for (from, to) in [
            ("/", "/dest"),
            ("/source", "/"),
            ("/a", "/a"),
            ("/a", "/a/child"),
            ("/a/child", "/a"),
            ("a", "/b"),
            ("/a//b", "/c"),
            ("/a/..", "/c"),
            ("/a/.", "/c"),
            ("/a\0", "/c"),
            ("/a/", "/c"),
        ] {
            let mut spec = original.clone();
            spec.from = from.into();
            spec.to = to.into();
            assert!(validate(&spec).is_err(), "{from}->{to}");
        }
        let mut spec = original.clone();
        spec.source.inode = spec.source_parent.inode;
        assert!(validate(&spec).is_err());
        let mut spec = original.clone();
        spec.target = Some(NodeRenameRevision {
            inode: Uuid::new_v4(),
            ..spec.source
        });
        assert!(validate(&spec).is_err());
        spec.no_replace = false;
        validate(&spec).unwrap();
        spec.target.as_mut().unwrap().namespace = Uuid::new_v4();
        assert!(validate(&spec).is_err());
        let mut spec = original;
        spec.destination_parent.version += 1;
        assert!(validate(&spec).is_err());
    }

    #[test]
    fn rename_snapshots_bind_both_parents_target_absence_and_attributes() {
        let original = fixture();
        let request = NodeRenameRequest::from_baseline(&original, TIME).unwrap();
        let mut changed = original.clone();
        changed.source_parent.attributes = Some(
            NodeAttributes::new(
                NodeMeta {
                    file_uid: 2,
                    readonly: true,
                    ..Default::default()
                },
                vec![],
                None,
            )
            .unwrap(),
        );
        assert!(!request.matches_baseline(&changed).unwrap());
        let mut changed = original.clone();
        changed.destination_parent.attributes = None;
        assert!(!request.matches_baseline(&changed).unwrap());
        let mut changed = original.clone();
        let ResolvedPath::Directory { attributes, .. } = &mut changed.source else {
            unreachable!()
        };
        *attributes = Some(
            NodeAttributes::new(
                NodeMeta {
                    file_uid: 4,
                    readonly: true,
                    ..Default::default()
                },
                vec![],
                None,
            )
            .unwrap(),
        );
        assert!(!request.matches_baseline(&changed).unwrap());
        let mut changed = original.clone();
        changed.target = Some(changed.source.clone());
        assert!(request.matches_baseline(&changed).is_err());
        let mut changed = original;
        changed.spec.source.inode = Uuid::new_v4();
        assert!(request.matches_baseline(&changed).is_err());
    }

    #[test]
    fn rename_codec_rejects_unknown_duplicate_missing_noncanonical_or_oversize_input() {
        let request = NodeRenameRequest::from_baseline(&fixture(), TIME).unwrap();
        let bytes = request.encode().unwrap();
        let original: Value = serde_json::from_slice(&bytes).unwrap();
        for key in original.as_object().unwrap().keys() {
            let mut value = original.clone();
            value.as_object_mut().unwrap().remove(key);
            assert!(
                NodeRenameRequest::decode(value.to_string().as_bytes()).is_err(),
                "missing {key}"
            );
        }
        let mut value = original.clone();
        value["unknown"] = json!(true);
        assert!(NodeRenameRequest::decode(value.to_string().as_bytes()).is_err());
        for key in ["version", "request_epoch"] {
            let mut value = original.clone();
            value[key] = json!(0);
            assert!(NodeRenameRequest::decode(value.to_string().as_bytes()).is_err());
        }
        let mut value = original.clone();
        value["version"] = json!(2);
        assert!(NodeRenameRequest::decode(value.to_string().as_bytes()).is_err());
        let mut value = original.clone();
        value["source"]["inode"] = json!("AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA");
        assert!(NodeRenameRequest::decode(value.to_string().as_bytes()).is_err());
        let mut value = original;
        value["time"] = json!("2026-11-02T10:00:00Z");
        assert!(NodeRenameRequest::decode(value.to_string().as_bytes()).is_err());
        let mut appended = bytes.clone();
        appended.push(b'\n');
        assert!(NodeRenameRequest::decode(&appended).is_err());
        let duplicate = format!(
            "{{\"version\":1,{}",
            std::str::from_utf8(&bytes).unwrap().trim_start_matches('{')
        );
        assert!(NodeRenameRequest::decode(duplicate.as_bytes()).is_err());
        for cut in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(NodeRenameRequest::decode(&bytes[..cut]).is_err());
        }
        assert!(NodeRenameRequest::decode(&vec![b' '; LIMIT + 1]).is_err());
    }
}
