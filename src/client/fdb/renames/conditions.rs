//! Complete original filesystem identities, independent of the FDB SDK.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenameCondition {
    pub source: FileVersion,
    pub source_parent: FileVersion,
    pub destination_parent: FileVersion,
    pub target: Option<FileVersion>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameSpec {
    pub pool: Uuid,
    pub from: String,
    pub to: String,
    pub condition: RenameCondition,
    pub no_replace: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Revision {
    pub namespace: Uuid,
    pub inode: Uuid,
    pub version: u64,
}
impl Revision {
    fn from_version(value: FileVersion) -> Result<Self> {
        let FileVersion::Fdb {
            namespace,
            inode,
            version,
        } = value
        else {
            return Err(invalid("rename 条件必须为完整 FDB 版本"));
        };
        if namespace.is_nil() || inode.is_nil() || version == 0 || version == u64::MAX {
            return Err(invalid("rename 原身份或可递增版本无效"));
        }
        Ok(Self {
            namespace,
            inode,
            version,
        })
    }
    fn parse(value: &Value) -> Result<Self> {
        if !fields(value, &["namespace", "inode", "version"]) {
            return Err(invalid("rename 原身份字段无效"));
        }
        Self::from_version(FileVersion::Fdb {
            namespace: uuid(value, "namespace")?,
            inode: uuid(value, "inode")?,
            version: value["version"]
                .as_u64()
                .ok_or_else(|| invalid("rename 原身份版本无效"))?,
        })
    }
    fn file_version(self) -> FileVersion {
        FileVersion::Fdb {
            namespace: self.namespace,
            inode: self.inode,
            version: self.version,
        }
    }
    pub(super) fn value(self) -> Value {
        serde_json::json!({"namespace":self.namespace.to_string(),"inode":self.inode.to_string(),"version":self.version})
    }
    pub(super) fn after(self) -> Value {
        Self {
            version: self.version + 1,
            ..self
        }
        .value()
    }
}
impl RenameCondition {
    pub(super) fn revisions(self) -> Result<(Revision, Revision, Revision, Option<Revision>)> {
        let source = Revision::from_version(self.source)?;
        let left = Revision::from_version(self.source_parent)?;
        let right = Revision::from_version(self.destination_parent)?;
        let target = self.target.map(Revision::from_version).transpose()?;
        if [Some(left), Some(right), target]
            .into_iter()
            .flatten()
            .any(|revision| revision.namespace != source.namespace)
        {
            return Err(invalid("rename 原两端身份必须属于同一 namespace"));
        }
        if source.inode == left.inode
            || source.inode == right.inode
            || target.is_some_and(|target| {
                [source.inode, left.inode, right.inode].contains(&target.inode)
            })
        {
            return Err(invalid("rename 原源、父目录或替换目标身份冲突"));
        }
        Ok((source, left, right, target))
    }
    pub(super) fn namespace(self) -> Result<Uuid> {
        Ok(self.revisions()?.0.namespace)
    }
    pub(super) fn value(self) -> Value {
        let (source, left, right, target) = self
            .revisions()
            .expect("validated original rename condition");
        serde_json::json!({"source":source.value(),"source_parent":left.value(),"destination_parent":right.value(),"target":target.map(Revision::value)})
    }
    pub(super) fn parse(value: &Value) -> Result<Self> {
        if !fields(
            value,
            &["source", "source_parent", "destination_parent", "target"],
        ) {
            return Err(invalid("rename 原两端条件字段无效"));
        }
        Self::from_request(value)
    }
    pub(super) fn from_request(value: &Value) -> Result<Self> {
        let condition = Self {
            source: Revision::parse(&value["source"])?.file_version(),
            source_parent: Revision::parse(&value["source_parent"])?.file_version(),
            destination_parent: Revision::parse(&value["destination_parent"])?.file_version(),
            target: if value["target"].is_null() {
                None
            } else {
                Some(Revision::parse(&value["target"])?.file_version())
            },
        };
        condition.revisions()?;
        Ok(condition)
    }
}
impl RenameSpec {
    pub fn validate(&self) -> Result<()> {
        if self.pool.is_nil() {
            return Err(invalid("rename 池 UUID 无效"));
        }
        for path in [&self.from, &self.to] {
            validate_path(path)?;
            if path == "/" || !crate::ltfs::xml_text_valid(path) {
                return Err(invalid("rename 路径无效"));
            }
        }
        let within = |path: &str, ancestor: &str| {
            path == ancestor
                || path
                    .strip_prefix(ancestor)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        };
        if within(&self.from, &self.to) || within(&self.to, &self.from) {
            return Err(invalid("rename 两端不能相同或互为祖先"));
        }
        let (_, left, right, target) = self.condition.revisions()?;
        if self.no_replace && target.is_some() {
            return Err(invalid("rename no-replace 与原替换目标冲突"));
        }
        let parent = |path: &str| path.rsplit_once('/').expect("validated path").0.to_owned();
        if (parent(&self.from) == parent(&self.to) && left != right)
            || (parent(&self.from) != parent(&self.to) && left.inode == right.inode)
        {
            return Err(invalid("rename 原父目录条件与两端路径不符"));
        }
        Ok(())
    }
    pub(super) fn value(&self) -> Value {
        serde_json::json!({"pool":self.pool.to_string(),"from":self.from,"to":self.to,"condition":self.condition.value(),"no_replace":self.no_replace})
    }
    pub(super) fn parse(value: &Value) -> Result<Self> {
        if !fields(value, &["pool", "from", "to", "condition", "no_replace"]) {
            return Err(invalid("rename 原意图字段无效"));
        }
        let spec = Self {
            pool: uuid(value, "pool")?,
            from: value["from"]
                .as_str()
                .ok_or_else(|| invalid("rename 原源路径无效"))?
                .into(),
            to: value["to"]
                .as_str()
                .ok_or_else(|| invalid("rename 原目标路径无效"))?
                .into(),
            condition: RenameCondition::parse(&value["condition"])?,
            no_replace: value["no_replace"]
                .as_bool()
                .ok_or_else(|| invalid("rename 原替换策略无效"))?,
        };
        spec.validate()?;
        Ok(spec)
    }
    pub(super) fn from_request(request: &Value) -> Result<Self> {
        let spec = Self {
            pool: uuid(request, "pool")?,
            from: request["from"]
                .as_str()
                .ok_or_else(|| invalid("rename 原源路径无效"))?
                .into(),
            to: request["to"]
                .as_str()
                .ok_or_else(|| invalid("rename 原目标路径无效"))?
                .into(),
            condition: RenameCondition::from_request(request)?,
            no_replace: request["no_replace"]
                .as_bool()
                .ok_or_else(|| invalid("rename 替换策略无效"))?,
        };
        spec.validate()?;
        Ok(spec)
    }
}
