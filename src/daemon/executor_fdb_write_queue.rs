//! Bounded FIFO coalescing of fully spooled native uploads. Never cross a
//! control/read/prepare request or combine competing logical identities.
use super::*;
use crate::managed_catalog::ActiveFileRequest;

pub(super) struct QueuedWrite {
    pub request: ActiveFileRequest,
    pub file: std::fs::File,
    pub reply: Sender<std::result::Result<u64, ReadError>>,
}

fn compatible(batch: &[QueuedWrite], next: &ActiveFileRequest) -> bool {
    let first = &batch[0].request;
    first.pool_id.is_some()
        && next.pool_id == first.pool_id
        && next.namespace == first.namespace
        && next.library_serial == first.library_serial
        && next.barcode == first.barcode
        && batch.iter().all(|saved| {
            let saved = &saved.request;
            (saved.parent != next.parent || saved.name != next.name)
                && (saved.parent_path != next.parent_path || saved.name != next.name)
                && saved.file_id != next.file_id
                && (saved.request_epoch != next.request_epoch
                    || saved.operation_id != next.operation_id)
        })
}

pub(super) fn collect(
    first: QueuedWrite,
    rx: &Receiver<ExecRequest>,
    deferred: &mut Option<ExecRequest>,
    policy: super::super::files::BatchPolicy,
) -> Vec<QueuedWrite> {
    assert!(deferred.is_none());
    let mut bytes = first.request.expected_size;
    let mut batch = vec![first];
    // Reuse the existing policy, with one catalog-page-sized hard ceiling.
    // A short fixed deadline bounds added latency even under continuous load.
    let limit = policy.max_files.clamp(1, 128);
    let deadline = Instant::now()
        + policy
            .idle
            .min(policy.max_wait)
            .min(Duration::from_millis(50));
    while batch.len() < limit && bytes < policy.max_bytes {
        let next = match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(next) => next,
            Err(_) => break,
        };
        match next {
            ExecRequest::FdbWrite {
                intent,
                file,
                reply,
            } if compatible(&batch, &intent)
                && bytes
                    .checked_add(intent.expected_size)
                    .is_some_and(|size| size <= policy.max_bytes) =>
            {
                bytes += intent.expected_size;
                batch.push(QueuedWrite {
                    request: *intent,
                    file,
                    reply,
                });
            }
            next => {
                *deferred = Some(next);
                break;
            }
        }
    }
    batch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::files::BatchPolicy;
    use crate::managed_catalog::ContentDigest;
    use std::sync::mpsc::channel;
    use uuid::Uuid;

    fn queued(template: Option<&ActiveFileRequest>, size: u64) -> QueuedWrite {
        let request = match template {
            Some(saved) => ActiveFileRequest {
                name: Uuid::new_v4().to_string().into_bytes(),
                file_id: Uuid::new_v4(),
                operation_id: Uuid::new_v4(),
                expected_size: size,
                ..saved.clone()
            },
            None => ActiveFileRequest {
                namespace: Uuid::new_v4(),
                parent: Uuid::new_v4(),
                parent_path: b"/".to_vec(),
                name: b"first".to_vec(),
                file_id: Uuid::new_v4(),
                expected_version: None,
                operation_id: Uuid::new_v4(),
                request_epoch: 1,
                library_serial: "QUEUE".into(),
                pool_id: Some(Uuid::new_v4()),
                barcode: None,
                expected_size: size,
                expected_digest: ContentDigest::Sha256([1; 32]),
            },
        };
        QueuedWrite {
            request,
            file: std::fs::File::open("/dev/null").unwrap(),
            reply: channel().0,
        }
    }

    fn enqueue(tx: &Sender<ExecRequest>, value: QueuedWrite) {
        tx.send(ExecRequest::FdbWrite {
            intent: Box::new(value.request),
            file: value.file,
            reply: value.reply,
        })
        .unwrap();
    }

    #[test]
    fn fifo_barrier_preserves_read_before_later_write() {
        let first = queued(None, 3);
        let second = queued(Some(&first.request), 4);
        let third = queued(Some(&first.request), 5);
        let (tx, rx) = channel();
        enqueue(&tx, second);
        tx.send(ExecRequest::Read {
            path: "/barrier".into(),
            reply: channel().0,
        })
        .unwrap();
        enqueue(&tx, third);
        let mut deferred = None;
        assert_eq!(
            collect(first, &rx, &mut deferred, BatchPolicy::default()).len(),
            2
        );
        assert!(matches!(deferred, Some(ExecRequest::Read { .. })));
        assert!(matches!(rx.try_recv(), Ok(ExecRequest::FdbWrite { .. })));
    }

    #[test]
    fn byte_limit_and_competing_identity_remain_queued_in_order() {
        for conflict in [
            "path",
            "inode",
            "operation",
            "pool",
            "barcode",
            "bytes",
            "overflow",
        ] {
            let first = queued(
                None,
                if conflict == "overflow" {
                    u64::MAX - 1
                } else {
                    3
                },
            );
            let mut next = queued(Some(&first.request), 4);
            match conflict {
                "path" => next.request.name = first.request.name.clone(),
                "inode" => next.request.file_id = first.request.file_id,
                "operation" => next.request.operation_id = first.request.operation_id,
                "pool" => next.request.pool_id = Some(Uuid::new_v4()),
                "barcode" => next.request.barcode = Some("OTHERL8".into()),
                _ => {}
            }
            let (tx, rx) = channel();
            enqueue(&tx, next);
            let mut deferred = None;
            let policy = BatchPolicy {
                max_bytes: if conflict == "bytes" { 6 } else { u64::MAX },
                ..Default::default()
            };
            assert_eq!(
                collect(first, &rx, &mut deferred, policy).len(),
                1,
                "{conflict}"
            );
            assert!(
                matches!(deferred, Some(ExecRequest::FdbWrite { .. })),
                "{conflict}"
            );
        }
    }

    #[test]
    fn page_ceiling_bounds_spools_and_zero_wait_still_drains_ready_work() {
        let first = queued(None, 0);
        let (tx, rx) = channel();
        for _ in 0..129 {
            enqueue(&tx, queued(Some(&first.request), 0));
        }
        let mut deferred = None;
        let policy = BatchPolicy {
            idle: Duration::ZERO,
            ..Default::default()
        };
        assert_eq!(collect(first, &rx, &mut deferred, policy).len(), 128);
        assert!(deferred.is_none());
        assert_eq!(rx.try_iter().count(), 2);
    }
}
