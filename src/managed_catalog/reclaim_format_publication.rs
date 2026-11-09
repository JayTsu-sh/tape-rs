//! Independent empty-volume publication receipt. Parent/temporary ledgers may
//! be cleaned later without changing the new baseline or original result.
use super::*;

#[path = "reclaim_cleanup.rs"]
mod cleanup;
use crate::ltfs::recovery::IndexSourceIdentity;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    namespace: String,
    library: String,
    intent: Value,
    goal: Goal,
    owner: u64,
    source: Vec<u8>,
    returned_home: Option<Vec<u8>>,
    logical_files: u64,
    logical_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimFormatResult(Wire);
impl ReclaimFormatResult {
    pub fn output(&self) -> Uuid {
        Uuid::parse_str(&self.0.goal.output).expect("validated output")
    }
    pub fn current(&self) -> ActiveIndexIdentity {
        self.source().identity.head().into()
    }
    pub fn source(&self) -> NativeMediaSource {
        NativeMediaSource {
            identity: IndexSourceIdentity::from_bytes(&self.0.source)
                .expect("validated result source"),
            files: 0,
            paths: 0,
        }
    }
    pub fn value(&self) -> Value {
        json!({"phase":"completed","barcode":self.0.intent["change"]["barcode"],"pool":self.0.intent["change"]["pool"],
            "source_volume":self.0.goal.source,"volume":self.0.goal.output,"unassigned":self.0.returned_home.is_some(),
            "migrated_logical_files":self.0.logical_files,"migrated_logical_bytes":self.0.logical_bytes,
            "initialization_attempts":self.0.goal.attempt,"source":native_media::source_value(&self.source())})
    }
    fn validate(&self) -> Result<()> {
        node_mutation::id(&self.0.namespace)?;
        validate_physical_serial(&self.0.library)?;
        self.0.goal.encode()?;
        let spec = ReclaimSpec::parse(&self.0.intent["change"])?;
        let i = &self.0.intent;
        let fields = [
            "version",
            "library",
            "namespace",
            "epoch",
            "operation",
            "change",
            "scan",
            "scan_generation",
            "index",
        ];
        let fingerprint = format!("{:x}", Sha256::digest(i.to_string().as_bytes()));
        let identity = IndexSourceIdentity::from_bytes(&self.0.source)
            .map_err(|e| CatalogError::Cache(e.to_string()))?;
        let head = identity.head();
        let options = crate::ltfs::mkltfs::MkltfsOptions {
            volume_uuid: Some(self.output()),
            quick: true,
            ..Default::default()
        };
        let records =
            crate::ltfs::mkltfs::reset_partition_records(&options, &self.0.goal.formatted_at, 1)
                .map_err(|e| CatalogError::Cache(e.to_string()))?;
        if let Some(raw) = &self.0.returned_home {
            home::validate_receipt(&self.0.intent, self.0.owner, raw)?;
        }
        if self.0.version != 2
            || self.0.owner == 0
            || self.0.owner > 0xFFFF_FFFF_FFFF
            || !self.0.goal.issued
            || !i.as_object().is_some_and(|o| {
                o.len() == fields.len() && o.keys().all(|k| fields.contains(&k.as_str()))
            })
            || i["version"] != 1
            || i["library"] != self.0.library
            || i["namespace"] != self.0.namespace
            || !i["epoch"].as_u64().is_some_and(|n| n > 0)
            || !i["scan_generation"].as_u64().is_some_and(|n| n > 0)
            || !i["operation"]
                .as_str()
                .is_some_and(|s| node_mutation::id(s).is_ok())
            || !i["scan"]
                .as_str()
                .is_some_and(|s| node_mutation::id(s).is_ok())
            || self.0.goal.parent != fingerprint
            || self.0.goal.source != spec.volume().to_string()
            || spec.unassign() != self.0.returned_home.is_some()
            || head.volume_uuid != self.output()
            || head.generation != 1
            || head.incremental
            || !identity.predecessors().is_empty()
            || head.location.partition != 'b'
            || head.location.start_block != crate::ltfs::volume::FIRST_INDEX_BLOCK
            || head.sha256
                != <[u8; 32]>::from(Sha256::digest(records[5].as_ref().expect("fixed index")))
        {
            return Err(CatalogError::Corrupt(
                "回收完成回执改变原身份、固定空卷或来源",
            ));
        }
        Ok(())
    }
    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut raw =
            serde_json::to_vec(&self.0).map_err(|e| CatalogError::Cache(e.to_string()))?;
        if raw.len() > 90_000 {
            return Err(CatalogError::Invalid("回收完成回执超出 FDB 预算"));
        }
        let sum = Sha256::new()
            .chain_update(b"tape-rs/reclaim-format-result/1")
            .chain_update(&raw)
            .finalize();
        raw.extend_from_slice(&sum);
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if !(32..=90_032).contains(&raw.len()) {
            return Err(CatalogError::Corrupt("回收完成回执长度无效"));
        }
        let value = Self(
            serde_json::from_slice(&raw[..raw.len() - 32])
                .map_err(|_| CatalogError::Corrupt("回收完成回执 JSON 无效"))?,
        );
        if value.encode()? != raw {
            return Err(CatalogError::Corrupt("回收完成回执完整性校验失败"));
        }
        Ok(value)
    }
}
fn result_key(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = domain(reservation.namespace(), b'f');
    key.extend_from_slice(
        &reservation.intent()["epoch"]
            .as_u64()
            .expect("validated epoch")
            .to_be_bytes(),
    );
    key.extend_from_slice(
        Uuid::parse_str(
            reservation.intent()["operation"]
                .as_str()
                .expect("validated operation"),
        )
        .expect("validated operation")
        .as_bytes(),
    );
    key
}
pub(in crate::managed_catalog::control::reclaim) async fn result_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<Option<ReclaimFormatResult>> {
    let Some(raw) = tr.get(&result_key(reservation), false).await? else {
        return Ok(None);
    };
    let result = ReclaimFormatResult::decode(&raw)?;
    if result.0.namespace != reservation.namespace().to_string()
        || result.0.library != reservation.serial()
        || result.0.intent != *reservation.intent()
    {
        return Err(CatalogError::Corrupt("回收完成回执不属于原父任务"));
    }
    Ok(Some(result))
}
impl ManagedCatalog {
    pub async fn reclaim_format_result(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<Option<ReclaimFormatResult>> {
        let tr = self.db.create_trx()?;
        routing::require_runtime_active_owner(&tr, reservation.serial(), reservation.route, owner)
            .await?;
        result_at(&tr, reservation).await
    }
    /// Publish only the actual fixed empty baseline, exact source retirement,
    /// original task result and notifications. Every mutation is one atomic
    /// transaction; an unknown reply is queried through the independent result.
    pub async fn complete_reclaim_format(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        plan: &ReclaimFormatPlan,
        proof: &ReclaimFormatCommitEvidence,
        home: Option<&ReclaimFormatHomeEvidence>,
    ) -> Result<ReclaimFormatResult> {
        proof.require(plan, owner)?;
        if reservation.spec().unassign() != home.is_some() {
            return Err(CatalogError::Invalid("回收解除分配必须提交原机械归位证明"));
        }
        let expected_home = home
            .map(|home| home.receipt(reservation, owner, plan, proof))
            .transpose()?;
        let expected_home = &expected_home;
        self.db.run(|tr,_| Box::pin(async move { requests::retry_result(async {
            routing::require_runtime_active_owner(&tr,reservation.serial(),reservation.route,owner).await?;
            if let Some(result)=result_at(&tr,reservation).await? {
                if result.0.goal!=plan.goal || result.source()!=*proof.source() || result.0.returned_home!=*expected_home {return Err(CatalogError::Invalid("回收完成重试改变原初始化或归位结果"));}
                return Ok(result);
            }
            let saved=at(&tr,reservation,owner).await?.ok_or(CatalogError::Incomplete)?;
            if saved!=*plan || gates_at(&tr,reservation,owner,Some(&saved.goal)).await?!=saved.goal.coverage {return Err(CatalogError::Invalid("回收完成原目标或双门槛改变"));}
            let spec=reservation.spec(); let ns=reservation.namespace(); let serial=reservation.serial();
            let (old_scan,_)=source_state_at(&tr,reservation).await?;
            let old_manifest=tr.get(&scan_key(spec.volume(),old_scan.id),false).await?.ok_or(CatalogError::Incomplete)?;
            if decode_scan_manifest(&old_manifest)?!=(old_scan,serial.into(),ScanState::Ready) {return Err(CatalogError::Corrupt("回收完成原扫描不是就绪状态"));}
            let assignment=ManagedCatalog::verified_assignment(&tr,serial,spec.barcode()).await?.ok_or(CatalogError::Incomplete)?;
            spec.require_assignment(&assignment,ns,serial)?;
            let attributes=tape_policy::read_at(&tr,&assignment).await?;
            let revision=source_revision(&tr,ns).await?.checked_add(1).ok_or(CatalogError::Corrupt("回收来源修订号溢出"))?;
            let scan=VolumeScan {generation:1,id:plan.scan()};
            let placement=VolumePlacement {volume_id:plan.output(),namespace:ns,pool_id:spec.pool(),library_serial:serial.into(),barcode:spec.barcode().into()};
            for key in [scan_key(plan.output(),scan.id),source_key(plan.output(),scan.id),native_media::assigned_base_key(ns,plan.output())] {
                if tr.get(&key,false).await?.is_some() {return Err(CatalogError::Corrupt("回收输出初始扫描已被占用"));}
            }
            let walk=walk::current_state_at(&tr,reservation).await?;
            let returned_home=if let Some(home)=home {Some(home.require_at(&tr,reservation,owner,plan,proof).await?)} else {None};
            let result=ReclaimFormatResult(Wire {version:2,namespace:ns.to_string(),library:serial.into(),intent:reservation.intent().clone(),goal:plan.goal.clone(),owner,source:proof.source().identity.to_bytes(),returned_home,logical_files:walk.copied_files(),logical_bytes:walk.copied_bytes()});
            let raw=result.encode()?;
            let request=ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":reservation.intent()["epoch"],"operation":reservation.intent()["operation"],"change":spec.to_json()}))?;
            let task=if management::has_task_at(&tr,ns,serial,request.epoch(),request.operation()).await? {
                let mut task=management::read_task(&tr,serial,&request).await?;
                if task["status"]!="running" || task["owner_epoch"]!=owner {return Err(CatalogError::Invalid("原回收管理任务不是当前运行中回执"));}
                task["status"]=json!("succeeded"); task["result"]=result.value();
                task["completed_at_millis"]=json!(management::now().max(task["created_at_millis"].as_u64().expect("validated time")));
                Some(task)
            } else {None};
            let claim=management::exclusion::LibraryTaskClaim::for_reclaim(&reservation.token())?;
            claim.require_owned(&tr).await?;
            let mut parent=record_at(&tr,reservation,owner).await?; parent["phase"]=json!("completed");
            let mut new_assignment=assignment.clone(); new_assignment.volume_id=Some(plan.output());
            // Everything above is read-only. Notification errors below abort the
            // whole transaction through atomic_error; no partial binding escapes.
            tape_policy::clear_at(&tr,&assignment);
            managed_task::clear_dependency_at(&tr,reservation);
            native_device::clear_home_at(&tr,ns,serial,spec.barcode());
            if !spec.unassign() {
                tr.set(&placement_key(plan.output()),&encode_placement(&placement));
                tr.set(&barcode_key(serial,spec.barcode()),plan.output().as_bytes());
                tr.set(&assignment_key(serial,spec.barcode()),&encode_assignment_state(ns,spec.pool(),Some(plan.output()),assignment.revision,assignment.online,assignment.blacklisted));
                tape_policy::write_at(&tr,&new_assignment,&attributes);
                tr.set(&ready_volume_key(plan.output()),&encode_ready_volume(scan,serial));
                tr.set(&native_media::assigned_base_key(ns,plan.output()),b"/");
            } else {
                tr.clear(&barcode_key(serial,spec.barcode()));
                tr.clear(&assignment_key(serial,spec.barcode()));
                tr.clear(&namespace_assignment_key(ns,spec.pool(),serial,spec.barcode()));
            }
            tr.set(&scan_key(plan.output(),scan.id),&encode_scan_manifest(scan,serial,if spec.unassign() {ScanState::Retired} else {ScanState::Ready}));
            tr.set(&source_key(plan.output(),scan.id),&index_evidence::encode_source(&proof.source().identity,0));
            tr.set(&source_revision_key(ns),&revision.to_be_bytes());
            tr.set(&scan_key(spec.volume(),old_scan.id),&encode_scan_manifest(old_scan,serial,ScanState::Retired));
            tr.clear(&ready_volume_key(spec.volume())); tr.clear(&placement_key(spec.volume()));
            active_volume::clear_unassigned_cursor_at(&tr,spec.volume());
            tr.clear(&native_media::assigned_base_key(ns,spec.volume()));
            tr.clear(&unavailable_volume_key(ns,plan.output()));
            tr.set(&result_key(reservation),&raw); tr.set(&reservation.journal_key(),parent.to_string().as_bytes());
            tr.set(&cleanup::key(reservation), reservation.token().to_string().as_bytes());
            tr.clear(&node_attempts::volume_key(spec.volume()));
            tr.clear(&native_media::volume_claim_key(plan.output()));
            layout::clear_at(&tr,reservation);
            tr.clear(&gate_key(ns,spec.pool())); claim.release(&tr);
            if let Some(task)=task {management::write_task(&tr,&request,&task).await?;management::clear_queued(&tr,&request);}
            namespace_visibility::publish(&tr,ns);
            notifications::tape_changed_at(&tr,ns,serial,spec.barcode(),"reclaim_completed").await?;
            Ok(result)
        }.await.map_err(notifications::atomic_error)) })).await?
    }
}
