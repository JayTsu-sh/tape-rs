//! Stable identity shared by management receipts, notifications and clients.
use std::{fmt, str::FromStr};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagementTaskId {
    pub namespace: Uuid,
    pub library: String,
    pub epoch: u64,
    pub operation: Uuid,
}
impl ManagementTaskId {
    pub fn new(
        namespace: Uuid,
        library: &str,
        epoch: u64,
        operation: Uuid,
    ) -> Result<Self, &'static str> {
        if namespace.is_nil()
            || operation.is_nil()
            || epoch == 0
            || library.is_empty()
            || library.len() > 64
            || !library
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
        {
            return Err("任务身份的命名空间、带库、代次或UUID无效");
        }
        Ok(Self {
            namespace,
            library: library.into(),
            epoch,
            operation,
        })
    }
    pub fn from_receipt(value: &serde_json::Value) -> Result<Self, &'static str> {
        Self::new(
            value["namespace"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or("任务namespace缺失或无效")?,
            value["library_serial"].as_str().ok_or("任务带库缺失")?,
            value["epoch"].as_u64().ok_or("任务代次缺失")?,
            value["operation"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or("任务UUID缺失或无效")?,
        )
    }
}
impl fmt::Display for ManagementTaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}:{}",
            self.namespace, self.library, self.epoch, self.operation
        )
    }
}
impl FromStr for ManagementTaskId {
    type Err = &'static str;
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let fields: Vec<_> = text.split(':').collect();
        let [namespace, library, epoch, operation] = fields.as_slice() else {
            return Err("任务号格式为NAMESPACE:LIBRARY:EPOCH:OPERATION");
        };
        let id = Self::new(
            namespace.parse().map_err(|_| "任务namespace无效")?,
            library,
            epoch.parse().map_err(|_| "任务代次无效")?,
            operation.parse().map_err(|_| "任务UUID无效")?,
        )?;
        if id.to_string() != text {
            return Err("任务号必须使用规范UUID与代次编码");
        }
        Ok(id)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_bind_library_even_when_other_identity_fields_are_shared() {
        let namespace = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let a = ManagementTaskId::new(namespace, "LIB-A", 1, operation).unwrap();
        let b = ManagementTaskId::new(namespace, "LIB-B", 1, operation).unwrap();
        assert_ne!(a, b);
        assert_ne!(a.to_string(), b.to_string());
        assert_eq!(a.to_string().parse::<ManagementTaskId>().unwrap(), a);
        for invalid in [
            format!("{namespace}:1:{operation}"),
            format!("{namespace}:LIB-A:01:{operation}"),
            format!("{namespace}:LIB-A:0:{operation}"),
            format!("{namespace}:LIB-A:1:{operation}:extra"),
            format!("{}:LIB-A:1:{operation}", Uuid::nil()),
            format!("{namespace}:LIB/A:1:{operation}"),
        ] {
            assert!(invalid.parse::<ManagementTaskId>().is_err(), "{invalid}");
        }
    }
}
