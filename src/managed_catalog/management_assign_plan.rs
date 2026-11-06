//! Original assignment identity and device plan, retained across unknown outcomes.
use super::*;
use crate::changer::element::ElementAddressMap;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssignPlan {
    pub version: u8,
    pub barcode: String,
    pub captured_at: String,
    pub map: ElementAddressMap,
    pub source: MoveLocation,
    pub drive: MoveLocation,
    pub home_address: u16,
    pub pool_revision: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignPhase {
    Prepared,
    Moving,
    Threading,
    Inspecting,
    Formatting,
    Confirmed,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssignExecution {
    pub version: u8,
    pub plan: AssignPlan,
    pub phase: AssignPhase,
    pub error: Option<String>,
}
impl AssignPlan {
    pub fn value(&self) -> Value {
        serde_json::to_value(self).expect("assignment plan serializes")
    }
    pub fn parse(value: Value) -> Result<Self> {
        let plan: Self = serde_json::from_value(value.clone())
            .map_err(|_| CatalogError::Invalid("分配计划字段或类型无效"))?;
        if plan.value() != value {
            return Err(CatalogError::Invalid("分配计划缺少字段"));
        }
        plan.validate()?;
        Ok(plan)
    }
    pub fn validate(&self) -> Result<()> {
        // The same location/map rules apply to both directions. The inverse
        // drive-to-home shape validates the configured destination drive too.
        for source in [&self.source, &self.drive] {
            MovePlan {
                version: self.version,
                barcode: self.barcode.clone(),
                captured_at: self.captured_at.clone(),
                map: self.map.clone(),
                source: source.clone(),
                destination: MoveLocation {
                    address: self.home_address,
                    kind: MoveLocationKind::Storage,
                    drive_serial: None,
                },
                home_address: Some(self.home_address),
            }
            .validate()?;
        }
        if self.pool_revision == 0
            || self.drive.kind != MoveLocationKind::Drive
            || !matches!(
                self.source.kind,
                MoveLocationKind::Storage | MoveLocationKind::Drive
            )
            || (self.source.kind == MoveLocationKind::Storage
                && self.source.address != self.home_address)
            || (self.source.kind == MoveLocationKind::Drive && self.source != self.drive)
        {
            return Err(CatalogError::Invalid(
                "分配源、home、原驱动器或池修订号不符",
            ));
        }
        Ok(())
    }
    pub(super) fn matches(&self, request: &ManagementRequest) -> Result<()> {
        self.validate()?;
        if request.value()["change"]["kind"] != "media-assign"
            || request.value()["change"]["barcode"] != self.barcode
        {
            return Err(CatalogError::Invalid("分配计划与原请求不符"));
        }
        Ok(())
    }
}
impl AssignExecution {
    pub fn value(&self) -> Value {
        serde_json::to_value(self).expect("assignment execution serializes")
    }
    pub fn parse(value: Value, request: &ManagementRequest) -> Result<Self> {
        let execution: Self = serde_json::from_value(value.clone())
            .map_err(|_| CatalogError::Corrupt("分配检查点字段或类型无效"))?;
        if execution.value() != value
            || execution.version != 1
            || execution
                .error
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 2048)
        {
            return Err(CatalogError::Corrupt("分配检查点版本、字段或诊断无效"));
        }
        execution
            .plan
            .matches(request)
            .map_err(|_| CatalogError::Corrupt("分配检查点偏离原计划"))?;
        Ok(execution)
    }
}
