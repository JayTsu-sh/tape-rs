//! Identities captured by the caller, before preparation can resolve a path.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeCondition {
    pub parent: FileVersion,
    /// None creates a new node. Existing nodes require the full FDB revision.
    pub node: Option<FileVersion>,
}

pub(super) struct ConditionParts {
    pub namespace: Uuid,
    pub parent: Uuid,
    pub parent_version: u64,
    pub node: Option<Uuid>,
    pub expected_version: Option<u64>,
}

impl NodeCondition {
    pub(crate) fn check(self, change: &Value) -> Result<()> {
        self.validate(change).map(|_| ())
    }
    pub(super) fn parts(self) -> Result<ConditionParts> {
        let FileVersion::Fdb {
            namespace,
            inode: parent,
            version: parent_version,
        } = self.parent
        else {
            return Err(invalid("节点父目录条件必须是完整 FDB 版本"));
        };
        if namespace.is_nil() || parent.is_nil() || parent_version == 0 {
            return Err(invalid("节点父目录身份或版本无效"));
        }
        let (node, expected_version) = match self.node {
            None => (None, None),
            Some(FileVersion::Fdb {
                namespace: ns,
                inode,
                version,
            }) if ns == namespace
                && !inode.is_nil()
                && inode != parent
                && version > 0
                && version < u64::MAX =>
            {
                (Some(inode), Some(version))
            }
            _ => {
                return Err(invalid(
                    "节点条件必须是同一 namespace 的原 inode 与可递增版本",
                ));
            }
        };
        Ok(ConditionParts {
            namespace,
            parent,
            parent_version,
            node,
            expected_version,
        })
    }

    pub(super) fn validate(self, change: &Value) -> Result<ConditionParts> {
        let parts = self.parts()?;
        let (creating, changes_parent) = mutation(change)?;
        if creating != parts.node.is_none() || (changes_parent && parts.parent_version == u64::MAX)
        {
            return Err(invalid("节点变更类型与原目标或父目录版本条件不符"));
        }
        Ok(parts)
    }

    pub(super) fn verify(self, prepared: &PreparedNodeMutation) -> Result<()> {
        let parts = self.validate(&prepared.request()["mutation"])?;
        let request = prepared.request();
        if request["namespace"] != parts.namespace.to_string()
            || request["parent"] != parts.parent.to_string()
            || request["parent_version"] != parts.parent_version
            || request["expected_version"] != serde_json::json!(parts.expected_version)
            || parts
                .node
                .is_some_and(|node| request["node"] != node.to_string())
        {
            return Err(ClientError::Verification(
                "节点准备应答改变了原 namespace、父目录、inode 或版本".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn value(self) -> Value {
        let parts = self.parts().expect("validated original node condition");
        serde_json::json!({"namespace":parts.namespace.to_string(),"parent":parts.parent.to_string(),
            "parent_version":parts.parent_version,"node":parts.node.map(|id|id.to_string()),
            "expected_version":parts.expected_version})
    }

    pub(super) fn parse(value: &Value) -> Result<Self> {
        if !fields(
            value,
            &[
                "namespace",
                "parent",
                "parent_version",
                "node",
                "expected_version",
            ],
        ) {
            return Err(invalid("原节点条件字段无效"));
        }
        let namespace = uuid(value, "namespace")?;
        let parent = FileVersion::Fdb {
            namespace,
            inode: uuid(value, "parent")?,
            version: value["parent_version"]
                .as_u64()
                .ok_or_else(|| invalid("原父目录版本无效"))?,
        };
        let node = if value["node"].is_null() && value["expected_version"].is_null() {
            None
        } else {
            Some(FileVersion::Fdb {
                namespace,
                inode: uuid(value, "node")?,
                version: value["expected_version"]
                    .as_u64()
                    .ok_or_else(|| invalid("原节点版本无效"))?,
            })
        };
        let condition = Self { parent, node };
        condition.parts()?;
        Ok(condition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_conditions_refuse_recreated_parent_or_node_at_the_same_version() {
        let prepared = super::super::tests::prepared();
        let request = prepared.request();
        let namespace = uuid(request, "namespace").unwrap();
        let parent = FileVersion::Fdb {
            namespace,
            inode: uuid(request, "parent").unwrap(),
            version: 7,
        };
        let original = NodeCondition { parent, node: None };
        original.verify(&prepared).unwrap();
        for other_parent in [
            FileVersion::Fdb {
                namespace,
                inode: Uuid::new_v4(),
                version: 7,
            },
            FileVersion::Fdb {
                namespace,
                inode: uuid(request, "parent").unwrap(),
                version: 8,
            },
        ] {
            assert!(
                NodeCondition {
                    parent: other_parent,
                    node: None
                }
                .verify(&prepared)
                .is_err()
            );
        }
        let mut value = prepared.value.clone();
        value["request"]["expected_version"] = serde_json::json!(3);
        value["request"]["mutation"] = serde_json::json!({"kind":"unlink"});
        value["fingerprint"] = serde_json::json!(fingerprint(&value["request"]));
        let existing = PreparedNodeMutation::parse(value.clone()).unwrap();
        let condition = NodeCondition {
            parent,
            node: Some(FileVersion::Fdb {
                namespace,
                inode: uuid(existing.request(), "node").unwrap(),
                version: 3,
            }),
        };
        condition.verify(&existing).unwrap();
        value["request"]["node"] = serde_json::json!(Uuid::new_v4().to_string());
        value["fingerprint"] = serde_json::json!(fingerprint(&value["request"]));
        assert!(
            condition
                .verify(&PreparedNodeMutation::parse(value).unwrap())
                .is_err()
        );
    }

    #[test]
    fn invalid_or_cross_domain_conditions_fail_before_creating_a_journal_or_rpc() {
        let root = std::env::temp_dir().join(format!("node-condition-{}", Uuid::new_v4()));
        let namespace = Uuid::new_v4();
        let parent_id = Uuid::new_v4();
        let parent = FileVersion::Fdb {
            namespace,
            inode: parent_id,
            version: 1,
        };
        let client = FdbClient::new(["127.0.0.1:0"]).unwrap();
        for condition in [
            NodeCondition {
                parent: FileVersion::Raft {
                    round: 1,
                    sequence: 1,
                },
                node: None,
            },
            NodeCondition {
                parent: FileVersion::Fdb {
                    namespace,
                    inode: parent_id,
                    version: 0,
                },
                node: None,
            },
            NodeCondition {
                parent: FileVersion::Fdb {
                    namespace,
                    inode: parent_id,
                    version: u64::MAX,
                },
                node: None,
            },
            NodeCondition {
                parent: FileVersion::Fdb {
                    namespace: Uuid::nil(),
                    inode: parent_id,
                    version: 1,
                },
                node: None,
            },
            NodeCondition {
                parent,
                node: Some(FileVersion::Ltfs {
                    volume: namespace,
                    file_uid: 1,
                    generation: 1,
                }),
            },
            NodeCondition {
                parent,
                node: Some(FileVersion::Fdb {
                    namespace: Uuid::new_v4(),
                    inode: Uuid::new_v4(),
                    version: 1,
                }),
            },
            NodeCondition {
                parent,
                node: Some(parent),
            },
            NodeCondition {
                parent,
                node: Some(FileVersion::Fdb {
                    namespace,
                    inode: Uuid::new_v4(),
                    version: u64::MAX,
                }),
            },
        ] {
            let mutation = if condition.node.is_some() {
                serde_json::json!({"kind":"unlink"})
            } else {
                serde_json::json!({"kind":"create-directory"})
            };
            assert!(
                matches!(client.prepare_node_mutation("NODE-CLIENT",Uuid::new_v4(),"/child",condition,mutation,&root), Err(ClientError::Io(error)) if error.kind()==std::io::ErrorKind::InvalidInput)
            );
            assert!(!root.exists());
        }
    }
}
