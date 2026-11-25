//! Complete one bounded eligibility plan before root metadata or group copying.
use super::*;
use crate::managed_catalog::{ReclaimCopyGroupLimits, ReclaimGroupOnlyQualification};

pub(super) fn prepare<'a>(
    owner: &'a Active,
    control: &'a FdbOwnerControl,
    parent: &'a ReclaimReservation,
    inventory: &'a crate::managed_catalog::ReclaimSourceInventory<'a>,
) -> std::pin::Pin<Box<impl std::future::Future<Output = std::result::Result<(), ReadError>> + 'a>>
{
    Box::pin(async move {
        if !parent.spec().group_only() {
            return Ok(());
        }
        let limits = ReclaimCopyGroupLimits::bounded(32, control.storage.download_bytes)
            .map_err(parent_error)?;
        let draft = match control
            .catalog
            .prepare_reclaim_group_only(
                parent,
                owner.round,
                inventory,
                limits,
                control.storage.index_bytes,
            )
            .await
            .map_err(parent_error)?
        {
            ReclaimGroupOnlyQualification::NotRequested | ReclaimGroupOnlyQualification::Ready => {
                return Ok(());
            }
            ReclaimGroupOnlyQualification::Unsupported(reason) => {
                return Err(lost(format!(
                    "UnsupportedGroupOnly：未执行迁移，原请求保留：{reason}"
                )));
            }
            ReclaimGroupOnlyQualification::NeedsCandidates(draft) => draft,
        };
        let mut candidates = Vec::new();
        for file in draft.files() {
            let full = crate::managed_catalog::pool_catalog_path(
                parent.spec().pool(),
                file.path().as_bytes(),
            )
            .map_err(parent_error)?;
            let selected = control
                .catalog
                .resolve_active_read_source_for_owner(
                    &control.serial,
                    &full,
                    control.route,
                    owner.round,
                )
                .await
                .map_err(parent_error)?
                .ok_or_else(|| lost("group-only原逻辑文件丢失"))?;
            let metadata = selected.metadata();
            let Some(binding) = selected.layout_binding().filter(|_| {
                metadata.copy.media.is_none()
                    && matches!(
                        metadata.copy.content_digest,
                        crate::managed_catalog::ContentDigest::Sha256(_)
                    )
            }) else {
                control
                    .catalog
                    .reject_reclaim_group_only(
                        parent,
                        owner.round,
                        &draft,
                        "完整输入包含非普通managed/SHA或缺少FDB extent布局",
                    )
                    .await
                    .map_err(parent_error)?;
                return Err(lost("UnsupportedGroupOnly：未执行迁移，原请求保留"));
            };
            let temporary = crate::local_read_storage::download_file(
                owner.files.spool_dir(),
                binding.encoded_bytes(),
                crate::local_read_storage::ReadStorageBudget {
                    download_bytes: control.storage.index_bytes,
                    ..control.storage
                },
            )
            .map_err(lost)?;
            let physical = selected
                .prepare_body_plan(&control.catalog, temporary, control.storage.index_bytes)
                .await
                .map_err(lost)?;
            let Some(candidate) = inventory
                .reclaim_group_candidate(parent, file, &metadata, &physical)
                .map_err(parent_error)?
            else {
                control
                    .catalog
                    .reject_reclaim_group_only(
                        parent,
                        owner.round,
                        &draft,
                        "完整输入包含不支持的原始节点或extent几何",
                    )
                    .await
                    .map_err(parent_error)?;
                return Err(lost("UnsupportedGroupOnly：未执行迁移，原请求保留"));
            };
            candidates.push((candidate, binding.clone()));
        }
        match control
            .catalog
            .seal_reclaim_group_only(parent, owner.round, &draft, &candidates)
            .await
            .map_err(parent_error)?
        {
            ReclaimGroupOnlyQualification::Ready => Ok(()),
            ReclaimGroupOnlyQualification::Unsupported(reason) => Err(lost(format!(
                "UnsupportedGroupOnly：未执行迁移，原请求保留：{reason}"
            ))),
            _ => Err(lost("group-only最终资格未完成，原请求保留")),
        }
    })
}
