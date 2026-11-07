//! Immutable, owner-fenced payload storage for node plans. Stored bytes are
//! inert: a sealed object is neither a mutation admission nor an S4 proof.
use super::*;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodePlanObject {
    library: String,
    namespace: Uuid,
    digest: [u8; 32],
    len: u32,
}
impl NodePlanObject {
    pub fn new(
        library: &str,
        namespace: Uuid,
        digest: [u8; 32],
        len: u32,
        max_bytes: usize,
    ) -> Result<Self> {
        control::validate_physical_serial(library)?;
        if namespace.is_nil()
            || len == 0
            || len as usize > MAX_REPRESENTABLE
            || len as usize > max_bytes
        {
            return Err(CatalogError::Invalid("节点计划对象身份、长度或预算无效"));
        }
        Ok(Self {
            library: library.into(),
            namespace,
            digest,
            len,
        })
    }
    pub fn for_bytes(
        library: &str,
        namespace: Uuid,
        bytes: &[u8],
        max_bytes: usize,
    ) -> Result<Self> {
        let len =
            u32::try_from(bytes.len()).map_err(|_| CatalogError::Invalid("节点计划对象过长"))?;
        let mut reference = Self::new(library, namespace, [0; 32], len, max_bytes)?;
        reference.digest = Sha256::new()
            .chain_update(super::node_mutation::DOMAIN)
            .chain_update(bytes)
            .finalize()
            .into();
        Ok(reference)
    }
    pub fn library(&self) -> &str {
        &self.library
    }
    pub fn namespace(&self) -> Uuid {
        self.namespace
    }
    pub fn digest(&self) -> [u8; 32] {
        self.digest
    }
    pub fn byte_len(&self) -> u32 {
        self.len
    }
    pub fn chunks(&self) -> u16 {
        (self.len as usize)
            .div_ceil(CHUNK_SIZE)
            .try_into()
            .expect("validated length")
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut out = b"J\x01".to_vec();
        out.extend_from_slice(&(self.library.len() as u16).to_be_bytes());
        out.extend_from_slice(self.library.as_bytes());
        out.extend_from_slice(self.namespace.as_bytes());
        out.extend_from_slice(&self.digest);
        out.extend_from_slice(&self.len.to_be_bytes());
        out
    }
    pub fn decode(raw: &[u8]) -> Result<Self> {
        if raw.len() < 56 || &raw[..2] != b"J\x01" {
            return Err(CatalogError::Corrupt("节点计划对象引用版本或长度"));
        }
        let len = u16::from_be_bytes(raw[2..4].try_into().expect("length checked")) as usize;
        if raw.len() != 56 + len {
            return Err(CatalogError::Corrupt("节点计划对象带库字段长度"));
        }
        let library = std::str::from_utf8(&raw[4..4 + len])
            .map_err(|_| CatalogError::Corrupt("节点计划对象带库编码"))?;
        Self::new(
            library,
            uuid_from(&raw[4 + len..20 + len])?,
            raw[20 + len..52 + len].try_into().expect("length checked"),
            u32::from_be_bytes(raw[52 + len..56 + len].try_into().expect("length checked")),
            MAX_REPRESENTABLE,
        )
        .map_err(|_| CatalogError::Corrupt("节点计划对象引用字段"))
    }
    fn prefix(&self) -> Vec<u8> {
        // @ is already included in namespace-owned cleanup. b is reserved for
        // these inert objects, separate from management tasks and media gates.
        let mut out = key(b'@');
        out.extend_from_slice(self.namespace.as_bytes());
        out.push(b'b');
        out.extend_from_slice(&(self.library.len() as u16).to_be_bytes());
        out.extend_from_slice(self.library.as_bytes());
        out.extend_from_slice(&self.digest);
        out.extend_from_slice(&self.len.to_be_bytes());
        out
    }
    fn marker(&self) -> Vec<u8> {
        let mut key = self.prefix();
        key.push(0);
        key
    }
    fn chunk(&self, index: u16) -> Vec<u8> {
        let mut key = self.prefix();
        key.push(1);
        key.extend_from_slice(&index.to_be_bytes());
        key
    }
    fn chunk_len(&self, index: u16) -> usize {
        CHUNK_SIZE.min(self.len as usize - usize::from(index) * CHUNK_SIZE)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Progress {
    token: Uuid,
    sealed: bool,
    next: u16,
}
impl Progress {
    fn encode(self) -> [u8; 20] {
        let mut out = [0; 20];
        out[0] = 1;
        out[1..17].copy_from_slice(self.token.as_bytes());
        out[17] = u8::from(self.sealed);
        out[18..20].copy_from_slice(&self.next.to_be_bytes());
        out
    }
    fn decode(raw: &[u8], object: &NodePlanObject) -> Result<Self> {
        if raw.len() != 20 || raw[0] != 1 || raw[17] > 1 {
            return Err(CatalogError::Corrupt("节点计划对象上传标记"));
        }
        let value = Self {
            token: uuid_from(&raw[1..17])?,
            sealed: raw[17] == 1,
            next: u16::from_be_bytes(raw[18..20].try_into().expect("length checked")),
        };
        if value.token.is_nil()
            || value.next > object.chunks()
            || (value.sealed && value.next != object.chunks())
        {
            return Err(CatalogError::Corrupt("节点计划对象上传进度"));
        }
        Ok(value)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodePlanUpload {
    Sealed,
    AlreadySealed,
}

async fn live(
    tr: &Transaction,
    object: &NodePlanObject,
    route: ActiveLibraryRoute,
    owner: u64,
) -> Result<()> {
    if route.route.namespace != object.namespace {
        return Err(CatalogError::Invalid("节点计划对象与当前命名空间不符"));
    }
    routing::require_runtime_active_owner(tr, &object.library, route, owner).await?;
    Ok(())
}
async fn progress_at(tr: &Transaction, object: &NodePlanObject) -> Result<Progress> {
    Progress::decode(
        &tr.get(&object.marker(), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
        object,
    )
}
/// The caller must check route/owner and object scope in the same transaction.
/// A sealed payload is still inert; this check grants no admission authority.
pub(super) async fn require_sealed_at(tr: &Transaction, object: &NodePlanObject) -> Result<()> {
    if !progress_at(tr, object).await?.sealed {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}
fn same_attempt(saved: Progress, initial: Progress) -> Result<()> {
    if saved.token != initial.token {
        return Err(CatalogError::Invalid("节点计划上传已重置，请重试同一对象"));
    }
    Ok(())
}
fn io(phase: &'static str, source: std::io::Error) -> CatalogError {
    CatalogError::NodePlanIo { phase, source }
}

impl ManagedCatalog {
    /// Store a validated plan for its current owner. This only persists inert
    /// payload bytes; callers still need durable admission before any media IO.
    pub async fn upload_node_mutation_plan(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        plan: &NodeMutationPlan,
        max_bytes: usize,
    ) -> Result<NodePlanObject> {
        if plan.context().owner_epoch != owner {
            return Err(CatalogError::OwnerFenced);
        }
        let bytes = plan.encode()?;
        let object = NodePlanObject::for_bytes(
            &plan.context().library_serial,
            plan.context().namespace,
            &bytes,
            max_bytes,
        )?;
        self.upload_node_plan_object(route, owner, &object, &mut bytes.as_slice())
            .await?;
        Ok(object)
    }

    /// Decode the complete sealed payload and bind its semantic identity to
    /// the object reference. A successor may read the original owner's plan;
    /// revalidation and admission remain the responsibility of the ledger.
    pub async fn load_node_mutation_plan(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        object: &NodePlanObject,
        max_bytes: usize,
    ) -> Result<NodeMutationPlan> {
        let mut bytes = Vec::new();
        self.node_plan_object_to(route, owner, object, max_bytes, &mut bytes)
            .await?;
        let plan = NodeMutationPlan::decode(&bytes)?;
        if plan.context().library_serial != object.library
            || plan.context().namespace != object.namespace
            || plan.fingerprint() != object.digest
        {
            return Err(CatalogError::Corrupt("节点计划内容与对象作用域不符"));
        }
        Ok(plan)
    }

    /// Stream and resume exact immutable chunks; seal only after the declared
    /// length, EOF and domain-separated digest all match. Partial data remains
    /// resumable and cannot be consumed as an admitted node operation.
    pub async fn upload_node_plan_object<R: Read>(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        object: &NodePlanObject,
        input: &mut R,
    ) -> Result<NodePlanUpload> {
        let token = Uuid::new_v4();
        let initial = self
            .db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        live(&tr, object, route, owner).await?;
                        if let Some(raw) = tr.get(&object.marker(), false).await? {
                            return Progress::decode(&raw, object);
                        }
                        let mut chunks = object.prefix();
                        chunks.push(1);
                        if requests::prefix_has_rows(&tr, &chunks).await? {
                            return Err(CatalogError::Corrupt("节点计划分块缺少上传标记"));
                        }
                        let progress = Progress {
                            token,
                            sealed: false,
                            next: 0,
                        };
                        tr.set(&object.marker(), &progress.encode());
                        Ok(progress)
                    }
                    .await,
                )
            })
            .await??;
        let mut hash = Sha256::new();
        hash.update(super::node_mutation::DOMAIN);
        let mut buffer = vec![0; CHUNK_SIZE];
        for index in 0..object.chunks() {
            let chunk = &mut buffer[..object.chunk_len(index)];
            input.read_exact(chunk).map_err(|e| io("读取计划分块", e))?;
            hash.update(&*chunk);
            let chunk: &[u8] = chunk;
            self.db
                .run(|tr, _| async move {
                    requests::retry_result(
                        async {
                            tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                                256_000,
                            ))?;
                            live(&tr, object, route, owner).await?;
                            let saved = progress_at(&tr, object).await?;
                            same_attempt(saved, initial)?;
                            if index < saved.next {
                                if tr.get(&object.chunk(index), false).await?.as_deref()
                                    != Some(chunk)
                                {
                                    return Err(CatalogError::Corrupt("节点计划对象重试分块不符"));
                                }
                            } else if index == saved.next && !saved.sealed {
                                if tr.get(&object.chunk(index), false).await?.is_some() {
                                    return Err(CatalogError::Corrupt("节点计划分块进度不一致"));
                                }
                                tr.set(&object.chunk(index), chunk);
                                tr.set(
                                    &object.marker(),
                                    &Progress {
                                        next: index + 1,
                                        ..saved
                                    }
                                    .encode(),
                                );
                            } else {
                                return Err(CatalogError::Corrupt("节点计划对象上传进度倒退"));
                            }
                            #[cfg(test)]
                            assert!(tr.get_approximate_size().await? < 256_000);
                            Ok(())
                        }
                        .await,
                    )
                })
                .await??;
        }
        let mut extra = [0];
        loop {
            match input.read(&mut extra) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io("核对计划结束", e)),
                Ok(0) => break,
                Ok(_) => return Err(CatalogError::Invalid("节点计划对象超过声明长度")),
            }
        }
        if <[u8; 32]>::from(hash.finalize()) != object.digest {
            return Err(CatalogError::Corrupt("节点计划对象内容摘要不符"));
        }
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        live(&tr, object, route, owner).await?;
                        let saved = progress_at(&tr, object).await?;
                        same_attempt(saved, initial)?;
                        if saved.next != object.chunks() {
                            return Err(CatalogError::Corrupt("节点计划对象封口前缺少分块"));
                        }
                        if saved.sealed {
                            return Ok(NodePlanUpload::AlreadySealed);
                        }
                        tr.set(
                            &object.marker(),
                            &Progress {
                                sealed: true,
                                ..saved
                            }
                            .encode(),
                        );
                        Ok(NodePlanUpload::Sealed)
                    }
                    .await,
                )
            })
            .await?
    }

    /// Writes only to the caller's private destination. An error may leave a
    /// partial destination; callers must not publish it before this succeeds.
    pub async fn node_plan_object_to<W: Write>(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        object: &NodePlanObject,
        max_bytes: usize,
        output: &mut W,
    ) -> Result<()> {
        if object.len as usize > max_bytes {
            return Err(CatalogError::Invalid("节点计划读取超过处理预算"));
        }
        let tr = self.db.create_trx()?;
        live(&tr, object, route, owner).await?;
        let initial = progress_at(&tr, object).await?;
        if !initial.sealed {
            return Err(CatalogError::Incomplete);
        }
        drop(tr);
        let mut hash = Sha256::new();
        hash.update(super::node_mutation::DOMAIN);
        for index in 0..object.chunks() {
            let tr = self.db.create_trx()?;
            live(&tr, object, route, owner).await?;
            let saved = progress_at(&tr, object).await?;
            same_attempt(saved, initial)?;
            if !saved.sealed {
                return Err(CatalogError::Corrupt("节点计划对象封口倒退"));
            }
            let bytes = tr
                .get(&object.chunk(index), false)
                .await?
                .ok_or(CatalogError::Corrupt("已封口节点计划缺少分块"))?;
            if bytes.len() != object.chunk_len(index) {
                return Err(CatalogError::Corrupt("节点计划对象分块长度"));
            }
            hash.update(bytes.as_ref());
            output
                .write_all(bytes.as_ref())
                .map_err(|e| io("写出计划分块", e))?;
        }
        if <[u8; 32]>::from(hash.finalize()) != object.digest {
            return Err(CatalogError::Corrupt("节点计划对象读取摘要不符"));
        }
        let tr = self.db.create_trx()?;
        live(&tr, object, route, owner).await?;
        if progress_at(&tr, object).await? != initial {
            return Err(CatalogError::Corrupt("节点计划对象读取期间被改变"));
        }
        Ok(())
    }

    /// Explicitly discard an unsealed upload. A fresh token prevents old
    /// writers from sealing a replacement attempt; sealed objects are retained.
    pub async fn discard_partial_node_plan_object(
        &self,
        route: ActiveLibraryRoute,
        owner: u64,
        object: &NodePlanObject,
    ) -> Result<bool> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        live(&tr, object, route, owner).await?;
                        let Some(raw) = tr.get(&object.marker(), false).await? else {
                            let prefix = object.prefix();
                            if requests::prefix_has_rows(&tr, &prefix).await? {
                                return Err(CatalogError::Corrupt("节点计划对象缺少上传标记"));
                            }
                            return Ok(false);
                        };
                        if Progress::decode(&raw, object)?.sealed {
                            return Err(CatalogError::Invalid("不能删除已封口的节点计划对象"));
                        }
                        let prefix = object.prefix();
                        tr.clear_range(&prefix, &prefix_end(&prefix));
                        Ok(true)
                    }
                    .await,
                )
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn elsewhere(action: impl FnOnce() + Send) {
        std::thread::scope(|scope| scope.spawn(action).join().unwrap());
    }

    struct Intercept<'a> {
        data: Cursor<&'a [u8]>,
        at: u64,
        action: Option<Box<dyn FnOnce() + 'a>>,
    }
    impl Read for Intercept<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if self.data.position() >= self.at
                && let Some(action) = self.action.take()
            {
                action();
            }
            self.data.read(buffer)
        }
    }
    struct AfterOutput<'a> {
        bytes: Vec<u8>,
        total: usize,
        action: Option<Box<dyn FnOnce() + 'a>>,
    }
    impl Write for AfterOutput<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            if self.bytes.len() == self.total
                && let Some(action) = self.action.take()
            {
                action();
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；SimLibrary 生成真实计划，不访问设备"]
    async fn node_plan_upload_is_resumable_immutable_scoped_and_fenced() {
        use crate::ltfs::index::Xattr;
        use crate::ltfs::mkltfs::{MkltfsOptions, mkltfs};
        use crate::ltfs::volume::LtfsVolume;
        use crate::scsi::reservation::{ReservationKey, fence};
        use crate::scsi::sim::{SimCartridge, SimLibrary};
        use serde_json::json;

        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let serial = format!("SIM-PLAN-{ns}");
        let challenge = Uuid::new_v4();
        let replies = (1..=3).map(|id| json!({"version":1,"challenge":challenge.to_string(),"node_id":id,"term":7,"leader":2,
            "role":if id==2 {"Leader"} else {"Follower"},"members":{"1":"a:1","2":"b:2","3":"c:3"},
            "quiesce_index":89,"frozen_index":90,"applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect::<Vec<_>>();
        let proof = verify_native_barriers(&serial, challenge, &replies).unwrap();
        let route = catalog.initialize_native_library(ns, &proof).await.unwrap();
        catalog
            .claim_active_owner_epoch(&serial, 100)
            .await
            .unwrap();

        let library = SimLibrary::new(1, 4, 1);
        library
            .insert_cartridge(SimCartridge::blank("PLAN001L8", 64 << 20), 0)
            .unwrap();
        library.load_into_drive("PLAN001L8", 0).unwrap();
        let dev = library.drive(0);
        mkltfs(
            &dev,
            &MkltfsOptions {
                volume_id: "PLN001".into(),
                block_size: 64 << 10,
                ..Default::default()
            },
        )
        .unwrap();
        let reservation = ReservationKey::new(1, 100);
        fence(&dev, reservation).unwrap();
        let mut volume = LtfsVolume::mount(&dev).unwrap();
        volume.set_reservation_guard(Some(reservation));
        volume
            .append_file("/file", &mut Cursor::new(b"preserved body"))
            .unwrap();
        volume.sync_batch().unwrap();
        let previous = volume.verified_committed_index_construct().unwrap().into();
        let context = NodeMutationContext {
            library_serial: serial.clone(),
            namespace: ns,
            pool: Uuid::new_v4(),
            operation: Uuid::new_v4(),
            request_epoch: 1,
            owner_epoch: 100,
            parent: Uuid::new_v4(),
            parent_version: 1,
            node: Uuid::new_v4(),
            expected_version: Some(1),
            scan: Uuid::new_v4(),
            previous,
            path: "/file".into(),
        };
        let canonical_ancestors = vec![NodeMutationAncestor {
            path: "/".into(),
            id: context.parent,
            version: context.parent_version,
            attributes: NodeAttributes::from_directory(&volume.index().root).unwrap(),
        }];
        let canonical_target =
            NodeAttributes::from_file(volume.index().find_file("/file").unwrap()).unwrap();
        let plan = NodeMutationPlan::prepare(
            context,
            NodeMutation::SetXattr {
                attribute: Xattr {
                    key: "large-unknown".into(),
                    value: "x".repeat(200_000),
                    base64: false,
                },
                mode: XattrMode::Upsert,
            },
            volume.index(),
            "2026-10-31T08:00:00.000000000Z",
            &canonical_ancestors,
            Some((NodeKind::File, &canonical_target)),
        )
        .unwrap();
        let bytes = plan.encode().unwrap();
        let object = NodePlanObject::for_bytes(&serial, ns, &bytes, bytes.len()).unwrap();
        assert_eq!(object.digest(), plan.fingerprint());
        assert!(object.chunks() > 3);
        assert!(
            catalog
                .db
                .create_trx()
                .unwrap()
                .get(&node_key(ns, plan.context().node), false)
                .await
                .unwrap()
                .is_none()
        );

        // A short stream persists only complete chunks and is never readable.
        let partial = &bytes[..CHUNK_SIZE * 2 + 3];
        assert!(matches!(
            catalog
                .upload_node_plan_object(route, 100, &object, &mut Cursor::new(partial))
                .await,
            Err(CatalogError::NodePlanIo { .. })
        ));
        let mut output = Vec::new();
        assert!(matches!(
            catalog
                .node_plan_object_to(route, 100, &object, bytes.len(), &mut output)
                .await,
            Err(CatalogError::Incomplete)
        ));
        assert!(output.is_empty());
        assert_eq!(
            catalog
                .upload_node_plan_object(route, 100, &object, &mut Cursor::new(&bytes))
                .await
                .unwrap(),
            NodePlanUpload::Sealed
        );
        assert_eq!(
            catalog
                .upload_node_plan_object(route, 100, &object, &mut Cursor::new(&bytes))
                .await
                .unwrap(),
            NodePlanUpload::AlreadySealed
        );
        catalog
            .node_plan_object_to(route, 100, &object, bytes.len(), &mut output)
            .await
            .unwrap();
        assert_eq!(output, bytes);
        assert_eq!(NodeMutationPlan::decode(&output).unwrap(), plan);
        assert_eq!(
            catalog
                .load_node_mutation_plan(route, 100, &object, bytes.len())
                .await
                .unwrap(),
            plan
        );
        assert_eq!(
            catalog
                .upload_node_mutation_plan(route, 100, &plan, bytes.len())
                .await
                .unwrap(),
            object
        );
        assert!(matches!(
            catalog
                .upload_node_mutation_plan(route, 99, &plan, bytes.len())
                .await,
            Err(CatalogError::OwnerFenced)
        ));
        assert_eq!(
            ActiveIndexIdentity::from(volume.verified_committed_index_construct().unwrap()),
            previous
        );
        assert!(
            volume
                .index()
                .find_file("/file")
                .unwrap()
                .xattr("large-unknown")
                .is_none()
        );
        assert!(
            catalog
                .discard_partial_node_plan_object(route, 100, &object)
                .await
                .is_err()
        );

        let mut altered = bytes.clone();
        altered[CHUNK_SIZE] ^= 1;
        assert!(
            catalog
                .upload_node_plan_object(route, 100, &object, &mut Cursor::new(&altered))
                .await
                .is_err()
        );
        assert!(
            catalog
                .node_plan_object_to(route, 100, &object, bytes.len() - 1, &mut Vec::new())
                .await
                .is_err()
        );
        assert!(
            catalog
                .node_plan_object_to(route, 99, &object, bytes.len(), &mut Vec::new())
                .await
                .is_err()
        );
        let other_ns =
            NodePlanObject::for_bytes(&serial, Uuid::new_v4(), &bytes, bytes.len()).unwrap();
        let other_library =
            NodePlanObject::for_bytes("OTHER-PLAN", ns, &bytes, bytes.len()).unwrap();
        for wrong in [&other_ns, &other_library] {
            assert!(
                catalog
                    .upload_node_plan_object(route, 100, wrong, &mut Cursor::new(&bytes))
                    .await
                    .is_err()
            );
            assert!(
                catalog
                    .node_plan_object_to(route, 100, wrong, bytes.len(), &mut Vec::new())
                    .await
                    .is_err()
            );
        }

        // Inert storage permits opaque bytes, but typed loading never accepts
        // bytes for a different library or a non-plan payload as a node plan.
        let mut wrong_scope = plan.context().clone();
        wrong_scope.namespace = Uuid::new_v4();
        let wrong_plan = NodeMutationPlan::prepare(
            wrong_scope,
            plan.mutation().clone(),
            volume.index(),
            "2026-10-31T08:00:00.000000000Z",
            &canonical_ancestors,
            Some((NodeKind::File, &canonical_target)),
        )
        .unwrap();
        let wrong_bytes = wrong_plan.encode().unwrap();
        let wrong =
            NodePlanObject::for_bytes(&serial, ns, &wrong_bytes, wrong_bytes.len()).unwrap();
        catalog
            .upload_node_plan_object(route, 100, &wrong, &mut wrong_bytes.as_slice())
            .await
            .unwrap();
        assert!(
            catalog
                .load_node_mutation_plan(route, 100, &wrong, wrong_bytes.len())
                .await
                .is_err()
        );

        // Overlong input and bad digests cannot seal. Explicit reset is needed
        // for poisoned complete chunks; it must fence writers of the old token.
        let payload = vec![7; CHUNK_SIZE * 3 + 1];
        let reset = NodePlanObject::for_bytes(&serial, ns, &payload, payload.len()).unwrap();
        let mut overlong = payload.clone();
        overlong.push(8);
        assert!(
            catalog
                .upload_node_plan_object(route, 100, &reset, &mut Cursor::new(overlong))
                .await
                .is_err()
        );
        assert!(
            !progress_at(&catalog.db.create_trx().unwrap(), &reset)
                .await
                .unwrap()
                .sealed
        );
        assert!(
            catalog
                .discard_partial_node_plan_object(route, 100, &reset)
                .await
                .unwrap()
        );
        let mut bad = payload.clone();
        bad[0] ^= 1;
        assert!(
            catalog
                .upload_node_plan_object(route, 100, &reset, &mut Cursor::new(bad))
                .await
                .is_err()
        );
        assert!(
            !progress_at(&catalog.db.create_trx().unwrap(), &reset)
                .await
                .unwrap()
                .sealed
        );
        assert!(
            catalog
                .upload_node_plan_object(route, 100, &reset, &mut Cursor::new(&payload))
                .await
                .is_err()
        );
        assert!(
            catalog
                .discard_partial_node_plan_object(route, 100, &reset)
                .await
                .unwrap()
        );
        let mut input = Intercept {
            data: Cursor::new(&payload),
            at: CHUNK_SIZE as u64,
            action: Some(Box::new(|| {
                elsewhere(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async {
                            assert!(
                                catalog
                                    .discard_partial_node_plan_object(route, 100, &reset)
                                    .await
                                    .unwrap()
                            );
                            catalog
                                .upload_node_plan_object(
                                    route,
                                    100,
                                    &reset,
                                    &mut Cursor::new(&payload),
                                )
                                .await
                                .unwrap();
                        })
                });
            })),
        };
        assert!(
            catalog
                .upload_node_plan_object(route, 100, &reset, &mut input)
                .await
                .is_err()
        );
        let mut recovered = Vec::new();
        catalog
            .node_plan_object_to(route, 100, &reset, payload.len(), &mut recovered)
            .await
            .unwrap();
        assert_eq!(recovered, payload);

        // Corruption in sealed storage is detected even at the same length.
        let tr = catalog.db.create_trx().unwrap();
        let original = tr
            .get(&reset.chunk(0), false)
            .await
            .unwrap()
            .unwrap()
            .to_vec();
        let mut damaged = original.clone();
        damaged[0] ^= 1;
        tr.set(&reset.chunk(0), &damaged);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .node_plan_object_to(route, 100, &reset, payload.len(), &mut Vec::new())
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&reset.chunk(0));
        tr.commit().await.unwrap();
        assert!(
            catalog
                .node_plan_object_to(route, 100, &reset, payload.len(), &mut Vec::new())
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&reset.chunk(0), &original[..1]);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .node_plan_object_to(route, 100, &reset, payload.len(), &mut Vec::new())
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&reset.chunk(0), &original);
        tr.commit().await.unwrap();

        // The final owner check must run after the last byte was delivered.
        let mut sink = AfterOutput {
            bytes: Vec::new(),
            total: payload.len(),
            action: Some(Box::new(|| {
                elsewhere(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async {
                            catalog
                                .claim_active_owner_epoch(&serial, 101)
                                .await
                                .unwrap();
                        })
                });
            })),
        };
        assert!(
            catalog
                .node_plan_object_to(route, 100, &reset, payload.len(), &mut sink)
                .await
                .is_err()
        );
        assert_eq!(sink.bytes, payload);
        catalog
            .node_plan_object_to(route, 101, &reset, payload.len(), &mut Vec::new())
            .await
            .unwrap();
        assert!(
            catalog
                .load_node_mutation_plan(route, 101, &reset, payload.len())
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .load_node_mutation_plan(route, 101, &object, bytes.len())
                .await
                .unwrap(),
            plan
        );

        let payload2 = vec![9; CHUNK_SIZE * 3];
        let unfinished = NodePlanObject::for_bytes(&serial, ns, &payload2, payload2.len()).unwrap();
        let mut input = Intercept {
            data: Cursor::new(&payload2),
            at: CHUNK_SIZE as u64,
            action: Some(Box::new(|| {
                elsewhere(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(async {
                            catalog
                                .claim_active_owner_epoch(&serial, 102)
                                .await
                                .unwrap();
                        })
                });
            })),
        };
        assert!(
            catalog
                .upload_node_plan_object(route, 101, &unfinished, &mut input)
                .await
                .is_err()
        );
        assert!(matches!(
            catalog
                .node_plan_object_to(route, 102, &unfinished, payload2.len(), &mut Vec::new())
                .await,
            Err(CatalogError::Incomplete)
        ));
        catalog
            .upload_node_plan_object(route, 102, &unfinished, &mut Cursor::new(&payload2))
            .await
            .unwrap();
        assert!(
            catalog
                .db
                .create_trx()
                .unwrap()
                .get(&node_key(ns, plan.context().node), false)
                .await
                .unwrap()
                .is_none()
        );

        let orphan = NodePlanObject::for_bytes(&serial, ns, b"orphan", 6).unwrap();
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&orphan.chunk(0), b"orphan");
        tr.commit().await.unwrap();
        assert!(matches!(
            catalog
                .upload_node_plan_object(route, 102, &orphan, &mut Cursor::new(b"orphan"))
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        assert!(matches!(
            catalog
                .discard_partial_node_plan_object(route, 102, &orphan)
                .await,
            Err(CatalogError::Corrupt(_))
        ));
        let tr = catalog.db.create_trx().unwrap();
        tr.clear(&orphan.chunk(0));
        tr.commit().await.unwrap();
        assert!(
            !catalog
                .discard_partial_node_plan_object(route, 102, &orphan)
                .await
                .unwrap()
        );
    }
    #[test]
    fn reference_and_progress_are_strict_scoped_and_bounded() {
        let ns = Uuid::new_v4();
        for len in [
            1,
            CHUNK_SIZE as u32,
            CHUNK_SIZE as u32 + 1,
            MAX_REPRESENTABLE as u32,
        ] {
            let object =
                NodePlanObject::new("SERIAL", ns, [7; 32], len, MAX_REPRESENTABLE).unwrap();
            let encoded = object.encode();
            assert_eq!(NodePlanObject::decode(&encoded).unwrap(), object);
            for end in 0..encoded.len() {
                assert!(NodePlanObject::decode(&encoded[..end]).is_err());
            }
            for next in [0, object.chunks()] {
                let progress = Progress {
                    token: Uuid::new_v4(),
                    sealed: false,
                    next,
                };
                assert_eq!(
                    Progress::decode(&progress.encode(), &object).unwrap(),
                    progress
                );
            }
            let mut changed = encoded;
            changed[1] = 2;
            assert!(NodePlanObject::decode(&changed).is_err());
            let invalid = Progress {
                token: Uuid::new_v4(),
                sealed: true,
                next: 0,
            };
            assert!(Progress::decode(&invalid.encode(), &object).is_err());
        }
        assert!(NodePlanObject::new("SERIAL", ns, [7; 32], 0, MAX_REPRESENTABLE).is_err());
        assert!(
            NodePlanObject::new(
                "SERIAL",
                ns,
                [7; 32],
                MAX_REPRESENTABLE as u32 + 1,
                usize::MAX
            )
            .is_err()
        );
        assert!(NodePlanObject::new("SERIAL", ns, [7; 32], 2, 1).is_err());
        let object = NodePlanObject::for_bytes("SERIAL", ns, b"payload", 7).unwrap();
        assert_ne!(
            object.prefix(),
            NodePlanObject::for_bytes("OTHER", ns, b"payload", 7)
                .unwrap()
                .prefix()
        );
        assert_ne!(
            object.prefix(),
            NodePlanObject::for_bytes("SERIAL", Uuid::new_v4(), b"payload", 7)
                .unwrap()
                .prefix()
        );
        assert!(
            object
                .prefix()
                .starts_with(&[key(b'@'), ns.as_bytes().to_vec()].concat())
        );
        assert!(super::super::cleanup::NAMESPACE_ROWS.contains(&b'@'));
    }
}
