//! Bounded read-your-writes negatives over one genuine Closed group/task.
use super::*;

impl ManagedCatalog {
    pub(crate) async fn check_directory_successor_rejections_for_test(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        original: &ReclaimMetadataPlan,
        source_directory: &crate::ltfs::index::DirectoryNode,
    ) -> Result<Vec<String>> {
        let control = self.db.create_trx()?;
        record_at(&control, parent, owner).await?;
        isolation::require_metadata_successor_at(&control, parent, owner, original).await?;
        let row = row_at(&control, parent)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        let ready = ready(&row)?;
        let group = Uuid::parse_str(&ready.group).map_err(cache)?;
        let mut prefix = domain(
            parent.namespace(),
            crate::managed_catalog::RECLAIM_COPY_GROUP_DOMAIN,
        );
        prefix.extend_from_slice(&parent.intent()["epoch"].as_u64().unwrap().to_be_bytes());
        prefix.extend_from_slice(
            Uuid::parse_str(parent.intent()["operation"].as_str().unwrap())
                .unwrap()
                .as_bytes(),
        );
        prefix.push(b'g');
        prefix.extend_from_slice(group.as_bytes());
        let mut closed = prefix.clone();
        closed.extend_from_slice(b"/c");
        let mut header = prefix;
        header.extend_from_slice(b"/h");
        let original_header = control
            .get(&header, false)
            .await?
            .ok_or(CatalogError::Incomplete)?
            .to_vec();
        let root = ready
            .directories
            .iter()
            .find(|d| d.path.is_empty())
            .ok_or(CatalogError::Incomplete)?
            .clone();
        let source_object = Object::capture(IndexEntry::Directory {
            path: original.0.source.path.clone(),
            node: source_directory.clone(),
        })?;
        if source_object != original.0.source {
            return Err(CatalogError::Invalid("目录负例缺少原完整source节点"));
        }
        let mut changed_directory = source_directory.clone();
        changed_directory.xattrs.push(crate::ltfs::index::Xattr {
            key: "test.original.directory".into(),
            value: "changed".into(),
            base64: false,
        });
        let changed_object = Object::capture(IndexEntry::Directory {
            path: original.0.source.path.clone(),
            node: changed_directory,
        })?;
        control.cancel();
        let mut cases = Vec::new();
        for kind in [
            "uid",
            "source_xattr",
            "volume",
            "inventory",
            "source_path",
            "destination_path",
            "root",
            "raw",
            "symlink",
            "file",
            "owner",
            "namespace",
            "not_closed",
            "missing_closed",
            "damaged_closed",
        ] {
            let tr = self.db.create_trx()?;
            let mut parent = parent.clone();
            let mut plan = original.clone();
            let mut holder = owner;
            match kind {
                "uid" => plan.0.source.uid += 1,
                "source_xattr" => plan.0.source = changed_object.clone(),
                "volume" => plan.0.volume = Uuid::new_v4().to_string(),
                "inventory" => plan.0.inventory[0] ^= 1,
                "source_path" => plan.0.source.path.push_str("-foreign"),
                "destination_path" => plan.0.expected.path.push_str("-foreign"),
                "root" => {
                    plan.0.source = root.clone();
                    plan.0.expected.path = format!(
                        "_reclaim_{}",
                        parent.intent()["operation"].as_str().unwrap()
                    );
                }
                "raw" => {
                    plan.0.raw_body = Some(metadata_plan::RawBody {
                        body: plan.0.source.clone(),
                        block_size: 4096,
                        sha256: [1; 32],
                        md5: None,
                    })
                }
                "symlink" => plan.0.source.kind = ReclaimSourceNodeKind::Symlink,
                "file" => plan.0.source.kind = ReclaimSourceNodeKind::File,
                "owner" => holder += 1,
                "namespace" => parent.route.route.namespace = Uuid::new_v4(),
                "not_closed" => {
                    tr.set(
                        &header,
                        &isolation::directory_unclosed_header_for_test(&original_header)?,
                    );
                }
                "missing_closed" => tr.clear(&closed),
                "damaged_closed" => tr.set(&closed, b"damaged-test-only-uncommitted"),
                _ => unreachable!(),
            }
            let result = async {
                record_at(&tr, &parent, holder).await?;
                isolation::require_metadata_successor_at(&tr, &parent, holder, &plan).await
            }
            .await;
            tr.cancel();
            let error = result
                .err()
                .ok_or(CatalogError::Invalid("目录负例被错误接受"))?;
            cases.push(format!("{kind}: {error}"));
        }
        Ok(cases)
    }
    pub(crate) async fn check_covered_directory_successor_for_test(
        &self,
        parent: &ReclaimReservation,
        owner: u64,
        plan: &ReclaimMetadataPlan,
    ) -> Result<String> {
        let tr = self.db.create_trx()?;
        record_at(&tr, parent, owner).await?;
        let error = coverage::require_uncovered_uid_at(
            &tr,
            parent,
            owner,
            &ReclaimSourceNode(plan.0.source.clone()),
        )
        .await
        .err()
        .ok_or(CatalogError::Incomplete)?;
        if !matches!(error, CatalogError::Invalid("规划源节点已经完成覆盖")) {
            return Err(error);
        }
        let refused = isolation::require_metadata_successor_at(&tr, parent, owner, plan)
            .await
            .err()
            .ok_or(CatalogError::Invalid("已覆盖目录被重新准入"))?;
        tr.cancel();
        Ok(refused.to_string())
    }
}
