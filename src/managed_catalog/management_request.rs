//! Validated management request identities shared by future CLI/HTTP task admission.
//! Decoding never persists intent, issues SCSI commands, or grants execution permission.
use super::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

fn fields(value: &Value, names: &[&str]) -> Result<()> {
    if !value
        .as_object()
        .is_some_and(|v| v.len() == names.len() && v.keys().all(|k| names.contains(&k.as_str())))
    {
        return Err(CatalogError::Invalid("管理请求字段不匹配"));
    }
    Ok(())
}
fn id(value: &Value, name: &str) -> Result<Uuid> {
    value[name]
        .as_str()
        .and_then(|s| {
            Uuid::parse_str(s)
                .ok()
                .filter(|id| !id.is_nil() && id.to_string() == s)
        })
        .ok_or(CatalogError::Invalid("管理 UUID 无效或不规范"))
}
fn text<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value[name]
        .as_str()
        .ok_or(CatalogError::Invalid("管理文本字段无效"))
}
fn number(value: &Value, name: &str) -> Result<u64> {
    value[name]
        .as_u64()
        .ok_or(CatalogError::Invalid("管理数值字段无效"))
}
fn revision(value: &Value) -> Result<u64> {
    number(value, "revision")?
        .checked_add(1)
        .ok_or(CatalogError::Invalid("管理修订号溢出"))
}
fn valid_drive(serial: &str) -> Result<()> {
    validate_serial(serial)?;
    if serial.contains('/') || serial.chars().any(char::is_whitespace) {
        return Err(CatalogError::Invalid("驱动器序列号无效"));
    }
    Ok(())
}
fn patch(value: &Value, pool: bool) -> Result<()> {
    let fields = value
        .as_object()
        .filter(|v| !v.is_empty())
        .ok_or(CatalogError::Invalid("策略 patch 必须是非空对象"))?;
    for (key, value) in fields {
        let good = match (pool, key.as_str()) {
            (true, "name") => value
                .as_str()
                .is_some_and(|name| validate_pool_name(name).is_ok()),
            (true, "file_limit") => value.as_u64().is_some_and(|n| n > 0),
            (true, "media_restriction") => value.as_str().is_some_and(valid_media_restriction),
            (true, "mount_limit" | "low_space_warning_threshold") => {
                value.as_u64().is_some_and(|n| n <= 32767)
            }
            (true, "low_space_warning_enabled" | "no_space_warning_enabled")
            | (false, "enabled") => value.is_boolean(),
            (false, "role") => value.as_str().is_some_and(|v| {
                !v.is_empty()
                    && v.len() <= 3
                    && v.bytes().all(|b| b"mrg".contains(&b))
                    && v.bytes().collect::<BTreeSet<_>>().len() == v.len()
            }),
            _ => false,
        };
        if !good {
            return Err(CatalogError::Invalid("策略字段、类型或范围无效"));
        }
    }
    Ok(())
}
fn validate_change(value: &Value) -> Result<()> {
    match text(value, "kind")? {
        "tape" => {
            fields(
                value,
                &["kind", "pool", "volume", "barcode", "revision", "patch"],
            )?;
            id(value, "pool")?;
            id(value, "volume")?;
            validate_barcode(text(value, "barcode")?)?;
            if number(value, "revision")? == 0 {
                return Err(CatalogError::Invalid("磁带修订号不能为零"));
            }
            revision(value)?;
            let patch = value["patch"]
                .as_object()
                .filter(|v| !v.is_empty())
                .ok_or(CatalogError::Invalid("磁带策略 patch 不能为空"))?;
            for (name, value) in patch {
                let valid = match name.as_str() {
                    "online" => value.is_boolean(),
                    "blacklisted" => value.is_boolean(),
                    "offline_message" => valid_offline_message(value),
                    _ => false,
                };
                if !valid {
                    return Err(CatalogError::Invalid(
                        "磁带策略仅支持 online=false、blacklisted 布尔值及 offline_message；上线须使用 tape online",
                    ));
                }
            }
        }
        "library-rescan" => fields(value, &["kind"])?,
        "media-assign" => {
            fields(
                value,
                &[
                    "kind",
                    "pool",
                    "barcode",
                    "volume",
                    "confirm_format",
                    "force_format",
                ],
            )?;
            id(value, "pool")?;
            id(value, "volume")?;
            validate_barcode(text(value, "barcode")?)?;
            if value["confirm_format"] != true || !value["force_format"].is_boolean() {
                return Err(CatalogError::Invalid(
                    "分配必须明确确认格式化及 force_format 布尔值",
                ));
            }
        }
        "media-unassign" | "media-validate" | "media-online" => {
            if value["kind"] == "media-online" {
                revision(value)?;
            }
            fields(value, &["kind", "pool", "barcode", "volume", "revision"])?;
            id(value, "pool")?;
            id(value, "volume")?;
            validate_barcode(text(value, "barcode")?)?;
            if number(value, "revision")? == 0 {
                return Err(CatalogError::Invalid("磁带修订号不能为零"));
            }
        }
        "media-move" => {
            fields(
                value,
                &[
                    "kind",
                    "pool",
                    "barcode",
                    "volume",
                    "revision",
                    "destination",
                ],
            )?;
            validate_barcode(text(value, "barcode")?)?;
            if !matches!(text(value, "destination")?, "homeslot" | "ieslot") {
                return Err(CatalogError::Invalid("移动目的地必须为 homeslot 或 ieslot"));
            }
            if value["pool"].is_null() {
                if !value["volume"].is_null() || !value["revision"].is_null() {
                    return Err(CatalogError::Invalid("未分配磁带不得指定卷或修订号"));
                }
            } else {
                id(value, "pool")?;
                id(value, "volume")?;
                if number(value, "revision")? == 0 {
                    return Err(CatalogError::Invalid("磁带修订号不能为零"));
                }
            }
        }
        "media-import" => {
            fields(
                value,
                &["kind", "pool", "barcode", "volume", "path", "merge_tapes"],
            )?;
            let pool = id(value, "pool")?;
            validate_barcode(text(value, "barcode")?)?;
            let path = text(value, "path")?;
            pool_catalog_path(pool, path.as_bytes())?;
            if !value["merge_tapes"].is_boolean() {
                return Err(CatalogError::Invalid("导入 merge_tapes 必须为布尔值"));
            }
            if value["merge_tapes"] == false {
                pool_catalog_path(
                    pool,
                    format!("{}/{}", path.trim_end_matches('/'), text(value, "barcode")?)
                        .as_bytes(),
                )?;
            }
            if !value["volume"].is_null() {
                id(value, "volume")?;
            }
        }
        "pool" => {
            fields(value, &["kind", "id", "revision", "patch"])?;
            id(value, "id")?;
            if number(value, "revision")? == 0 {
                return Err(CatalogError::Invalid("池修订号不能为零"));
            }
            revision(value)?;
            patch(&value["patch"], true)?;
        }
        "pool-delete" => {
            fields(value, &["kind", "id", "revision"])?;
            id(value, "id")?;
            if number(value, "revision")? == 0 {
                return Err(CatalogError::Invalid("池修订号不能为零"));
            }
            revision(value)?;
        }
        "drive" => {
            fields(value, &["kind", "serial", "revision", "patch"])?;
            valid_drive(text(value, "serial")?)?;
            revision(value)?;
            patch(&value["patch"], false)?;
        }
        _ => return Err(CatalogError::Invalid("未知管理操作")),
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ManagementRequest {
    value: Value,
    namespace: Uuid,
    epoch: u64,
    operation: Uuid,
}
impl ManagementRequest {
    /// Decode and validate admission shape; does not verify live state or execution authority.
    pub fn parse(value: Value) -> Result<Self> {
        let request = Self::decode_envelope(value)?;
        validate_change(&request.value["change"])?;
        validate_admission_change(&request.value["change"])?;
        Ok(request)
    }
    fn decode_envelope(value: Value) -> Result<Self> {
        fields(
            &value,
            &[
                "version",
                "library",
                "namespace",
                "epoch",
                "operation",
                "change",
            ],
        )?;
        if value["version"] != 1 || value.to_string().len() > 4096 {
            return Err(CatalogError::Invalid("管理请求版本或大小无效"));
        }
        validate_physical_serial(text(&value, "library")?)?;
        let namespace = id(&value, "namespace")?;
        let operation = id(&value, "operation")?;
        let epoch = number(&value, "epoch")?;
        if epoch == 0 {
            return Err(CatalogError::Invalid("管理请求代次不能为零"));
        }
        if !value["change"].is_object()
            || !value["change"]["kind"].as_str().is_some_and(|kind| {
                !kind.is_empty()
                    && kind.len() <= 63
                    && kind.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return Err(CatalogError::Invalid("管理变更类型或结构无效"));
        }
        Ok(Self {
            value,
            namespace,
            epoch,
            operation,
        })
    }
    pub fn value(&self) -> &Value {
        &self.value
    }
    pub fn namespace(&self) -> Uuid {
        self.namespace
    }
    pub fn epoch(&self) -> u64 {
        self.epoch
    }
    pub fn operation(&self) -> Uuid {
        self.operation
    }
    pub fn fingerprint(&self) -> String {
        format!("{:x}", Sha256::digest(self.value.to_string().as_bytes()))
    }
}
fn validate_admission_change(change: &Value) -> Result<()> {
    if change["kind"] == "tape" && change["patch"]["online"] == true {
        return Err(CatalogError::Invalid(
            "online=true 必须通过 tape online 的实带核验任务",
        ));
    }
    Ok(())
}
fn valid_offline_message(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 63
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_- ".contains(&b))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn empty_pool_delete_request_is_strict_and_cannot_force_data_removal() {
        let base = json!({"version":1,"library":"SIM","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"pool-delete","id":Uuid::new_v4().to_string(),"revision":1}});
        assert!(ManagementRequest::parse(base.clone()).is_ok());
        for field in ["kind", "id", "revision"] {
            let mut bad = base.clone();
            bad["change"].as_object_mut().unwrap().remove(field);
            assert!(ManagementRequest::parse(bad).is_err());
        }
        for revision in [json!(0), json!(u64::MAX), json!("1")] {
            let mut bad = base.clone();
            bad["change"]["revision"] = revision;
            assert!(ManagementRequest::parse(bad).is_err());
        }
        for (field, value) in [
            ("force", json!(true)),
            ("barcode", json!("ANY000L8")),
            ("id", json!(Uuid::nil().to_string())),
        ] {
            let mut bad = base.clone();
            bad["change"][field] = value;
            assert!(ManagementRequest::parse(bad).is_err());
        }
    }

    #[test]
    fn pool_name_patch_uses_the_same_name_bounds_as_creation() {
        let ns = Uuid::new_v4();
        let pool = Uuid::new_v4();
        let make = |name: Value| json!({"version":1,"library":"SIM","namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"pool","id":pool.to_string(),"revision":1,"patch":{"name":name}}});
        for name in [
            "archive".into(),
            " ".into(),
            "档案".into(),
            "x".repeat(255),
            "汉".repeat(85),
        ] {
            assert!(
                ManagementRequest::parse(make(json!(name))).is_ok(),
                "{name}"
            );
        }
        for name in [
            Value::Null,
            json!(1),
            json!(true),
            json!({}),
            json!([]),
            json!(""),
            json!("/a"),
            json!("a\0b"),
            json!("x".repeat(256)),
            json!("汉".repeat(86)),
        ] {
            assert!(ManagementRequest::parse(make(name)).is_err());
        }
    }

    fn request(namespace: Uuid, serial: &str, change: Value) -> ManagementRequest {
        ManagementRequest::parse(json!({"version":1,"library":serial,"namespace":namespace.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change})).unwrap()
    }

    #[test]
    fn tape_requests_require_complete_binding_revision_and_boolean_policy() {
        let base = json!({"kind":"tape","pool":Uuid::new_v4().to_string(),"volume":Uuid::new_v4().to_string(),"barcode":"TAPE001L8","revision":1,"patch":{"online":false,"blacklisted":true}});
        assert!(validate_change(&base).is_ok());
        for field in ["pool", "volume", "barcode", "revision", "patch"] {
            let mut value = base.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(validate_change(&value).is_err(), "missing {field}");
        }
        for patch in [
            json!({}),
            json!({"online":"false"}),
            json!({"recall_only":"true"}),
            json!({"offline_message":"bad\nmessage"}),
            json!({"blacklisted":0}),
            json!({"offline":true}),
            json!({"blacklist":true}),
            json!({"online":true,"enabled":true}),
            Value::Null,
        ] {
            let mut value = base.clone();
            value["patch"] = patch;
            assert!(validate_change(&value).is_err());
        }
        for revision in [0, u64::MAX] {
            let mut value = base.clone();
            value["revision"] = json!(revision);
            assert!(validate_change(&value).is_err());
        }
        for field in ["pool", "volume"] {
            let mut value = base.clone();
            value[field] = json!(Uuid::nil().to_string());
            assert!(validate_change(&value).is_err());
        }
    }

    #[test]
    fn management_request_rejects_unknown_fields_and_unsafe_patches() {
        let ns = Uuid::new_v4();
        for change in [
            json!({"kind":"pool","id":Uuid::new_v4().to_string(),"revision":1,"patch":{"display_name":"rename"}}),
            json!({"kind":"pool","id":Uuid::new_v4().to_string(),"revision":1,"patch":{"mount_limit":32768}}),
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"role":"mm"}}),
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"enabled":"false"}}),
            json!({"kind":"node","node":1,"revision":0,"group":"removed"}),
            json!({"kind":"group-create","name":"removed","id":Uuid::new_v4().to_string()}),
            json!({"kind":"group-delete","name":"removed","id":Uuid::new_v4().to_string()}),
            json!({"kind":"pool","id":Uuid::new_v4().to_string(),"revision":1,"patch":{"node_group":"removed"}}),
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"node_group":"removed"}}),
        ] {
            assert!(ManagementRequest::parse(json!({"version":1,"library":"SIM","namespace":ns.to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change})).is_err());
        }
        let good = request(
            ns,
            "SIM",
            json!({"kind":"drive","serial":"DRV","revision":0,"patch":{"enabled":false}}),
        );
        let mut bad = good.value.clone();
        bad["extra"] = json!(true);
        assert!(ManagementRequest::parse(bad).is_err());
        let mut bad = good.value;
        bad["namespace"] = json!(ns.simple().to_string());
        assert!(ManagementRequest::parse(bad).is_err());
    }

    #[test]
    fn media_requests_capture_exact_identity_and_format_confirmation() {
        let pool = Uuid::new_v4().to_string();
        let volume = Uuid::new_v4().to_string();
        let make = |change: Value| json!({"version":1,"library":"LIB-A","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":change});
        let assigned = json!({"kind":"media-assign","pool":pool,"barcode":"AA0001L8","volume":volume,"confirm_format":true,"force_format":false});
        assert!(ManagementRequest::parse(make(assigned.clone())).is_ok());
        for confirmation in [Value::Null, json!(false), json!("true"), json!(1)] {
            let mut bad = assigned.clone();
            bad["confirm_format"] = confirmation;
            assert!(ManagementRequest::parse(make(bad)).is_err());
        }
        let mut missing = assigned.clone();
        missing.as_object_mut().unwrap().remove("force_format");
        assert!(ManagementRequest::parse(make(missing)).is_err());
        for kind in ["media-unassign", "media-validate", "media-online"] {
            let base =
                json!({"kind":kind,"pool":pool,"barcode":"AA0001L8","volume":volume,"revision":1});
            assert!(ManagementRequest::parse(make(base.clone())).is_ok());
            for field in ["pool", "volume", "revision"] {
                let mut bad = base.clone();
                bad[field] = Value::Null;
                assert!(ManagementRequest::parse(make(bad)).is_err());
            }
        }
        let unassigned = json!({"kind":"media-move","pool":null,"volume":null,"revision":null,"barcode":"AA0001L8","destination":"homeslot"});
        assert!(ManagementRequest::parse(make(unassigned.clone())).is_ok());
        for (field, value) in [
            ("volume", json!(volume)),
            ("revision", json!(1)),
            ("destination", json!("drive0")),
        ] {
            let mut bad = unassigned.clone();
            bad[field] = value;
            assert!(ManagementRequest::parse(make(bad)).is_err());
        }
        let import = json!({"kind":"media-import","pool":pool,"barcode":"AA0001L8","volume":null,"path":"/archive","merge_tapes":false});
        assert!(ManagementRequest::parse(make(import.clone())).is_ok());
        for (field, value) in [
            ("path", json!("/../archive")),
            ("path", json!("archive")),
            ("path", json!("/a//b")),
            ("merge_tapes", json!("false")),
        ] {
            let mut bad = import.clone();
            bad[field] = value;
            assert!(ManagementRequest::parse(make(bad)).is_err());
        }
        assert!(ManagementRequest::parse(make(json!({"kind":"library-rescan"}))).is_ok());
        assert!(
            ManagementRequest::parse(make(json!({"kind":"library-rescan","device":"/dev/sg0"})))
                .is_err()
        );
        assert!(ManagementRequest::parse(make(json!({"kind":"reconcile"}))).is_err());
    }

    #[test]
    fn immutable_request_identity_and_tape_online_admission_are_strict() {
        let base = request(
            Uuid::new_v4(),
            "SIM",
            json!({"kind":"tape","pool":Uuid::new_v4().to_string(),"volume":Uuid::new_v4().to_string(),"barcode":"AA0001L8","revision":1,"patch":{"online":false}}),
        );
        assert_eq!(
            base.fingerprint(),
            ManagementRequest::parse(base.value().clone())
                .unwrap()
                .fingerprint()
        );
        let mut online = base.value().clone();
        online["change"]["patch"]["online"] = json!(true);
        assert!(ManagementRequest::parse(online).is_err());
        let mut changed = base.value().clone();
        changed["change"]["patch"]["blacklisted"] = json!(true);
        let changed = ManagementRequest::parse(changed).unwrap();
        assert_ne!(base.fingerprint(), changed.fingerprint());
        assert_eq!(base.namespace(), changed.namespace());
        assert_eq!(base.epoch(), changed.epoch());
        assert_eq!(base.operation(), changed.operation());
        let mut oversized = base.value().clone();
        oversized["change"]["patch"]["offline_message"] = json!("a".repeat(4096));
        assert!(ManagementRequest::parse(oversized).is_err());
        for field in ["namespace", "operation"] {
            let mut nil = base.value().clone();
            nil[field] = json!(Uuid::nil().to_string());
            assert!(ManagementRequest::parse(nil).is_err());
        }
        let mut zero = base.value().clone();
        zero["epoch"] = json!(0);
        assert!(ManagementRequest::parse(zero).is_err());
    }
}
