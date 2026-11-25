//! Task-owned destination scheduling hints. No entry is media evidence.
use super::*;
use crate::ltfs::recovery::{
    CapturedIndexSource, FULL_HISTORY_ENTRY_BYTES, FullHistoryDirectory, FullHistoryEntry,
    FullHistoryHintBuilder, FullHistoryManifest, IndexScratchBudget,
};
use crate::ltfs::volume::CommittedIndexConstruct;

const ENTRIES_PER_CHUNK: u64 = 384;
const TRANSACTION_PAGE: u64 = 4;
#[path = "reclaim_destination_history/codec.rs"]
mod codec;
#[path = "reclaim_destination_history/context.rs"]
mod context;
#[path = "reclaim_destination_history/store.rs"]
mod store;
use codec::{Node, Retired, Root, Staging, State};
pub(crate) use context::DestinationHistoryContext;
pub(super) use store::clear_parent_at;

fn cache(error: impl std::fmt::Display) -> CatalogError {
    CatalogError::Cache(format!("目的完整历史位置提示: {error}"))
}
fn invalid() -> CatalogError {
    CatalogError::Corrupt("目的完整历史提示范围、固定对象或检查点不符")
}
fn parent_prefix(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = crate::managed_catalog::key(crate::managed_catalog::FULL_HISTORY_HINT_KIND);
    key.extend_from_slice(reservation.namespace().as_bytes());
    key.push(b't');
    key.extend_from_slice(reservation.fingerprint().as_bytes());
    key
}
fn parent_pins_key(reservation: &ReclaimReservation) -> Vec<u8> {
    let mut key = parent_prefix(reservation);
    key.push(b'p');
    key
}
async fn counter_at(tr: &Transaction, key: &[u8]) -> Result<u64> {
    let Some(raw) = tr.get(key, false).await? else {
        return Ok(0);
    };
    Ok(u64::from_be_bytes(
        raw.as_ref().try_into().map_err(|_| invalid())?,
    ))
}
fn write_counter(tr: &Transaction, key: &[u8], count: u64) {
    if count == 0 {
        tr.clear(key);
    } else {
        tr.set(key, &count.to_be_bytes());
    }
}
fn committed(head: ActiveIndexIdentity) -> CommittedIndexConstruct {
    CommittedIndexConstruct {
        volume_uuid: head.volume_uuid,
        generation: head.generation,
        location: head.location,
        sha256: head.sha256,
        incremental: head.incremental,
    }
}
