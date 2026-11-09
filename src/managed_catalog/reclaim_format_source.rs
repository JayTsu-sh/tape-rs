//! Original physical cartridge identity from a guarded current-source capture.
//! Saved MAM facts alone are not execution permission or a fresh observation.
use super::*;
use crate::ltfs::mam::MediumIdentity;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in super::super) struct FormatSourceFact {
    parent: String,
    inventory: [u8; 32],
    head: Vec<u8>,
    identity: Vec<u8>,
    dp_eod: u64,
    label: Vec<u8>,
    pub cartridge: MediumIdentity,
}
impl FormatSourceFact {
    pub(in super::super) fn require_origin(&self, volume: Uuid, parent: &str) -> Result<()> {
        self.cartridge.validate().map_err(cache)?;
        let head = active_volume::decode_index(volume, &self.head)?;
        let identity = crate::ltfs::recovery::IndexSourceIdentity::from_bytes(&self.identity)
            .map_err(cache)?;
        let label = crate::ltfs::label::LtfsLabel::parse(&self.label).map_err(cache)?;
        if ActiveIndexIdentity::from(identity.head()) != head
            || label.volume_uuid != volume
            || label.location != crate::ltfs::label::PART_INDEX
            || self.dp_eod <= head.location.start_block
        {
            return Err(CatalogError::Corrupt("格式化来源链、标签或原 DP 边界无效"));
        }
        if self.parent != parent {
            return Err(CatalogError::Corrupt("格式化介质事实改变原父身份"));
        }
        Ok(())
    }
    pub(in super::super) fn identity(&self) -> Result<crate::ltfs::recovery::IndexSourceIdentity> {
        crate::ltfs::recovery::IndexSourceIdentity::from_bytes(&self.identity).map_err(cache)
    }
    pub(in super::super) fn old_label(&self) -> Result<crate::ltfs::label::LtfsLabel> {
        crate::ltfs::label::LtfsLabel::parse(&self.label).map_err(cache)
    }
    pub(in super::super) fn dp_eod(&self) -> u64 {
        self.dp_eod
    }
    pub(in super::super) fn require_capture(&self, actual: &CapturedIndexSource) -> Result<()> {
        let inventory = ReclaimSourceInventory::from_captured(actual)?;
        if inventory.summary.digest != self.inventory
            || inventory.summary.head != self.head
            || actual.identity().map_err(cache)?.to_bytes() != self.identity
            || inventory.summary.dp_eod != self.dp_eod
        {
            return Err(CatalogError::Invalid("格式化实际源索引改变原完整清单"));
        }
        Ok(())
    }
    pub(in super::super) async fn require_at(
        &self,
        tr: &Transaction,
        reservation: &ReclaimReservation,
    ) -> Result<()> {
        self.require_origin(reservation.spec().volume(), &reservation.fingerprint())?;
        let manifest = ledger::require_complete_at(tr, reservation).await?;
        let (_, index) = source_state_at(tr, reservation).await?;
        let mut head = Vec::new();
        active_volume::encode_index(&mut head, index);
        if self.inventory != manifest.summary.digest
            || self.head != manifest.summary.head
            || self.head != head
        {
            return Err(CatalogError::Corrupt("格式化原始介质事实与封存源清单不同"));
        }
        Ok(())
    }
}
#[derive(Debug, PartialEq, Eq)]
pub struct ReclaimFormatSourceEvidence {
    pub(in super::super) owner: u64,
    pub(in super::super) fact: FormatSourceFact,
}
impl ReclaimFormatSourceEvidence {
    pub(crate) fn from_read(
        reservation: &ReclaimReservation,
        owner: u64,
        source: &CapturedIndexSource,
        cartridge: MediumIdentity,
        label: &crate::ltfs::label::LtfsLabel,
    ) -> Result<Self> {
        let inventory = ReclaimSourceInventory::from_captured(source)?;
        if owner == 0
            || inventory.summary.volume != reservation.spec().volume().to_string()
            || reservation.intent()["index"] != json!(inventory.summary.head)
        {
            return Err(CatalogError::Invalid("格式化来源不是原实际捕获源卷"));
        }
        let fact = FormatSourceFact {
            parent: reservation.fingerprint(),
            inventory: inventory.summary.digest,
            head: inventory.summary.head,
            cartridge,
            identity: source.identity().map_err(cache)?.to_bytes(),
            dp_eod: inventory.summary.dp_eod,
            label: label.to_xml().map_err(cache)?.to_vec(),
        };
        fact.require_origin(reservation.spec().volume(), &reservation.fingerprint())?;
        Ok(Self { owner, fact })
    }
}
