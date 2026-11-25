//! A scoped reference never grants an ordinary copy or permanent read right.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::managed_catalog) enum BindingLocation {
    Ordinary,
    Retained {
        snapshot: Arc<CopyReadSnapshot>,
        authority: RetainedLedgerAuthority,
        scoped: bool,
    },
}
impl BindingLocation {
    pub(super) fn key(&self, binding: &CopyLayoutBinding) -> Vec<u8> {
        match self {
            Self::Ordinary => binding_key(binding.namespace, binding.file, binding.copy),
            Self::Retained {
                authority, scoped, ..
            } => {
                if *scoped {
                    retained_binding_key(authority)
                } else {
                    binding_key(binding.namespace, binding.file, binding.copy)
                }
            }
        }
    }
    pub(super) async fn require_at(&self, tr: &Transaction) -> Result<()> {
        if let Self::Retained {
            snapshot,
            authority,
            ..
        } = self
        {
            snapshot
                .require_at(tr, authority.serial(), authority.route(), authority.owner())
                .await?;
        }
        Ok(())
    }
    pub(super) fn retained(&self) -> bool {
        matches!(self, Self::Retained { .. })
    }
    pub(super) async fn selected_binding_at(
        &self,
        tr: &Transaction,
        binding: &CopyLayoutBinding,
    ) -> Result<Option<CopyLayoutBinding>> {
        // A corrupt or required-but-missing ordinary value cannot be hidden by
        // a scoped reference. Ordinary lookup never searches this scope space.
        let ordinary = binding_at(tr, binding.namespace, binding.file, binding.copy).await?;
        match self {
            Self::Ordinary | Self::Retained { scoped: false, .. } => Ok(ordinary),
            Self::Retained {
                authority,
                scoped: true,
                ..
            } => retained_binding_at(tr, authority).await,
        }
    }
}
fn child_prefix(namespace: Uuid, parent_epoch: u64, parent: Uuid, child: Uuid) -> Vec<u8> {
    let mut key = namespace_prefix(namespace);
    key.push(3);
    key.extend_from_slice(&parent_epoch.to_be_bytes());
    key.extend_from_slice(parent.as_bytes());
    key.extend_from_slice(child.as_bytes());
    key
}
fn retained_binding_key(authority: &RetainedLedgerAuthority) -> Vec<u8> {
    let (epoch, parent, child, file, copy) = authority.scope_identity();
    let mut key = child_prefix(authority.namespace(), epoch, parent, child);
    key.extend_from_slice(file.as_bytes());
    key.extend_from_slice(copy.as_bytes());
    key
}

#[cfg(test)]
pub(in crate::managed_catalog) fn test_retained_layout_key(
    authority: &RetainedLedgerAuthority,
) -> Vec<u8> {
    retained_binding_key(authority)
}

#[cfg(test)]
pub(in crate::managed_catalog) fn test_inert_neighbor_scope_key(
    authority: &RetainedLedgerAuthority,
    parent: Uuid,
) -> Vec<u8> {
    let (epoch, original, child, file, copy) = authority.scope_identity();
    assert_ne!(original, parent);
    let mut key = child_prefix(authority.namespace(), epoch, parent, child);
    key.extend_from_slice(file.as_bytes());
    key.extend_from_slice(copy.as_bytes());
    key
}
pub(in crate::managed_catalog) async fn retained_binding_at(
    tr: &Transaction,
    authority: &RetainedLedgerAuthority,
) -> Result<Option<CopyLayoutBinding>> {
    let Some(raw) = tr.get(&retained_binding_key(authority), false).await? else {
        return Ok(None);
    };
    let binding = CopyLayoutBinding::decode(&raw)?;
    let (_, _, _, file, copy) = authority.scope_identity();
    if binding.namespace != authority.namespace()
        || binding.file != file
        || binding.copy != copy
        || !matches!(
            &binding.provenance,
            LayoutProvenance::VerifiedCopySource { .. }
        )
        || !binding.matches_raw_copy(Some(authority.original_record()))
    {
        return Err(CatalogError::Corrupt("保留作用域布局与原 child 身份不同"));
    }
    Ok(Some(binding))
}
/// Called only after the original checked child/i and settled history pin.
pub(in crate::managed_catalog) fn clear_retained_child_at(
    tr: &Transaction,
    namespace: Uuid,
    parent_epoch: u64,
    parent: Uuid,
    child: Uuid,
) {
    let prefix = child_prefix(namespace, parent_epoch, parent, child);
    tr.clear_range(&prefix, &prefix_end(&prefix));
}
