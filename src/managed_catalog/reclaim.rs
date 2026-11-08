//! Reclaim input binds one assigned medium and explicit format consent.
//! A validated request is intent only; it never constitutes an erase proof.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[path = "reclaim_children.rs"]
mod children;
#[path = "reclaim_prepared_cancel.rs"]
mod prepared_cancel;
pub(in crate::managed_catalog) use children::{
    require_intent_at, require_receipt_at, require_request_at,
};
#[path = "reclaim_walk.rs"]
mod walk;
pub use walk::{ReclaimFile, ReclaimWalk};
#[path = "reclaim_recovery.rs"]
mod recovery;
#[path = "reclaim_retirement.rs"]
mod retirement;
pub use retirement::ReclaimBodyDestination;
#[path = "reclaim_source.rs"]
mod source;
pub(in crate::managed_catalog) use source::require_publication_at as require_metadata_publication_at;
pub use source::{
    ReclaimDataProgress, ReclaimFileMetadata, ReclaimMetadataAdmission,
    ReclaimMetadataCommitEvidence, ReclaimMetadataDiscardObservation, ReclaimMetadataInput,
    ReclaimMetadataObservation, ReclaimMetadataObservationKind, ReclaimMetadataPlan,
    ReclaimMetadataProgress, ReclaimMetadataPublication, ReclaimMetadataTailEvidence,
    ReclaimMetadataUnwrittenEvidence, ReclaimMetadataWrite, ReclaimNodeDestination,
    ReclaimNodeMetadata, ReclaimNodeReadRequest, ReclaimReadSource, ReclaimSourceInventory,
    ReclaimSourceNode, ReclaimSourceNodeKind, ReclaimSourceProgress, ReclaimSourceRead,
};

fn domain(ns: Uuid, kind: u8) -> Vec<u8> {
    let mut key = management::prefix(ns, b'R');
    key.push(kind);
    key
}
fn gate_key(ns: Uuid, pool: Uuid) -> Vec<u8> {
    let mut key = domain(ns, b'p');
    key.extend_from_slice(pool.as_bytes());
    key
}

/// An owner-fenced reservation, not permission to format the source medium.
/// No physical operation is performed by the reservation kernel.
#[derive(Clone, Debug)]
pub struct ReclaimReservation {
    intent: Value,
    route: ActiveLibraryRoute,
}

impl ReclaimReservation {
    pub fn intent(&self) -> &Value {
        &self.intent
    }
    pub fn spec(&self) -> ReclaimSpec {
        ReclaimSpec::parse(&self.intent["change"]).expect("validated reclaim identity")
    }
    pub(crate) fn namespace(&self) -> Uuid {
        self.route.route.namespace
    }
    pub(crate) fn serial(&self) -> &str {
        self.intent["library"].as_str().expect("validated library")
    }
    fn fingerprint(&self) -> String {
        format!("{:x}", Sha256::digest(self.intent.to_string().as_bytes()))
    }
    fn journal_key(&self) -> Vec<u8> {
        let mut key = domain(self.namespace(), b'j');
        key.extend_from_slice(
            &self.intent["epoch"]
                .as_u64()
                .expect("validated epoch")
                .to_be_bytes(),
        );
        key.extend_from_slice(
            Uuid::parse_str(
                self.intent["operation"]
                    .as_str()
                    .expect("validated operation"),
            )
            .expect("validated operation")
            .as_bytes(),
        );
        key
    }
    fn token(&self) -> Value {
        json!({"version":1,"namespace":self.namespace().to_string(),"library":self.serial(),"pool":self.intent["change"]["pool"],"epoch":self.intent["epoch"],"operation":self.intent["operation"],"fingerprint":self.fingerprint()})
    }
}

fn decode(raw: &[u8]) -> Result<Value> {
    let value: Value =
        serde_json::from_slice(raw).map_err(|_| CatalogError::Corrupt("回收检查点 JSON 损坏"))?;
    if value.to_string().as_bytes() != raw || raw.len() > 90_000 {
        return Err(CatalogError::Corrupt("回收检查点非规范编码或过大"));
    }
    Ok(value)
}

// A pool with an unrecognised or partially written fence is never writable.
async fn pool_gate_at(tr: &Transaction, ns: Uuid, pool: Uuid) -> Result<Option<Value>> {
    let Some(raw) = tr.get(&gate_key(ns, pool), false).await? else {
        return Ok(None);
    };
    let token = decode(&raw)?;
    let fields = [
        "version",
        "namespace",
        "library",
        "pool",
        "epoch",
        "operation",
        "fingerprint",
    ];
    if token["version"] != 1
        || token["namespace"] != ns.to_string()
        || token["pool"] != pool.to_string()
        || !token.as_object().is_some_and(|map| {
            map.len() == fields.len() && map.keys().all(|k| fields.contains(&k.as_str()))
        })
        || !token["epoch"].as_u64().is_some_and(|n| n > 0)
        || !token["operation"]
            .as_str()
            .is_some_and(|s| Uuid::parse_str(s).is_ok_and(|id| !id.is_nil() && id.to_string() == s))
        || !token["library"]
            .as_str()
            .is_some_and(|s| validate_physical_serial(s).is_ok())
        || !token["fingerprint"].as_str().is_some_and(|s| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
    {
        return Err(CatalogError::Corrupt("回收池写屏障身份或格式损坏"));
    }
    Ok(Some(token))
}

async fn require_metadata_copy_idle_at(tr: &Transaction, ns: Uuid, pool: Uuid) -> Result<()> {
    if let Some(token) = pool_gate_at(tr, ns, pool).await? {
        let mut key = domain(ns, b'A');
        key.extend_from_slice(
            &token["epoch"]
                .as_u64()
                .expect("validated epoch")
                .to_be_bytes(),
        );
        key.extend_from_slice(
            Uuid::parse_str(token["operation"].as_str().expect("validated operation"))
                .expect("validated UUID")
                .as_bytes(),
        );
        if tr.get(&key, false).await?.is_some() {
            return Err(CatalogError::PoolBusy);
        }
    }
    Ok(())
}

pub(super) async fn require_pool_write_at(tr: &Transaction, ns: Uuid, pool: Uuid) -> Result<()> {
    if pool_gate_at(tr, ns, pool).await?.is_some() {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}

async fn record_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<Value> {
    routing::require_runtime_active_owner(tr, reservation.serial(), reservation.route, owner)
        .await?;
    let value = decode(
        &tr.get(&reservation.journal_key(), false)
            .await?
            .ok_or(CatalogError::Incomplete)?,
    )?;
    let fields = [
        "version",
        "intent",
        "fingerprint",
        "owner_epoch",
        "phase",
        "children",
    ];
    if value["version"] != 1
        || value["intent"] != reservation.intent
        || value["fingerprint"] != reservation.fingerprint()
        || !matches!(
            (value["phase"].as_str(), value["children"].as_u64()),
            (Some("reserved"), Some(0)) | (Some("copying"), Some(1..))
        )
        || !value.as_object().is_some_and(|map| {
            map.len() == fields.len() && map.keys().all(|k| fields.contains(&k.as_str()))
        })
        || !value["owner_epoch"].as_u64().is_some_and(|n| n > 0)
        || tr
            .get(
                &gate_key(reservation.namespace(), reservation.spec().pool),
                false,
            )
            .await?
            .as_deref()
            != Some(reservation.token().to_string().as_bytes())
    {
        return Err(CatalogError::Corrupt("回收预留与池屏障不一致"));
    }
    management::exclusion::LibraryTaskClaim::for_reclaim(&reservation.token())?
        .require_owned(tr)
        .await?;
    Ok(value)
}

async fn source_state_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<(VolumeScan, ActiveIndexIdentity)> {
    let spec = reservation.spec();
    let assignment = ManagedCatalog::verified_assignment(tr, reservation.serial(), spec.barcode())
        .await?
        .ok_or(CatalogError::Incomplete)?;
    spec.require_assignment(&assignment, reservation.namespace(), reservation.serial())?;
    let (scan, index) = active_volume::observed_at(tr, reservation.serial(), spec.volume).await?;
    let mut encoded = Vec::new();
    active_volume::encode_index(&mut encoded, index);
    if reservation.intent["scan"] != scan.id.to_string()
        || reservation.intent["scan_generation"] != scan.generation
        || reservation.intent["index"] != json!(encoded)
    {
        return Err(CatalogError::Invalid("回收源索引在封存后改变"));
    }
    Ok((scan, index))
}

impl ManagedCatalog {
    /// Reserve only after current ownership, the original assignment and every
    /// older namespace hold have been proved. The source index is captured in
    /// the same transaction. A retry claims the same immutable reservation.
    #[allow(clippy::too_many_arguments)]
    pub async fn reserve_reclaim_pool(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        owner: u64,
        epoch: u64,
        operation: Uuid,
        spec: &ReclaimSpec,
    ) -> Result<ReclaimReservation> {
        validate_physical_serial(serial)?;
        let spec = ReclaimSpec::parse(&spec.to_json())?;
        if epoch == 0 || operation.is_nil() {
            return Err(CatalogError::Invalid("回收预留请求身份无效"));
        }
        self.db.run(|tr,_| {
            let spec=spec.clone();
            async move { requests::retry_result(async {
                let ns=routing::require_runtime_active_owner(&tr,serial,route,owner).await?;
                require_pool_library(&tr,ns,spec.pool,serial).await?;
                let assignment=Self::verified_assignment(&tr,serial,&spec.barcode).await?.ok_or(CatalogError::Incomplete)?;
                spec.require_assignment(&assignment,ns,serial)?;
                let (scan,index)=active_volume::observed_at(&tr,serial,spec.volume).await?;
                let mut index_bytes=Vec::new();active_volume::encode_index(&mut index_bytes,index);
                let reservation=ReclaimReservation { route, intent:json!({"version":1,"library":serial,"namespace":ns.to_string(),"epoch":epoch,"operation":operation.to_string(),"change":spec.to_json(),"scan":scan.id.to_string(),"scan_generation":scan.generation,"index":index_bytes}) };
                if tr.get(&reservation.journal_key(),false).await?.is_some() {
                    let mut value=record_at(&tr,&reservation,owner).await?;
                    if value["owner_epoch"].as_u64().is_none_or(|saved| saved>owner) {return Err(CatalogError::OwnerFenced);}
                    value["owner_epoch"]=json!(owner);
                    tr.set(&reservation.journal_key(),value.to_string().as_bytes());
                    return Ok(reservation);
                }
                let window=management::window(&tr,ns).await?;
                if epoch!=window.accepting {return Err(CatalogError::Invalid("回收管理请求代次已关闭"));}
                require_pool_writable_at(&tr,ns,spec.pool).await?;
                management::exclusion::require_available_at(&tr,ns,serial).await?;
                let claim=management::exclusion::LibraryTaskClaim::for_reclaim(&reservation.token())?;
                if requests::has_namespace_holds(&tr,ns).await? || active_volume::recovery_active(&tr,spec.volume).await? {
                    return Err(CatalogError::PoolBusy);
                }
                let value=json!({"version":1,"intent":reservation.intent,"fingerprint":reservation.fingerprint(),"owner_epoch":owner,"phase":"reserved","children":0});
                tr.set(&reservation.journal_key(),value.to_string().as_bytes());
                tr.set(&gate_key(ns,spec.pool),reservation.token().to_string().as_bytes());
                claim.acquire(&tr);
                Ok(reservation)
            }.await) }
        }).await?
    }

    pub async fn reserved_reclaim_status(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<Value> {
        let tr = self.db.create_trx()?;
        let value = record_at(&tr, reservation, owner).await?;
        if value["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        Ok(value)
    }

    /// This kernel can release only a reservation which has admitted no child
    /// work. Later copy/format phases require their own durable outcome proofs.
    pub async fn release_reserved_reclaim(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<()> {
        self.db
            .run(|tr, _| async move {
                requests::retry_result(
                    async {
                        routing::require_runtime_active_owner(
                            &tr,
                            reservation.serial(),
                            reservation.route,
                            owner,
                        )
                        .await?;
                        let journal = tr.get(&reservation.journal_key(), false).await?;
                        let gate = tr
                            .get(
                                &gate_key(reservation.namespace(), reservation.spec().pool),
                                false,
                            )
                            .await?;
                        if journal.is_none() && gate.is_none() {
                            return management::exclusion::LibraryTaskClaim::for_reclaim(
                                &reservation.token(),
                            )?
                            .require_released(&tr)
                            .await;
                        }
                        let value = record_at(&tr, reservation, owner).await?;
                        if value["owner_epoch"] != owner {
                            return Err(CatalogError::OwnerFenced);
                        }
                        if value["phase"] != "reserved" || value["children"] != 0 {
                            return Err(CatalogError::PoolBusy);
                        }
                        source::require_metadata_idle_at(&tr, reservation).await?;
                        let claim = management::exclusion::LibraryTaskClaim::for_reclaim(
                            &reservation.token(),
                        )?;
                        walk::clear_reserved_at(&tr, reservation).await?;
                        source::clear_reserved_at(&tr, reservation);
                        tr.clear(&gate_key(reservation.namespace(), reservation.spec().pool));
                        tr.clear(&reservation.journal_key());
                        claim.release(&tr);
                        Ok(())
                    }
                    .await,
                )
            })
            .await?
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimSpec {
    pool: Uuid,
    barcode: String,
    volume: Uuid,
    revision: u64,
    unassign: bool,
}

impl ReclaimSpec {
    pub fn pool(&self) -> Uuid {
        self.pool
    }
    pub fn barcode(&self) -> &str {
        &self.barcode
    }
    pub fn volume(&self) -> Uuid {
        self.volume
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn unassign(&self) -> bool {
        self.unassign
    }

    pub fn parse(value: &Value) -> Result<Self> {
        const FIELDS: &[&str] = &[
            "kind",
            "pool",
            "barcode",
            "volume",
            "revision",
            "confirm_format",
            "unassign",
        ];
        if !value.as_object().is_some_and(|fields| {
            fields.len() == FIELDS.len() && fields.keys().all(|key| FIELDS.contains(&key.as_str()))
        }) || value["kind"] != "media-reclaim"
            || value["confirm_format"] != true
            || !value["unassign"].is_boolean()
        {
            return Err(CatalogError::Invalid(
                "回收须固定完整原请求、显式确认格式化及 unassign 布尔值",
            ));
        }
        let id = |field: &str| {
            value[field]
                .as_str()
                .and_then(|text| {
                    Uuid::parse_str(text)
                        .ok()
                        .filter(|id| !id.is_nil() && id.to_string() == text)
                })
                .ok_or(CatalogError::Invalid("回收池/卷 UUID 无效或不规范"))
        };
        let barcode = value["barcode"]
            .as_str()
            .ok_or(CatalogError::Invalid("回收须指定原条码"))?;
        validate_barcode(barcode)?;
        let revision = value["revision"]
            .as_u64()
            .filter(|revision| *revision > 0)
            .ok_or(CatalogError::Invalid("回收须固定磁带原策略修订号"))?;
        Ok(Self {
            pool: id("pool")?,
            barcode: barcode.to_owned(),
            volume: id("volume")?,
            revision,
            unassign: value["unassign"].as_bool().expect("validated boolean"),
        })
    }

    pub fn to_json(&self) -> Value {
        json!({"kind":"media-reclaim","pool":self.pool.to_string(),"barcode":self.barcode,
            "volume":self.volume.to_string(),"revision":self.revision,"confirm_format":true,"unassign":self.unassign})
    }

    pub fn require_assignment(
        &self,
        assignment: &TapeAssignment,
        ns: Uuid,
        serial: &str,
    ) -> Result<()> {
        if assignment.namespace != ns
            || assignment.library_serial != serial
            || assignment.pool_id != self.pool
            || assignment.barcode != self.barcode
            || assignment.volume_id != Some(self.volume)
            || assignment.revision != self.revision
        {
            return Err(CatalogError::Invalid(
                "回收原磁带的归属、卷或策略修订号已改变",
            ));
        }
        if !assignment.allows_io() {
            return Err(CatalogError::MediaUnavailable);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Value {
        json!({"kind":"media-reclaim","pool":Uuid::new_v4().to_string(),"barcode":"RECLAIML8",
            "volume":Uuid::new_v4().to_string(),"revision":3,"confirm_format":true,"unassign":false})
    }

    #[test]
    fn reclaim_requires_explicit_consent_and_complete_original_identity() {
        let original = request();
        assert_eq!(ReclaimSpec::parse(&original).unwrap().to_json(), original);
        for field in [
            "kind",
            "pool",
            "barcode",
            "volume",
            "revision",
            "confirm_format",
            "unassign",
        ] {
            let mut bad = original.clone();
            bad.as_object_mut().unwrap().remove(field);
            assert!(ReclaimSpec::parse(&bad).is_err(), "missing {field}");
        }
        for (field, value) in [
            ("confirm_format", json!(false)),
            ("confirm_format", json!("true")),
            ("unassign", Value::Null),
            ("revision", json!(0)),
            ("revision", json!(-1)),
            ("pool", json!(Uuid::nil().to_string())),
            ("volume", json!(Uuid::new_v4().simple().to_string())),
            ("barcode", json!("bad/name")),
            ("extra", json!(true)),
        ] {
            let mut bad = original.clone();
            bad[field] = value;
            assert!(ReclaimSpec::parse(&bad).is_err(), "invalid {field}");
        }
    }

    #[test]
    fn reclaim_original_assignment_cannot_follow_rebinding_or_policy_changes() {
        let spec = ReclaimSpec::parse(&request()).unwrap();
        let ns = Uuid::new_v4();
        let assignment = TapeAssignment {
            namespace: ns,
            library_serial: "LIB".into(),
            pool_id: spec.pool,
            barcode: spec.barcode.clone(),
            volume_id: Some(spec.volume),
            revision: spec.revision,
            online: true,
            blacklisted: false,
        };
        spec.require_assignment(&assignment, ns, "LIB").unwrap();
        for field in 0..8 {
            let mut changed = assignment.clone();
            match field {
                0 => changed.namespace = Uuid::new_v4(),
                1 => changed.library_serial = "OTHER".into(),
                2 => changed.pool_id = Uuid::new_v4(),
                3 => changed.barcode = "OTHER0L8".into(),
                4 => changed.volume_id = Some(Uuid::new_v4()),
                5 => changed.revision += 1,
                6 => changed.online = false,
                7 => changed.blacklisted = true,
                _ => unreachable!(),
            }
            assert!(spec.require_assignment(&changed, ns, "LIB").is_err());
        }
    }
}
