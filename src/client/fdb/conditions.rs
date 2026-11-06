//! The caller's original logical target, captured before read-only preparation.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadCondition {
    /// Create only beneath this captured parent directory in this namespace.
    Create { namespace: Uuid, parent: Uuid },
    /// Replace this exact FDB inode revision; Raft and LTFS versions are invalid.
    Replace(FileVersion),
}

pub(super) struct ConditionParts {
    pub namespace: Uuid,
    pub parent: Option<Uuid>,
    pub inode: Option<Uuid>,
    pub previous: Option<u64>,
}

impl UploadCondition {
    pub(super) fn parts(self) -> Result<ConditionParts> {
        match self {
            Self::Create { namespace, parent } if !namespace.is_nil() && !parent.is_nil() => {
                Ok(ConditionParts {
                    namespace,
                    parent: Some(parent),
                    inode: None,
                    previous: None,
                })
            }
            Self::Replace(FileVersion::Fdb {
                namespace,
                inode,
                version,
            }) if !namespace.is_nil() && !inode.is_nil() && version > 0 && version < u64::MAX => {
                Ok(ConditionParts {
                    namespace,
                    parent: None,
                    inode: Some(inode),
                    previous: Some(version),
                })
            }
            _ => Err(invalid(
                "创建须绑定有效 namespace 与原父目录；覆盖须提供完整 FDB inode 与版本",
            )),
        }
    }

    pub(super) fn verify(self, prepared: &PreparedUpload) -> Result<()> {
        let ConditionParts {
            namespace,
            parent,
            inode,
            previous,
        } = self.parts()?;
        let identity = prepared.identity();
        if identity["namespace"] != namespace.to_string()
            || identity["previous"] != serde_json::json!(previous)
            || parent.is_some_and(|id| identity["parent"] != id.to_string())
            || inode.is_some_and(|id| identity["file"] != id.to_string())
        {
            return Err(ClientError::Verification(
                "上传准备目标与原 namespace、父目录、inode 或版本不一致".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_creation_rejects_another_parent_at_the_same_path() {
        let prepared = super::super::upload_tests::request();
        let original = super::super::upload_intent::OriginalUpload::fixture(&prepared);
        original.verify(&prepared).unwrap();
        let mut value = prepared.value.clone();
        let mut identity = prepared.identity();
        identity["parent"] = serde_json::json!(Uuid::new_v4().to_string());
        value["metadata"] = serde_json::json!(URL_SAFE_NO_PAD.encode(identity.to_string()));
        let recreated = PreparedUpload::parse(value).unwrap();
        assert!(
            matches!(
                original.verify(&recreated),
                Err(ClientError::Verification(_))
            ),
            "the original creation must not bind to a replacement parent directory"
        );
    }

    #[test]
    fn original_target_rejects_same_version_with_another_inode_or_namespace() {
        let create = super::super::upload_tests::request();
        let identity = create.identity();
        let namespace = Uuid::parse_str(identity["namespace"].as_str().unwrap()).unwrap();
        let inode = Uuid::parse_str(identity["file"].as_str().unwrap()).unwrap();
        let parent = Uuid::parse_str(identity["parent"].as_str().unwrap()).unwrap();
        UploadCondition::Create { namespace, parent }
            .verify(&create)
            .unwrap();
        assert!(
            UploadCondition::Create {
                namespace: Uuid::new_v4(),
                parent
            }
            .verify(&create)
            .is_err()
        );
        let mut value = create.value.clone();
        let mut identity = identity;
        identity["previous"] = serde_json::json!(1);
        value["metadata"] = serde_json::json!(URL_SAFE_NO_PAD.encode(identity.to_string()));
        let replacement = PreparedUpload::parse(value).unwrap();
        let original = UploadCondition::Replace(FileVersion::Fdb {
            namespace,
            inode,
            version: 1,
        });
        original.verify(&replacement).unwrap();
        for version in [
            FileVersion::Fdb {
                namespace: Uuid::new_v4(),
                inode,
                version: 1,
            },
            FileVersion::Fdb {
                namespace,
                inode: Uuid::new_v4(),
                version: 1,
            },
            FileVersion::Fdb {
                namespace,
                inode,
                version: 2,
            },
        ] {
            assert!(
                UploadCondition::Replace(version)
                    .verify(&replacement)
                    .is_err()
            );
        }
        assert!(original.verify(&create).is_err());
        assert!(
            UploadCondition::Create { namespace, parent }
                .verify(&replacement)
                .is_err()
        );
    }

    #[test]
    fn invalid_and_cross_domain_conditions_fail_before_source_or_network_io() {
        let namespace = Uuid::new_v4();
        let inode = Uuid::new_v4();
        let client = FdbClient::new(["127.0.0.1:0"]).unwrap();
        let missing = std::env::temp_dir().join(format!("absent-scope-{}", Uuid::new_v4()));
        for condition in [
            UploadCondition::Create {
                namespace: Uuid::nil(),
                parent: inode,
            },
            UploadCondition::Create {
                namespace,
                parent: Uuid::nil(),
            },
            UploadCondition::Replace(FileVersion::Raft {
                round: 0,
                sequence: 0,
            }),
            UploadCondition::Replace(FileVersion::Ltfs {
                volume: namespace,
                file_uid: 1,
                generation: 1,
            }),
            UploadCondition::Replace(FileVersion::Fdb {
                namespace: Uuid::nil(),
                inode,
                version: 1,
            }),
            UploadCondition::Replace(FileVersion::Fdb {
                namespace,
                inode: Uuid::nil(),
                version: 1,
            }),
            UploadCondition::Replace(FileVersion::Fdb {
                namespace,
                inode,
                version: 0,
            }),
            UploadCondition::Replace(FileVersion::Fdb {
                namespace,
                inode,
                version: u64::MAX,
            }),
        ] {
            assert!(
                matches!(client.prepare_file("SIM",Uuid::new_v4(),"/file",&missing,condition,None,&missing.with_extension("request")),
                Err(ClientError::Io(error)) if error.kind() == std::io::ErrorKind::InvalidInput)
            );
        }
    }
}
