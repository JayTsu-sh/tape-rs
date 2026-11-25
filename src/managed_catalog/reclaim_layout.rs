//! Immutable source/destination drive placement for one original reclaim.
//! The layout reserves its library's mechanical/data lane, not erase authority.
use super::*;
use crate::changer::element::{ElementAddressMap, ElementStatus, ElementType};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimDriveBinding {
    pub barcode: String,
    #[serde(with = "mechanics::uuid_text")]
    pub volume: Uuid,
    pub revision: u64,
    pub drive_serial: String,
    pub address: u16,
    pub home: u16,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReclaimLayout {
    version: u8,
    parent: String,
    pub source: ReclaimDriveBinding,
    pub destination: ReclaimDriveBinding,
    pub map: ElementAddressMap,
}
impl ReclaimLayout {
    fn validate(&self) -> Result<()> {
        if self.version != 1
            || self.parent.len() != 64
            || !self
                .parent
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.source.barcode == self.destination.barcode
            || self.source.volume == self.destination.volume
            || self.source.drive_serial == self.destination.drive_serial
            || self.source.address == self.destination.address
            || self.source.home == self.destination.home
        {
            return Err(CatalogError::Invalid(
                "回收布局需要独立的源/目的介质、两个驱动和原 home",
            ));
        }
        for binding in [&self.source, &self.destination] {
            if binding.revision == 0 {
                return Err(CatalogError::Invalid("回收布局缺少原分配修订号"));
            }
            self.plan(binding, ReclaimMediaAction::Load).validate()?;
        }
        Ok(())
    }
    pub fn plan(
        &self,
        binding: &ReclaimDriveBinding,
        action: ReclaimMediaAction,
    ) -> ReclaimMediaPlan {
        ReclaimMediaPlan {
            action,
            barcode: binding.barcode.clone(),
            volume: binding.volume,
            drive_serial: binding.drive_serial.clone(),
            source: binding.address,
            destination: binding.address,
            home: binding.home,
            map: self.map.clone(),
        }
    }
    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut raw = serde_json::to_vec(self).map_err(|e| CatalogError::Cache(e.to_string()))?;
        if raw.len() > 4096 {
            return Err(CatalogError::Invalid("回收布局超出预算"));
        }
        let sum = Sha256::new()
            .chain_update(b"tape-rs/reclaim-layout/1")
            .chain_update(&raw)
            .finalize();
        raw.extend_from_slice(&sum);
        Ok(raw)
    }
    fn decode(raw: &[u8]) -> Result<Self> {
        if !(32..=4128).contains(&raw.len()) {
            return Err(CatalogError::Corrupt("回收布局长度无效"));
        }
        let layout: Self = serde_json::from_slice(&raw[..raw.len() - 32])
            .map_err(|_| CatalogError::Corrupt("回收布局 JSON 无效"))?;
        if layout.encode()? != raw {
            return Err(CatalogError::Corrupt("回收布局完整性校验失败"));
        }
        Ok(layout)
    }
    fn require_parent(&self, reservation: &ReclaimReservation) -> Result<()> {
        self.validate()?;
        if self.parent != reservation.fingerprint()
            || self.source.barcode != reservation.spec().barcode()
            || self.source.volume != reservation.spec().volume()
            || self.source.revision != reservation.spec().revision()
        {
            return Err(CatalogError::Invalid("回收布局改变原源任务"));
        }
        Ok(())
    }
    pub(super) async fn require_resource_scope_at(
        &self,
        tr: &Transaction,
        reservation: &ReclaimReservation,
    ) -> Result<()> {
        let spec = reservation.spec();
        let ns = reservation.namespace();
        let serial = reservation.serial();
        logical_libraries::require_pool_scope_at(
            tr,
            ns,
            serial,
            spec.pool(),
            spec.logical_library(),
        )
        .await?;
        for binding in [&self.source, &self.destination] {
            logical_libraries::require_automatic_resource_claims(
                tr,
                ns,
                serial,
                spec.pool(),
                &binding.drive_serial,
                Some(binding.home),
            )
            .await?;
        }
        Ok(())
    }

    pub(super) fn require_plan(&self, plan: &ReclaimMediaPlan) -> Result<()> {
        self.validate()?;
        let binding = [&self.source, &self.destination]
            .into_iter()
            .find(|b| b.volume == plan.volume)
            .ok_or(CatalogError::Invalid("机械目标不在原回收布局中"))?;
        let expected = self.plan(binding, plan.action);
        if plan.barcode != expected.barcode
            || plan.drive_serial != expected.drive_serial
            || plan.home != expected.home
            || plan.map != expected.map
            || match plan.action {
                ReclaimMediaAction::Move => {
                    !((plan.source == binding.home && plan.destination == binding.address)
                        || (plan.source == binding.address && plan.destination == binding.home))
                }
                _ => plan.source != binding.address || plan.destination != binding.address,
            }
        {
            return Err(CatalogError::Invalid(
                "机械地址、驱动或 home 改变原回收布局",
            ));
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct ReclaimLayoutEvidence {
    owner: u64,
    layout: ReclaimLayout,
}
impl ReclaimLayoutEvidence {
    pub(crate) fn from_read(
        reservation: &ReclaimReservation,
        owner: u64,
        source: ReclaimDriveBinding,
        destination: ReclaimDriveBinding,
        map: &ElementAddressMap,
        rows: &[ElementStatus],
    ) -> Result<Self> {
        if owner == 0 {
            return Err(CatalogError::OwnerFenced);
        }
        let layout = ReclaimLayout {
            version: 1,
            parent: reservation.fingerprint(),
            source,
            destination,
            map: map.clone(),
        };
        layout.require_parent(reservation)?;
        for binding in [&layout.source, &layout.destination] {
            let media: Vec<_> = rows
                .iter()
                .filter(|r| r.full && r.volume_tag.as_deref() == Some(binding.barcode.as_str()))
                .collect();
            let drives: Vec<_> = rows
                .iter()
                .filter(|r| {
                    r.element_type == ElementType::DataTransfer
                        && r.drive_serial() == Some(binding.drive_serial.as_str())
                })
                .collect();
            if media.len() != 1
                || drives.len() != 1
                || drives[0].address != binding.address
                || media[0].exception
                || media[0].disabled
                || media[0].is_cleaning()
                || drives[0].exception
                || drives[0].disabled
            {
                return Err(CatalogError::Invalid(
                    "回收布局介质或驱动未唯一出现在实际库存中",
                ));
            }
            let media = media[0];
            let drive = drives[0];
            match media.element_type {
                ElementType::Storage
                    if media.address == binding.home
                        && !drive.full
                        && drive.volume_tag.is_none()
                        && media.accessible != Some(false)
                        && drive.accessible != Some(false) => {}
                ElementType::DataTransfer
                    if media.address == binding.address
                        && media.source_address == Some(binding.home) =>
                {
                    let homes: Vec<_> = rows
                        .iter()
                        .filter(|r| {
                            r.address == binding.home && r.element_type == ElementType::Storage
                        })
                        .collect();
                    if homes.len() != 1
                        || homes[0].full
                        || homes[0].volume_tag.is_some()
                        || homes[0].exception
                        || homes[0].disabled
                        || homes[0].accessible == Some(false)
                    {
                        return Err(CatalogError::Invalid(
                            "回收布局原 home 非空或不可访问，禁止改选其他槽位",
                        ));
                    }
                }
                _ => {
                    return Err(CatalogError::Invalid(
                        "回收布局原驱动非空、home 不符或介质不在库内存储槽",
                    ));
                }
            }
        }
        Ok(Self { owner, layout })
    }
}
fn key(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = reservation.journal_key();
    key[domain(reservation.namespace(), b'j').len() - 1] = b'O';
    key
}
pub(in crate::managed_catalog::control) fn gate(ns: Uuid, serial: &str) -> Vec<u8> {
    let mut key = management::prefix(ns, b'K');
    key.push(b'g');
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
pub(super) async fn at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<Option<ReclaimLayout>> {
    let own = tr.get(&key(reservation), false).await?;
    let global = tr
        .get(&gate(reservation.namespace(), reservation.serial()), false)
        .await?;
    match (own, global) {
        (None, None) => Ok(None),
        (Some(own), Some(global)) if own.as_ref() == global.as_ref() => {
            let layout = ReclaimLayout::decode(&own)?;
            layout.require_parent(reservation)?;
            Ok(Some(layout))
        }
        _ => Err(CatalogError::Corrupt("回收布局原目标与库屏障不一致")),
    }
}
pub(super) fn clear_at(tr: &Transaction, reservation: &ReclaimReservation) {
    tr.clear(&key(reservation));
    tr.clear(&gate(reservation.namespace(), reservation.serial()));
}
impl ManagedCatalog {
    pub async fn reclaim_layout(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
    ) -> Result<Option<ReclaimLayout>> {
        let tr = self.db.create_trx()?;
        let parent = record_at(&tr, reservation, owner).await?;
        if parent["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        let layout = at(&tr, reservation).await?;
        if let Some(layout) = &layout {
            layout.require_resource_scope_at(&tr, reservation).await?;
        }
        Ok(layout)
    }
    pub async fn prepare_reclaim_layout(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        evidence: &ReclaimLayoutEvidence,
    ) -> Result<ReclaimLayout> {
        if evidence.owner != owner {
            return Err(CatalogError::OwnerFenced);
        }
        evidence.layout.require_parent(reservation)?;
        let raw = evidence.layout.encode()?;
        self.db
            .run(|tr, _| {
                let raw = &raw;
                Box::pin(async move {
                    requests::retry_result(
                        async {
                            let parent = record_at(&tr, reservation, owner).await?;
                            if parent["owner_epoch"] != owner {
                                return Err(CatalogError::OwnerFenced);
                            }
                            evidence
                                .layout
                                .require_resource_scope_at(&tr, reservation)
                                .await?;
                            if let Some(layout) = at(&tr, reservation).await? {
                                if layout != evidence.layout {
                                    return Err(CatalogError::Invalid("不能更换原回收驱动布局"));
                                }
                                return Ok(layout);
                            }
                            if parent["phase"] != "reserved" || parent["children"] != 0 {
                                return Err(CatalogError::Invalid("回收布局必须先于迁移固定"));
                            }
                            source_state_at(&tr, reservation).await?;
                            source::require_metadata_idle_at(&tr, reservation).await?;
                            mechanics::require_parent_idle_at(&tr, reservation).await?;
                            native_device::require_library_available(
                                &tr,
                                reservation.namespace(),
                                reservation.serial(),
                            )
                            .await?;
                            let dest = &evidence.layout.destination;
                            let assignment = ManagedCatalog::verified_assignment(
                                &tr,
                                reservation.serial(),
                                &dest.barcode,
                            )
                            .await?
                            .ok_or(CatalogError::Incomplete)?;
                            if assignment.namespace != reservation.namespace()
                                || assignment.pool_id != reservation.spec().pool()
                                || assignment.volume_id != Some(dest.volume)
                                || assignment.revision != dest.revision
                                || !assignment.allows_io()
                            {
                                return Err(CatalogError::Invalid("回收布局目的不再属于原在线池"));
                            }
                            active_volume::observed_at(&tr, reservation.serial(), dest.volume)
                                .await?;
                            native_device::require_volume_quiescent_at(
                                &tr,
                                reservation.namespace(),
                                dest.volume,
                            )
                            .await?;
                            tr.set(&key(reservation), raw);
                            tr.set(&gate(reservation.namespace(), reservation.serial()), raw);
                            Ok(evidence.layout.clone())
                        }
                        .await
                        .map_err(notifications::atomic_error),
                    )
                })
            })
            .await?
    }
}

impl ManagedCatalog {
    /// Explicit parent-bound readonly access to the fixed source/destination.
    /// Ordinary library access remains closed while the layout is held.
    pub async fn require_reclaim_read_lane(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        volume: Uuid,
    ) -> Result<()> {
        let tr = self.db.create_trx()?;
        let parent = record_at(&tr, reservation, owner).await?;
        if parent["owner_epoch"] != owner {
            return Err(CatalogError::OwnerFenced);
        }
        if !matches!(parent["phase"].as_str(), Some("reserved" | "copying")) {
            return Err(CatalogError::Invalid(
                "初始化阶段不能继续读取迁移正文或节点",
            ));
        }
        source_state_at(&tr, reservation).await?;
        mechanics::require_parent_idle_at(&tr, reservation).await?;
        if tr
            .get(
                &native_device::library_gate(reservation.namespace(), reservation.serial()),
                false,
            )
            .await?
            .is_some()
            || automatic_media::pending_at(&tr, reservation.namespace(), reservation.serial())
                .await?
                .is_some()
        {
            return Err(CatalogError::PoolBusy);
        }
        if let Some(layout) = at(&tr, reservation).await?
            && volume != layout.source.volume
            && volume != layout.destination.volume
        {
            return Err(CatalogError::Invalid("读取卷不属于原固定回收布局"));
        }
        if volume != reservation.spec().volume() {
            let placement = verified_placement(&tr, volume)
                .await?
                .ok_or(CatalogError::Incomplete)?;
            if placement.namespace != reservation.namespace()
                || placement.library_serial != reservation.serial()
                || placement.pool_id != reservation.spec().pool()
            {
                return Err(CatalogError::Invalid("回收读取目的不属于原带库和池"));
            }
            let assignment =
                ManagedCatalog::verified_assignment(&tr, reservation.serial(), &placement.barcode)
                    .await?
                    .ok_or(CatalogError::Incomplete)?;
            if assignment.volume_id != Some(volume) || !assignment.allows_io() {
                return Err(CatalogError::MediaUnavailable);
            }
            active_volume::observed_at(&tr, reservation.serial(), volume).await?;
        }
        Ok(())
    }
}
