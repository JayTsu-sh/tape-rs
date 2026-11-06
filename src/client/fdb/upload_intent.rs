//! Durable logical intent exists before the server selects a physical target.
use super::*;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OriginalUpload {
    version: u8,
    pub library: String,
    pub pool: Uuid,
    pub path: String,
    pub namespace: Uuid,
    pub parent: Option<Uuid>,
    pub inode: Option<Uuid>,
    pub previous: Option<u64>,
    pub operation: Uuid,
    pub size: u64,
    pub sha256: String,
    pub barcode: Option<String>,
}

impl OriginalUpload {
    fn value(&self) -> Value {
        serde_json::json!({"version":self.version,"library":self.library,"pool":self.pool.to_string(),
            "path":self.path,"namespace":self.namespace.to_string(),"parent":self.parent.map(|id|id.to_string()),"inode":self.inode.map(|id|id.to_string()),
            "previous":self.previous,"operation":self.operation.to_string(),"size":self.size,"sha256":self.sha256,
            "barcode":self.barcode})
    }

    fn parse(value: &Value) -> Result<Self> {
        let bad = || invalid("保存的原上传意图字段无效");
        let fields = [
            "version",
            "library",
            "pool",
            "path",
            "namespace",
            "parent",
            "inode",
            "previous",
            "operation",
            "size",
            "sha256",
            "barcode",
        ];
        if !value
            .as_object()
            .is_some_and(|v| v.len() == fields.len() && fields.iter().all(|k| v.contains_key(*k)))
            || value["version"] != 2
        {
            return Err(bad());
        }
        let text = |key: &str| value[key].as_str().ok_or_else(bad);
        let id = |key: &str| {
            let raw = text(key)?;
            let id = Uuid::parse_str(raw).map_err(|_| bad())?;
            if id.to_string() != raw {
                return Err(bad());
            }
            Ok(id)
        };
        let original = Self {
            version: 2,
            library: text("library")?.into(),
            pool: id("pool")?,
            path: text("path")?.into(),
            namespace: id("namespace")?,
            parent: if value["parent"].is_null() {
                None
            } else {
                Some(id("parent")?)
            },
            inode: if value["inode"].is_null() {
                None
            } else {
                Some(id("inode")?)
            },
            previous: if value["previous"].is_null() {
                None
            } else {
                Some(value["previous"].as_u64().ok_or_else(bad)?)
            },
            operation: id("operation")?,
            size: value["size"].as_u64().ok_or_else(bad)?,
            sha256: text("sha256")?.into(),
            barcode: if value["barcode"].is_null() {
                None
            } else {
                Some(text("barcode")?.into())
            },
        };
        original.validate()?;
        Ok(original)
    }

    pub fn new(
        library: &str,
        pool: Uuid,
        path: &str,
        source: &Path,
        condition: UploadCondition,
        barcode: Option<&str>,
    ) -> Result<Self> {
        let conditions::ConditionParts {
            namespace,
            parent,
            inode,
            previous,
        } = condition.parts()?;
        validate_path(path)?;
        Client::new(Vec::<String>::new()).with_library(library)?;
        if pool.is_nil() {
            return Err(invalid("pool UUID 无效"));
        }
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(source)?;
        let metadata = input.metadata()?;
        if !metadata.is_file() {
            return Err(invalid("上传源必须是普通文件"));
        }
        let size = metadata.len();
        let mut hash = Sha256::new();
        let mut bytes = 0u64;
        let mut buffer = [0; 64 * 1024];
        let mut bounded = (&mut input).take(
            size.checked_add(1)
                .ok_or_else(|| invalid("原正文长度无效"))?,
        );
        loop {
            let n = bounded.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            bytes += n as u64;
            hash.update(&buffer[..n]);
        }
        if bytes != size {
            return Err(ClientError::Verification("读取上传源时长度变化".into()));
        }
        let value = Self {
            version: 2,
            library: library.into(),
            pool,
            path: path.into(),
            namespace,
            parent,
            inode,
            previous,
            operation: Uuid::new_v4(),
            size,
            sha256: format!("{:x}", hash.finalize()),
            barcode: barcode.map(str::to_owned),
        };
        value.validate()?;
        Ok(value)
    }

    pub(super) fn condition(&self) -> Result<UploadCondition> {
        match (self.parent, self.inode, self.previous) {
            (Some(parent), None, None) => Ok(UploadCondition::Create {
                namespace: self.namespace,
                parent,
            }),
            (None, Some(inode), Some(version)) => Ok(UploadCondition::Replace(FileVersion::Fdb {
                namespace: self.namespace,
                inode,
                version,
            })),
            _ => Err(invalid("保存的上传条件不完整")),
        }
    }

    fn validate(&self) -> Result<()> {
        self.condition()?.parts()?;
        validate_path(&self.path)?;
        Client::new(Vec::<String>::new()).with_library(&self.library)?;
        if self.version != 2
            || self.pool.is_nil()
            || self.operation.is_nil()
            || self.size == u64::MAX
            || self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.barcode.as_ref().is_some_and(|s| {
                s.is_empty()
                    || s.len() > 64
                    || !s.bytes().all(|b| b.is_ascii_graphic() && b != b'/')
            })
        {
            return Err(invalid("保存的原上传意图无效"));
        }
        Ok(())
    }

    pub fn verify(&self, prepared: &PreparedUpload) -> Result<()> {
        self.condition()?.verify(prepared)?;
        let identity = prepared.identity();
        if prepared.value["library"] != self.library
            || prepared.value["pool"] != self.pool.to_string()
            || prepared.value["path"] != URL_SAFE_NO_PAD.encode(&self.path)
            || identity["operation"] != self.operation.to_string()
            || identity["size"] != self.size
            || identity["sha256"] != self.sha256
            || identity["barcode"] != serde_json::json!(self.barcode)
        {
            return Err(ClientError::Verification(
                "上传准备应答与保存的原意图不一致".into(),
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    pub fn fixture(prepared: &PreparedUpload) -> Self {
        let id = prepared.identity();
        let previous = id["previous"].as_u64();
        Self {
            version: 2,
            library: prepared.value["library"].as_str().unwrap().into(),
            pool: Uuid::parse_str(prepared.value["pool"].as_str().unwrap()).unwrap(),
            path: String::from_utf8(
                URL_SAFE_NO_PAD
                    .decode(prepared.value["path"].as_str().unwrap())
                    .unwrap(),
            )
            .unwrap(),
            namespace: Uuid::parse_str(id["namespace"].as_str().unwrap()).unwrap(),
            parent: if previous.is_none() {
                Some(Uuid::parse_str(id["parent"].as_str().unwrap()).unwrap())
            } else {
                None
            },
            inode: previous.map(|_| Uuid::parse_str(id["file"].as_str().unwrap()).unwrap()),
            previous,
            operation: Uuid::parse_str(id["operation"].as_str().unwrap()).unwrap(),
            size: id["size"].as_u64().unwrap(),
            sha256: id["sha256"].as_str().unwrap().into(),
            barcode: id["barcode"].as_str().map(str::to_owned),
        }
    }
}

fn record_digest(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::new()
            .chain_update(b"tape-rs/local-upload/2")
            .chain_update(value.to_string())
            .finalize()
    )
}

pub(super) fn encode(original: &OriginalUpload, prepared: Option<&PreparedUpload>) -> Vec<u8> {
    let mut value = serde_json::json!({"version":2,"original":original.value(),"prepared":prepared.map(|p|&p.value)});
    value["record_digest"] = Value::String(record_digest(&value));
    format!("{value}\n").into_bytes()
}

pub(super) fn decode(bytes: &[u8]) -> Result<(OriginalUpload, Option<PreparedUpload>)> {
    let mut value: Value =
        serde_json::from_slice(bytes).map_err(|_| invalid("上传请求头被截断或损坏"))?;
    if bytes.strip_suffix(b"\n").unwrap_or(bytes) != value.to_string().as_bytes()
        || !value.as_object().is_some_and(|v| {
            v.len() == 4
                && ["version", "original", "prepared", "record_digest"]
                    .iter()
                    .all(|k| v.contains_key(*k))
        })
        || value["version"] != 2
    {
        return Err(invalid("上传请求头必须是当前完整状态的规范 JSON"));
    }
    let digest = value
        .as_object_mut()
        .unwrap()
        .remove("record_digest")
        .unwrap();
    if digest != record_digest(&value) {
        return Err(invalid("上传请求头摘要不一致"));
    }
    let original = OriginalUpload::parse(&value["original"])?;
    let prepared = if value["prepared"].is_null() {
        None
    } else {
        let prepared = PreparedUpload::parse(value["prepared"].clone())?;
        original.verify(&prepared)?;
        Some(prepared)
    };
    Ok((original, prepared))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_state_rejects_damage_and_never_treats_missing_selection_as_a_draft() {
        let request = super::super::tests::request();
        let original = OriginalUpload::fixture(&request);
        let draft = encode(&original, None);
        let (saved, prepared) = decode(&draft).unwrap();
        assert_eq!(saved, original);
        assert!(prepared.is_none());
        let ready = encode(&original, Some(&request));
        let (saved, prepared) = decode(&ready).unwrap();
        assert_eq!(saved, original);
        assert_eq!(prepared.unwrap().value, request.value);
        for end in 0..ready.len() - 1 {
            assert!(decode(&ready[..end]).is_err(), "truncated {end}");
        }
        for index in 0..ready.len() - 1 {
            let mut damaged = ready.clone();
            damaged[index] ^= 1;
            assert!(decode(&damaged).is_err(), "damaged {index}");
        }
        let mut value: Value = serde_json::from_slice(&ready).unwrap();
        value["prepared"] = Value::Null;
        assert!(decode(value.to_string().as_bytes()).is_err());
        value.as_object_mut().unwrap().remove("prepared");
        assert!(decode(value.to_string().as_bytes()).is_err());
        // The old physical-only header cannot be interpreted as a local draft.
        assert!(decode(request.value.to_string().as_bytes()).is_err());
        value = serde_json::from_slice(&draft).unwrap();
        value["original"]["unexpected"] = serde_json::json!(true);
        value.as_object_mut().unwrap().remove("record_digest");
        value["record_digest"] = Value::String(record_digest(&value));
        assert!(decode(value.to_string().as_bytes()).is_err());
        for parent in [Value::Null, serde_json::json!(Uuid::nil().to_string())] {
            value = serde_json::from_slice(&draft).unwrap();
            value["original"]["parent"] = parent;
            value.as_object_mut().unwrap().remove("record_digest");
            value["record_digest"] = Value::String(record_digest(&value));
            assert!(decode(value.to_string().as_bytes()).is_err());
        }
        value = serde_json::from_slice(&draft).unwrap();
        value["original"].as_object_mut().unwrap().remove("parent");
        value["original"]["version"] = serde_json::json!(1);
        value.as_object_mut().unwrap().remove("record_digest");
        value["record_digest"] = Value::String(record_digest(&value));
        assert!(decode(value.to_string().as_bytes()).is_err());
    }
}
