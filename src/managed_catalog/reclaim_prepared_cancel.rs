//! Retain original cancellations of pre-Q or already recovered children.
//! Unknown device outcomes remain in the original media recovery protocol.
use super::*;

#[derive(Clone, Copy)]
enum Cancellation {
    Unstarted,
    Recovered,
}

fn key(reservation: &ReclaimReservation, operation: Uuid) -> Vec<u8> {
    let mut key = reservation.journal_key();
    key[domain(reservation.namespace(), b'j').len() - 1] = b'z';
    key.extend_from_slice(operation.as_bytes());
    key
}

pub(super) async fn require_no_cancellations_at(
    tr: &Transaction,
    reservation: &ReclaimReservation,
) -> Result<()> {
    let mut prefix = reservation.journal_key();
    prefix[domain(reservation.namespace(), b'j').len() - 1] = b'z';
    if requests::prefix_has_rows(tr, &prefix).await? {
        return Err(CatalogError::Corrupt("预期无子请求的父任务仍有取消终态"));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Release the original pre-Q permit exactly once. This never releases the
    /// parent pool gate or treats an admitted/unknown write as cancelled.
    pub async fn cancel_unstarted_reclaim_upload(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<PublishOutcome> {
        self.settle_reclaim_upload_cancellation(
            reservation,
            owner,
            request,
            Cancellation::Unstarted,
        )
        .await
    }

    /// Retain the exact cancellation already committed by fenced media recovery.
    /// The generic cancellation released its permit; never release it again.
    pub async fn seal_reclaim_upload_cancellation(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
    ) -> Result<PublishOutcome> {
        self.settle_reclaim_upload_cancellation(
            reservation,
            owner,
            request,
            Cancellation::Recovered,
        )
        .await
    }

    async fn settle_reclaim_upload_cancellation(
        &self,
        reservation: &ReclaimReservation,
        owner: u64,
        request: &ActiveFileRequest,
        origin: Cancellation,
    ) -> Result<PublishOutcome> {
        request.validate()?;
        self.db.run(|tr,_| Box::pin(async move {requests::retry_result(async {
            let parent=record_at(&tr,reservation,owner).await?;
            if parent["owner_epoch"]!=owner {return Err(CatalogError::OwnerFenced);}
            if request.namespace!=reservation.namespace() || request.pool_id!=Some(reservation.spec().pool) {
                return Err(CatalogError::Invalid("取消必须属于原回收命名空间和池"));
            }
            let (intent,permit)=children::child_at(&tr,request.namespace,reservation.spec().pool,
                request.request_epoch,request.operation_id,owner).await?.ok_or(CatalogError::Incomplete)?;
            if intent.request()!=*request {return Err(CatalogError::Invalid("取消不能改变封存的回收子请求"));}
            let child_raw=tr.get(&children::child_key(request.namespace,&reservation.token(),request.operation_id)?,false)
                .await?.ok_or(CatalogError::Incomplete)?;
            let terminal=requests::terminal_value(&reservation::cancellation_fingerprint(&intent),None);
            let receipt=json!({"version":1,"parent":reservation.fingerprint(),"request":request.fingerprint(),
                "child_sha256":Sha256::digest(&child_raw).to_vec(),"permit":permit,"terminal":terminal});
            let raw=receipt.to_string();
            let receipt_key=key(reservation,request.operation_id);
            let op=op_key(request.namespace,request.request_epoch,request.operation_id);
            let live_terminal=tr.get(&op,false).await?;
            let pin=children::history_pin_key(request);
            if let Some(previous)=tr.get(&receipt_key,false).await? {
                if previous.as_ref()!=raw.as_bytes() || live_terminal.as_ref().is_some_and(|p|p.as_ref()!=terminal)
                    || tr.get(&pin,false).await?.is_some() {
                    return Err(CatalogError::Corrupt("回收原取消终态、回执或历史引用不一致"));
                }
                reservation::require_no_attempt_at(&tr,&intent).await?;
                return Ok(PublishOutcome::RequestCancelled);
            }
            let window=requests::window(&tr,request.namespace).await?;
            if request.request_epoch<window.first_retained || request.request_epoch>window.accepting
                || tr.get(&pin,false).await?.as_deref()!=Some(&[][..]) {
                return Err(CatalogError::Corrupt("未结清回收子请求缺少原代次或历史引用"));
            }
            // These negative reads conflict with Q/I/S4 admission. A known
            // reservation or an unknown device outcome must not use this path.
            let release=match origin {
                Cancellation::Unstarted => {
                    if live_terminal.is_some() {return Err(CatalogError::PoolBusy);}
                    reservation::require_unstarted_at(&tr,&intent).await?;
                    Some(maintenance::release(&tr,request.namespace,permit).await?)
                }
                Cancellation::Recovered => {
                    if live_terminal.as_deref()!=Some(terminal.as_slice()) {return Err(CatalogError::Incomplete);}
                    reservation::require_no_attempt_at(&tr,&intent).await?;
                    None
                }
            };
            source_state_at(&tr,reservation).await?;
            // No fallible work is allowed after the first mutation.
            if let Some(release)=release {release.apply(&tr);}
            tr.set(&op,&terminal);tr.set(&receipt_key,raw.as_bytes());tr.clear(&pin);
            Ok(PublishOutcome::RequestCancelled)
        }.await)})).await?
    }
}
