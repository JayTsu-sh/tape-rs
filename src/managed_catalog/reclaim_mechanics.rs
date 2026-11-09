//! A mechanical command belongs to the original reclaim. Unknown commands
//! retain their issue; a takeover observes the exact postcondition first.
use super::*;
use crate::changer::element::{ElementAddressMap, ElementStatus, ElementType};
use serde::{Deserialize, Serialize};

pub(super) mod uuid_text {
    use super::*;
    pub fn serialize<S: serde::Serializer>(
        value: &Uuid,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        value.to_string().serialize(serializer)
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Uuid, D::Error> {
        let value = String::deserialize(deserializer)?;
        Uuid::parse_str(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclaimMediaAction {
    Load,
    Unload,
    Move,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimMediaPlan {
    pub action: ReclaimMediaAction,
    pub barcode: String,
    #[serde(with = "uuid_text")]
    pub volume: Uuid,
    pub drive_serial: String,
    pub source: u16,
    pub destination: u16,
    pub home: u16,
    pub map: ElementAddressMap,
}
impl ReclaimMediaPlan {
    pub(super) fn validate(&self) -> Result<()> {
        validate_barcode(&self.barcode)?;
        validate_serial(&self.drive_serial)?;
        let within = |address: u16, first: u16, count: u16| {
            count > 0 && address >= first && address - first < count
        };
        let storage = |a| within(a, self.map.storage_start, self.map.storage_count);
        let drive = |a| within(a, self.map.dt_start, self.map.dt_count);
        let valid = match self.action {
            ReclaimMediaAction::Move => {
                self.source != self.destination
                    && ((storage(self.source)
                        && drive(self.destination)
                        && self.home == self.source)
                        || (drive(self.source)
                            && storage(self.destination)
                            && self.home == self.destination))
            }
            _ => self.source == self.destination && drive(self.source) && storage(self.home),
        };
        if self.volume.is_nil() || !valid {
            return Err(CatalogError::Invalid(
                "回收机械计划必须绑定实际地址映射、驱动和原 home",
            ));
        }
        Ok(())
    }
    pub(crate) fn condition(
        &self,
        map: &ElementAddressMap,
        rows: &[ElementStatus],
        ready: Option<bool>,
        completed: bool,
    ) -> Result<bool> {
        self.validate()?;
        if map != &self.map {
            return Err(CatalogError::Invalid("回收机械原地址映射改变"));
        }
        let one = |address| -> Result<&ElementStatus> {
            let mut matches = rows.iter().filter(|e| e.address == address);
            let row = matches
                .next()
                .ok_or(CatalogError::Invalid("回收机械原地址未出现在库存中"))?;
            if matches.next().is_some()
                || row.exception
                || row.disabled
                || self.action == ReclaimMediaAction::Move
                    && !completed
                    && row.accessible == Some(false)
                || row.is_cleaning()
            {
                return Err(CatalogError::Invalid(
                    "回收机械元素不可访问、异常、重复或为清洗带",
                ));
            }
            Ok(row)
        };
        let source = one(self.source)?;
        let destination = one(self.destination)?;
        for row in [source, destination] {
            let expected = if row.address >= map.storage_start
                && row.address - map.storage_start < map.storage_count
            {
                ElementType::Storage
            } else {
                ElementType::DataTransfer
            };
            if row.element_type != expected {
                return Err(CatalogError::Invalid("回收机械元素类型与地址映射不同"));
            }
        }
        if rows
            .iter()
            .filter(|e| e.full && e.volume_tag.as_deref() == Some(self.barcode.as_str()))
            .count()
            != 1
        {
            return Err(CatalogError::Invalid("回收机械原条码未唯一出现在库存中"));
        }
        let drive = if source.element_type == ElementType::DataTransfer {
            source
        } else {
            destination
        };
        let ids: Vec<_> = rows
            .iter()
            .filter(|e| {
                e.element_type == ElementType::DataTransfer
                    && e.drive_serial() == Some(self.drive_serial.as_str())
            })
            .collect();
        if ids.len() != 1 || ids[0].address != drive.address {
            return Err(CatalogError::Invalid(
                "回收机械驱动序列号未唯一映射到原地址",
            ));
        }
        let full = |row: &ElementStatus| {
            row.full && row.volume_tag.as_deref() == Some(self.barcode.as_str())
        };
        let empty = |row: &ElementStatus| !row.full && row.volume_tag.is_none();
        Ok(match self.action {
            ReclaimMediaAction::Move if completed => {
                empty(source)
                    && full(destination)
                    && (destination.element_type != ElementType::DataTransfer
                        || destination.source_address == Some(self.home))
            }
            ReclaimMediaAction::Move => {
                full(source)
                    && empty(destination)
                    && (source.element_type != ElementType::DataTransfer
                        || source.source_address == Some(self.home) && ready == Some(false))
            }
            ReclaimMediaAction::Load => {
                full(source) && ready == Some(completed) && source.source_address == Some(self.home)
            }
            ReclaimMediaAction::Unload => {
                full(source)
                    && ready == Some(!completed)
                    && source.source_address == Some(self.home)
            }
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    version: u8,
    parent: Value,
    owner: u64,
    #[serde(with = "uuid_text")]
    issue: Uuid,
    plan: ReclaimMediaPlan,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimMediaCommand(Wire);
impl ReclaimMediaCommand {
    pub(in crate::managed_catalog::control::reclaim) fn intent(&self) -> &Value {
        &self.0.parent
    }
    pub fn plan(&self) -> &ReclaimMediaPlan {
        &self.0.plan
    }
    pub fn owner(&self) -> u64 {
        self.0.owner
    }
    pub(in crate::managed_catalog::control::reclaim) fn encode(&self) -> Result<Vec<u8>> {
        self.0.plan.validate()?;
        if self.0.version != 1 || self.0.owner == 0 || self.0.issue.is_nil() {
            return Err(CatalogError::Corrupt("回收机械命令身份无效"));
        }
        let mut raw =
            serde_json::to_vec(&self.0).map_err(|e| CatalogError::Cache(e.to_string()))?;
        if raw.len() > 8192 {
            return Err(CatalogError::Invalid("回收机械命令超出预算"));
        }
        let sum = Sha256::new()
            .chain_update(b"tape-rs/reclaim-media-command/1")
            .chain_update(&raw)
            .finalize();
        raw.extend_from_slice(&sum);
        Ok(raw)
    }
    pub(in crate::managed_catalog::control::reclaim) fn decode(raw: &[u8]) -> Result<Self> {
        if !(32..=8224).contains(&raw.len()) {
            return Err(CatalogError::Corrupt("回收机械命令过长"));
        }
        let command = Self(
            serde_json::from_slice(&raw[..raw.len() - 32])
                .map_err(|_| CatalogError::Corrupt("回收机械命令 JSON 无效"))?,
        );
        if command.encode()? != raw {
            return Err(CatalogError::Corrupt("回收机械命令编码不规范"));
        }
        Ok(command)
    }
}
#[derive(Debug)]
pub struct ReclaimMediaPreparation {
    plan: ReclaimMediaPlan,
    owner: u64,
}
impl ReclaimMediaPreparation {
    pub(crate) fn from_read(
        plan: &ReclaimMediaPlan,
        owner: u64,
        map: &ElementAddressMap,
        rows: &[ElementStatus],
        ready: Option<bool>,
    ) -> Result<Self> {
        if owner == 0 || !plan.condition(map, rows, ready, false)? {
            return Err(CatalogError::Invalid("回收机械命令不满足实际原前置状态"));
        }
        Ok(Self {
            plan: plan.clone(),
            owner,
        })
    }
}
#[derive(Debug)]
pub struct ReclaimMediaObservation {
    command: ReclaimMediaCommand,
    owner: u64,
}
impl ReclaimMediaObservation {
    pub(crate) fn from_read(
        command: &ReclaimMediaCommand,
        owner: u64,
        map: &ElementAddressMap,
        rows: &[ElementStatus],
        ready: Option<bool>,
    ) -> Result<Self> {
        if owner < command.owner() || !command.plan().condition(map, rows, ready, true)? {
            return Err(CatalogError::Incomplete);
        }
        Ok(Self {
            command: command.clone(),
            owner,
        })
    }
}
fn own_key(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = reservation.journal_key();
    key[domain(reservation.namespace(), b'j').len() - 1] = b'M';
    key
}
fn last_key(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = own_key(reservation);
    key[domain(reservation.namespace(), b'j').len() - 1] = b'N';
    key
}
pub(super) fn clear_finished_at(tr: &Transaction, reservation: &ReclaimReservation) {
    tr.clear(&last_key(reservation));
}
pub(super) async fn require_confirmed_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    command: &ReclaimMediaCommand,
) -> Result<()> {
    require_parent_idle_at(tr, reservation).await?;
    scope(tr, reservation, owner, command.plan()).await?;
    if tr.get(&last_key(reservation), false).await?.as_deref() != Some(command.encode()?.as_slice())
    {
        return Err(CatalogError::Corrupt("回收机械归位缺少原 MOVE 确认回执"));
    }
    Ok(())
}
pub(super) async fn require_parent_idle_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<()> {
    require_idle_at(tr, reservation.namespace(), reservation.serial()).await?;
    if tr.get(&own_key(reservation), false).await?.is_some() {
        return Err(CatalogError::Corrupt("回收机械库屏障缺失但原命令仍在"));
    }
    Ok(())
}
pub(in crate::managed_catalog::control) fn gate(ns: Uuid, serial: &str) -> Vec<u8> {
    let mut key = management::prefix(ns, b'K');
    key.push(b'r');
    append_component(&mut key, serial.as_bytes());
    key
}
pub(in crate::managed_catalog::control) async fn require_idle_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
) -> Result<()> {
    if tr.get(&gate(ns, serial), false).await?.is_some() {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}
async fn scope(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
    plan: &ReclaimMediaPlan,
) -> Result<()> {
    plan.validate()?;
    let parent = record_at(tr, reservation, owner).await?;
    if let Some(layout) = layout::at(tr, reservation).await? {
        layout.require_plan(plan)?;
    }
    if parent["owner_epoch"] != owner {
        return Err(CatalogError::OwnerFenced);
    }
    if plan.volume == reservation.spec().volume() {
        if plan.barcode != reservation.spec().barcode() {
            return Err(CatalogError::Invalid("回收机械源条码改变"));
        }
        source_state_at(tr, reservation).await?;
    } else {
        if parent["phase"] == "reformatting" {
            return Err(CatalogError::Invalid("初始化阶段禁止移动其他磁带"));
        }
        let assignment =
            ManagedCatalog::verified_assignment(tr, reservation.serial(), &plan.barcode)
                .await?
                .ok_or(CatalogError::Incomplete)?;
        if assignment.namespace != reservation.namespace()
            || assignment.pool_id != reservation.spec().pool()
            || assignment.volume_id != Some(plan.volume)
            || !assignment.online
            || assignment.blacklisted
        {
            return Err(CatalogError::Invalid("回收机械目的不再属于原在线池"));
        }
    }
    if tr
        .get(
            &native_device::library_gate(reservation.namespace(), reservation.serial()),
            false,
        )
        .await?
        .is_some()
        || automatic_media::pending_at(tr, reservation.namespace(), reservation.serial())
            .await?
            .is_some()
    {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}
async fn pending_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
    owner: u64,
) -> Result<Option<ReclaimMediaCommand>> {
    let global = tr
        .get(&gate(reservation.namespace(), reservation.serial()), false)
        .await?;
    let own = tr.get(&own_key(reservation), false).await?;
    match (global, own) {
        (None, None) => {
            record_at(tr, reservation, owner).await?;
            Ok(None)
        }
        (Some(global), Some(own)) if global.as_ref() == own.as_ref() => {
            let command = ReclaimMediaCommand::decode(&own)?;
            if command.0.parent != *reservation.intent() || command.owner() > owner {
                return Err(CatalogError::OwnerFenced);
            }
            scope(tr, reservation, owner, command.plan()).await?;
            Ok(Some(command))
        }
        _ => Err(CatalogError::Corrupt("回收机械原命令与库屏障不一致")),
    }
}
pub(super) async fn management_progress_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    parent: &Value,
) -> Result<Value> {
    let mut own = domain(ns, b'M');
    own.extend_from_slice(
        &parent["intent"]["epoch"]
            .as_u64()
            .ok_or(CatalogError::Corrupt("回收机械进度原代次无效"))?
            .to_be_bytes(),
    );
    own.extend_from_slice(
        node_mutation::id(
            parent["intent"]["operation"]
                .as_str()
                .ok_or(CatalogError::Corrupt("回收机械进度原操作缺失"))?,
        )?
        .as_bytes(),
    );
    let global = tr.get(&gate(ns, serial), false).await?;
    let local = tr.get(&own, false).await?;
    let Some(local) = local else {
        if global.is_some() {
            return Err(CatalogError::Corrupt("回收机械库屏障缺少原进度"));
        }
        return Ok(Value::Null);
    };
    if global.as_deref() != Some(local.as_ref()) {
        return Err(CatalogError::Corrupt("回收机械进度与库屏障不同"));
    }
    let command = ReclaimMediaCommand::decode(&local)?;
    if command.0.parent != parent["intent"]
        || Some(command.owner()) > parent["owner_epoch"].as_u64()
    {
        return Err(CatalogError::Corrupt("回收机械进度改变原父任务或 owner"));
    }
    Ok(
        json!({"phase":"result_unknown","issue":command.0.issue.to_string(),"issued_owner":command.owner(),"plan":command.plan()}),
    )
}
impl ManagedCatalog {
    pub async fn reclaim_confirmed_media(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<Option<ReclaimMediaCommand>> {
        let tr = self.db.create_trx()?;
        require_parent_idle_at(&tr, reservation).await?;
        let Some(raw) = tr.get(&last_key(reservation), false).await? else {
            let parent = record_at(&tr, reservation, owner).await?;
            if parent["owner_epoch"] != owner {
                return Err(CatalogError::OwnerFenced);
            }
            return Ok(None);
        };
        let command = ReclaimMediaCommand::decode(&raw)?;
        if command.intent() != reservation.intent() || command.owner() > owner {
            return Err(CatalogError::OwnerFenced);
        }
        scope(&tr, reservation, owner, command.plan()).await?;
        Ok(Some(command))
    }
    pub async fn reclaim_media_pending(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<Option<ReclaimMediaCommand>> {
        pending_at(&self.db.create_trx()?, reservation, owner).await
    }
    pub async fn issue_reclaim_media(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        proof: &ReclaimMediaPreparation,
    ) -> Result<(ReclaimMediaCommand, bool)> {
        if proof.owner != owner {
            return Err(CatalogError::OwnerFenced);
        }
        let proposed = ReclaimMediaCommand(Wire {
            version: 1,
            parent: reservation.intent().clone(),
            owner,
            issue: Uuid::new_v4(),
            plan: proof.plan.clone(),
        });
        let raw = proposed.encode()?;
        self.db
            .run(|tr, _| {
                let proposed = &proposed;
                let raw = &raw;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            if let Some(command) = pending_at(&tr, reservation, owner).await? {
                                if command.plan() != &proof.plan {
                                    return Err(CatalogError::PoolBusy);
                                }
                                let granted = command == *proposed;
                                return Ok((command, granted));
                            }
                            scope(&tr, reservation, owner, &proof.plan).await?;
                            tr.set(&gate(reservation.namespace(), reservation.serial()), raw);
                            tr.set(&own_key(reservation), raw);
                            Ok((proposed.clone(), true))
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
    pub async fn verify_reclaim_media(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimMediaCommand,
    ) -> Result<()> {
        if pending_at(&self.db.create_trx()?, reservation, owner)
            .await?
            .as_ref()
            != Some(original)
        {
            return Err(CatalogError::OwnerFenced);
        }
        Ok(())
    }
    pub async fn confirm_reclaim_media(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        original: &ReclaimMediaCommand,
        proof: &ReclaimMediaObservation,
    ) -> Result<()> {
        if proof.owner != owner || proof.command != *original {
            return Err(CatalogError::OwnerFenced);
        }
        self.db
            .run(|tr, _| {
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let pending = pending_at(&tr, reservation, owner).await?;
                            let raw = original.encode()?;
                            if pending.is_none()
                                && tr.get(&last_key(reservation), false).await?.as_deref()
                                    == Some(raw.as_slice())
                            {
                                return Ok(());
                            }
                            if pending.as_ref() != Some(original) {
                                return Err(CatalogError::OwnerFenced);
                            }
                            tr.set(&last_key(reservation), &raw);
                            tr.clear(&gate(reservation.namespace(), reservation.serial()));
                            tr.clear(&own_key(reservation));
                            Ok(())
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (ReclaimMediaPlan, Vec<ElementStatus>) {
        let map = ElementAddressMap {
            transport_start: 0,
            transport_count: 1,
            storage_start: 100,
            storage_count: 2,
            ie_start: 200,
            ie_count: 1,
            dt_start: 10,
            dt_count: 1,
        };
        let row = |address, kind, full| ElementStatus {
            address,
            element_type: kind,
            full,
            accessible: Some(true),
            exception: false,
            asc: 0,
            ascq: 0,
            disabled: false,
            medium_type: Some(1),
            volume_tag: full.then(|| "TEST00L8".into()),
            source_address: (kind == ElementType::DataTransfer).then_some(100),
            drive_id: (kind == ElementType::DataTransfer).then(|| "DRIVE1".into()),
        };
        let plan = ReclaimMediaPlan {
            action: ReclaimMediaAction::Move,
            barcode: "TEST00L8".into(),
            volume: Uuid::new_v4(),
            drive_serial: "DRIVE1".into(),
            source: 10,
            destination: 100,
            home: 100,
            map,
        };
        (
            plan,
            vec![
                row(10, ElementType::DataTransfer, true),
                row(100, ElementType::Storage, false),
            ],
        )
    }
    #[test]
    fn vendor_model_dvcid_requires_one_exact_serial_at_the_original_address() {
        let (plan, mut rows) = fixture();
        rows[0].drive_id = Some("IBM ULT3580-TD8 DRIVE1".into());
        assert!(
            plan.condition(&plan.map, &rows, Some(false), false)
                .unwrap()
        );
        rows[0].drive_id = Some("IBM ULT3580-TD8 DRIVE10".into());
        assert!(
            plan.condition(&plan.map, &rows, Some(false), false)
                .is_err()
        );
        rows[0].drive_id = Some("IBM ULT3580-TD8 DRIVE1".into());
        let mut duplicate = rows[0].clone();
        duplicate.address = 11;
        let mut plan = plan;
        plan.map.dt_count = 2;
        rows.push(duplicate);
        assert!(
            plan.condition(&plan.map, &rows, Some(false), false)
                .is_err()
        );
    }

    #[test]
    fn mechanical_receipt_detects_corrupt_bytes_truncation_and_changed_plan() {
        let (plan, _) = fixture();
        // Codec payload only; device authority separately checks the exact
        // original reservation and its current owner in the FDB transaction.
        let command = ReclaimMediaCommand(Wire {
            version: 1,
            parent: json!({"epoch":1}),
            owner: 7,
            issue: Uuid::new_v4(),
            plan,
        });
        let raw = command.encode().unwrap();
        assert_eq!(ReclaimMediaCommand::decode(&raw).unwrap(), command);
        for index in [0, raw.len() / 2, raw.len() - 1] {
            let mut changed = raw.clone();
            changed[index] ^= 1;
            assert!(ReclaimMediaCommand::decode(&changed).is_err());
        }
        assert!(ReclaimMediaCommand::decode(&raw[..31]).is_err());
        let mut wire: Value = serde_json::from_slice(&raw[..raw.len() - 32]).unwrap();
        wire["plan"]["drive_serial"] = json!("OTHER1");
        let mut changed = serde_json::to_vec(&wire).unwrap();
        changed.extend_from_slice(&raw[raw.len() - 32..]);
        assert!(ReclaimMediaCommand::decode(&changed).is_err());
    }
    #[test]
    fn movement_requires_unthreaded_unique_original_cartridge_drive_map_and_home() {
        let (plan, mut rows) = fixture();
        assert!(
            plan.condition(&plan.map, &rows, Some(false), false)
                .unwrap()
        );
        assert!(!plan.condition(&plan.map, &rows, Some(true), false).unwrap());
        assert!(!plan.condition(&plan.map, &rows, None, false).unwrap());
        rows[0].accessible = Some(false);
        assert!(
            plan.condition(&plan.map, &rows, Some(false), false)
                .is_err()
        );
        rows[0].accessible = Some(true);
        rows[0].source_address = Some(101);
        assert!(
            !plan
                .condition(&plan.map, &rows, Some(false), false)
                .unwrap()
        );
        rows[0].source_address = Some(100);
        let mut duplicate = rows[0].clone();
        duplicate.address = 101;
        rows.push(duplicate);
        assert!(
            plan.condition(&plan.map, &rows, Some(false), false)
                .is_err()
        );
        rows.pop();
        rows[1].element_type = ElementType::Transport;
        assert!(
            plan.condition(&plan.map, &rows, Some(false), false)
                .is_err()
        );
    }
    #[test]
    fn load_and_unload_observation_do_not_require_robot_access_to_threaded_drive() {
        let (mut plan, mut rows) = fixture();
        plan.action = ReclaimMediaAction::Load;
        plan.destination = plan.source;
        rows[0].accessible = Some(false);
        assert!(plan.condition(&plan.map, &rows, Some(true), true).unwrap());
        plan.action = ReclaimMediaAction::Unload;
        assert!(plan.condition(&plan.map, &rows, Some(true), false).unwrap());
        assert!(!plan.condition(&plan.map, &rows, None, true).unwrap());
        assert!(plan.condition(&plan.map, &rows, Some(false), true).unwrap());
    }
}
