//! Strict original physical plan. Reconciliation never chooses a new target.
use super::*;
use crate::changer::element::ElementAddressMap;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveLocationKind {
    Storage,
    ImportExport,
    Drive,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveLocation {
    pub address: u16,
    pub kind: MoveLocationKind,
    pub drive_serial: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovePlan {
    pub version: u8,
    pub barcode: String,
    pub captured_at: String,
    pub map: ElementAddressMap,
    pub source: MoveLocation,
    pub destination: MoveLocation,
    pub home_address: Option<u16>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovePhase {
    Prepared,
    Unloading,
    Moving,
    Confirmed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveExecution {
    pub version: u8,
    pub plan: MovePlan,
    pub phase: MovePhase,
    pub error: Option<String>,
}
fn contains(start: u16, count: u16, address: u16) -> bool {
    (u32::from(start)..u32::from(start) + u32::from(count)).contains(&u32::from(address))
}
impl MovePlan {
    pub fn parse(value: Value) -> Result<Self> {
        let plan: Self = serde_json::from_value(value.clone())
            .map_err(|_| CatalogError::Invalid("搬带计划字段或类型无效"))?;
        if plan.value() != value {
            return Err(CatalogError::Invalid("搬带计划字段缺失"));
        }
        plan.validate()?;
        Ok(plan)
    }
    pub fn value(&self) -> Value {
        serde_json::to_value(self).expect("move plan serializes")
    }
    pub fn validate(&self) -> Result<()> {
        validate_barcode(&self.barcode)?;
        if self.version != 1 || !library_observations::valid_time(&self.captured_at) {
            return Err(CatalogError::Invalid("搬带计划版本或原采样时间无效"));
        }
        let m = &self.map;
        let ranges = [
            (m.transport_start, m.transport_count),
            (m.storage_start, m.storage_count),
            (m.ie_start, m.ie_count),
            (m.dt_start, m.dt_count),
        ];
        if m.transport_count == 0 || m.storage_count == 0 {
            return Err(CatalogError::Invalid("搬带地址map缺少机械手或存储槽"));
        }
        for (i, &(start, count)) in ranges.iter().enumerate() {
            let (start, end) = (u32::from(start), u32::from(start) + u32::from(count));
            if end > 65536 {
                return Err(CatalogError::Invalid("搬带地址map超过16位地址空间"));
            }
            if count > 0
                && ranges[..i].iter().any(|&(s, c)| {
                    c > 0 && start < u32::from(s) + u32::from(c) && u32::from(s) < end
                })
            {
                return Err(CatalogError::Invalid("搬带地址map元素空间重叠"));
            }
        }
        for location in [&self.source, &self.destination] {
            let valid = match location.kind {
                MoveLocationKind::Storage => {
                    contains(m.storage_start, m.storage_count, location.address)
                }
                MoveLocationKind::ImportExport => {
                    contains(m.ie_start, m.ie_count, location.address)
                }
                MoveLocationKind::Drive => contains(m.dt_start, m.dt_count, location.address),
            };
            if !valid
                || (location.kind == MoveLocationKind::Drive) != location.drive_serial.is_some()
            {
                return Err(CatalogError::Invalid("搬带位置与map或驱动身份不符"));
            }
            if let Some(serial) = &location.drive_serial {
                validate_physical_serial(serial)?;
            }
        }
        if self.destination.kind == MoveLocationKind::Drive
            || self
                .home_address
                .is_some_and(|a| !contains(m.storage_start, m.storage_count, a))
        {
            return Err(CatalogError::Invalid("搬带目标或home地址无效"));
        }
        Ok(())
    }
    pub(super) fn matches(&self, request: &ManagementRequest) -> Result<()> {
        self.validate()?;
        let change = &request.value()["change"];
        let destination = change["destination"].as_str();
        if change["kind"] != "media-move"
            || change["barcode"] != self.barcode
            || match destination {
                Some("homeslot") => {
                    self.destination.kind != MoveLocationKind::Storage
                        || self.home_address != Some(self.destination.address)
                }
                Some("ieslot") => self.destination.kind != MoveLocationKind::ImportExport,
                _ => true,
            }
        {
            return Err(CatalogError::Invalid("搬带计划与原请求条码或目标不符"));
        }
        Ok(())
    }
}
impl MoveExecution {
    pub fn parse(value: Value, request: &ManagementRequest) -> Result<Self> {
        let execution: Self = serde_json::from_value(value.clone())
            .map_err(|_| CatalogError::Corrupt("搬带检查点字段或类型无效"))?;
        if serde_json::to_value(&execution).expect("execution serializes") != value {
            return Err(CatalogError::Corrupt("搬带检查点缺少字段"));
        }
        execution
            .plan
            .matches(request)
            .map_err(|_| CatalogError::Corrupt("搬带检查点计划与原请求不符"))?;
        if execution.version != 1
            || execution
                .error
                .as_ref()
                .is_some_and(|s| s.is_empty() || s.len() > 2048)
            || (execution.phase == MovePhase::Unloading
                && execution.plan.source.kind != MoveLocationKind::Drive)
            || (execution.phase == MovePhase::Moving
                && execution.plan.source == execution.plan.destination)
        {
            return Err(CatalogError::Corrupt("搬带检查点版本、阶段或诊断无效"));
        }
        Ok(execution)
    }
    pub fn value(&self) -> Value {
        serde_json::to_value(self).expect("execution serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plan() -> MovePlan {
        MovePlan {
            version: 1,
            barcode: "TEST01L8".into(),
            captured_at: "2026-11-06T00:00:00.000000000Z".into(),
            map: ElementAddressMap {
                transport_start: 0,
                transport_count: 1,
                storage_start: 1001,
                storage_count: 35,
                ie_start: 101,
                ie_count: 5,
                dt_start: 1,
                dt_count: 2,
            },
            source: MoveLocation {
                address: 1001,
                kind: MoveLocationKind::Storage,
                drive_serial: None,
            },
            destination: MoveLocation {
                address: 101,
                kind: MoveLocationKind::ImportExport,
                drive_serial: None,
            },
            home_address: Some(1001),
        }
    }
    #[test]
    fn move_plan_rejects_wrong_address_spaces_missing_fields_and_plan_drift() {
        let p = plan();
        assert_eq!(MovePlan::parse(p.value()).unwrap(), p);
        for field in ["home_address", "barcode", "captured_at", "map"] {
            let mut value = p.value();
            value.as_object_mut().unwrap().remove(field);
            assert!(MovePlan::parse(value).is_err());
        }
        let mut value = p.value();
        value["unexpected"] = json!(true);
        assert!(MovePlan::parse(value).is_err());
        let mut bad = p.clone();
        bad.map.ie_start = 1001;
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.map.storage_start = 65535;
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.destination.address = 1;
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.source.kind = MoveLocationKind::Drive;
        bad.source.address = 1;
        assert!(bad.validate().is_err());
        bad.source.drive_serial = Some("DRIVE01".into());
        bad.validate().unwrap();
        let mut bad = p.clone();
        bad.home_address = Some(101);
        assert!(bad.validate().is_err());
        let ns = Uuid::new_v4();
        let r=ManagementRequest::parse(json!({"version":1,"library":"LIB01","namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-move","pool":null,"volume":null,"revision":null,"barcode":"TEST01L8","destination":"ieslot"}})).unwrap();
        p.matches(&r).unwrap();
        let mut bad = p.clone();
        bad.barcode = "TEST02L8".into();
        assert!(bad.matches(&r).is_err());
    }
}
