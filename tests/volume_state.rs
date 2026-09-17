//! D03 每卷根与差量发布：PC01—PC06（commit-publication.md）与 PR01—PR06（publication-root.md）。

use std::collections::BTreeMap;
use std::sync::Arc;

use tape_rs::core::volume_state::*;

const INSTANCE: InstanceId = 7;

fn fresh() -> VolumeState {
    VolumeState::new(INSTANCE, Arc::new(DirNode::default()), 1, 1_000_000, true)
}

fn evidence(b: &FrozenBatch) -> S4Evidence {
    S4Evidence {
        batch_id: b.batch_id,
        generation: b.generation,
    }
}

#[test]
fn pc01_snapshot_x_then_more_writes_y_keeps_y_pending() {
    let st = fresh();
    st.open_session(1, 100);
    st.admit(1, "/d/a", 1000, 10).unwrap();
    st.ingest("/d/a", 400).unwrap();
    // fsync 快照 X = 400 字节
    let batch = st.freeze(&["/d/a"]).unwrap();
    assert_eq!(
        batch.cover["/d/a"],
        CoverEntry {
            version: 1,
            len: 400,
            complete: false
        }
    );
    // 写索引期间继续接收 Y
    st.ingest("/d/a", 300).unwrap();
    let w_x = st.wait_for("/d/a", 400);
    let w_y = st.wait_for("/d/a", 700);

    let out = st.publish(&batch, evidence(&batch)).unwrap();
    assert_eq!(out.notified, vec![w_x], "只确认要求 X 的等待者");
    let r = st.load();
    assert_eq!(
        *r.committed.get("/d/a").unwrap(),
        FileVersion {
            version: 1,
            len: 400,
            partial: true
        }
    );
    let p = &r.pending["/d/a"];
    assert_eq!(
        (p.staged_len, p.committed_prefix, p.earliest_registered),
        (700, 400, 10)
    );
    assert_eq!(p.state, PendingState::InProgress);
    assert!(!r.ledger.reservations[&1].released, "前缀提交不释放预留");
    assert_eq!(r.generation, 2);

    // 完成后整文件提交：条目移除、预留释放、Y 的等待者被确认
    st.complete("/d/a").unwrap();
    let batch2 = st.freeze(&[]).unwrap();
    assert_eq!(
        batch2.cover["/d/a"],
        CoverEntry {
            version: 1,
            len: 700,
            complete: true
        }
    );
    let out2 = st.publish(&batch2, evidence(&batch2)).unwrap();
    assert_eq!(out2.notified, vec![w_y]);
    let r = st.load();
    assert!(r.pending.is_empty());
    assert!(r.ledger.reservations[&1].released);
    assert_eq!(
        r.ledger.available(),
        1_000_000 - 700,
        "物理占用不因提交消失"
    );
    assert_eq!(
        *r.committed.get("/d/a").unwrap(),
        FileVersion {
            version: 1,
            len: 700,
            partial: false
        }
    );
}

#[test]
fn pc02_cancel_and_calibration_after_freeze_do_not_double_account() {
    let st = fresh();
    st.open_session(1, 100);
    st.admit(1, "/f", 1000, 0).unwrap();
    st.ingest("/f", 500).unwrap();
    st.complete("/f").unwrap();
    let batch = st.freeze(&[]).unwrap();
    // 冻结后取消：会话有在途引用 → Closing，预留暂不释放
    st.cancel(1).unwrap();
    let r = st.load();
    assert_eq!(r.sessions[&1].state, SessionState::Closing);
    assert!(!r.ledger.reservations[&1].released);
    // 较新的校准：设备读数已包含这 500 字节
    st.calibrate(900_000);
    assert_eq!(st.load().ledger.consumed_since_cut, 0);

    st.publish(&batch, evidence(&batch)).unwrap();
    let r = st.load();
    assert_eq!(r.sessions[&1].state, SessionState::Expired);
    assert!(r.ledger.reservations[&1].released);
    assert_eq!(
        r.ledger.available(),
        900_000,
        "不双扣、不多退：以当前基线为准"
    );
    // 重复发布不再改账本
    let dup = st.publish(&batch, evidence(&batch)).unwrap();
    assert!(dup.duplicate);
    assert_eq!(st.load().ledger.available(), 900_000);
}

#[test]
fn pc03_single_slot_duplicate_publish_and_notify_retry() {
    let st = fresh();
    st.open_session(1, 100);
    st.admit(1, "/a", 10, 0).unwrap();
    st.ingest("/a", 10).unwrap();
    st.complete("/a").unwrap();
    let w = st.wait_for("/a", 10);
    let a = st.freeze(&[]).unwrap();
    assert_eq!(
        st.freeze(&[]).unwrap_err(),
        StateError::CommitSlotOccupied(a.batch_id)
    );
    let out = st.publish(&a, evidence(&a)).unwrap();
    assert_eq!(out.notified, vec![w]);
    let e1 = out.epoch;
    let again = st.publish(&a, evidence(&a)).unwrap();
    assert!(again.duplicate && again.notified.is_empty() && again.epoch == e1);
    assert_eq!(
        st.renotify(&a),
        vec![w],
        "通知重试只返回已覆盖者，不重发状态转换"
    );
    // 证据不匹配被拒绝
    let bad = S4Evidence {
        batch_id: 99,
        generation: 2,
    };
    assert!(matches!(
        st.publish(&a, bad),
        Err(StateError::EvidenceMismatch { .. })
    ));
}

#[test]
fn pc04_root_construction_failure_poisons_but_keeps_evidence_and_old_view() {
    let st = fresh();
    st.open_session(1, 100);
    st.admit(1, "/a", 10, 0).unwrap();
    st.ingest("/a", 10).unwrap();
    st.complete("/a").unwrap();
    let w = st.wait_for("/a", 10);
    let reader = st.load();
    let batch = st.freeze(&[]).unwrap();
    st.fail_next_publish();
    let err = st.publish(&batch, evidence(&batch)).unwrap_err();
    assert_eq!(
        err,
        StateError::PublishFailed { notified: vec![w] },
        "S4 证据完整的等待者仍可通知"
    );
    let r = st.load();
    assert!(!r.capability.trusted && !r.capability.readable);
    assert_eq!(st.stats().poisoned, 1);
    // 旧根对象完整，不冒充最新
    assert_eq!(reader.committed.count_files(), 0);
    assert!(reader.capability.trusted);
    assert!(matches!(
        st.admit(1, "/b", 1, 0),
        Err(StateError::NotWritable(_))
    ));
}

#[test]
fn pc05_stale_instance_batch_is_rejected_without_touching_state() {
    let st = fresh();
    st.open_session(1, 100);
    st.admit(1, "/a", 10, 0).unwrap();
    st.ingest("/a", 10).unwrap();
    st.complete("/a").unwrap();
    let real = st.freeze(&[]).unwrap();
    let mut old = (*real).clone();
    old.instance = INSTANCE + 1;
    let before = st.load().epoch;
    assert_eq!(
        st.publish(&old, evidence(&old)).unwrap_err(),
        StateError::StaleInstance {
            batch: INSTANCE + 1,
            current: INSTANCE
        }
    );
    assert_eq!(st.load().epoch, before);
    // 真正的批次仍可发布：可信历史不被抹掉
    st.publish(&real, evidence(&real)).unwrap();
    assert_eq!(st.load().committed.count_files(), 1);
}

#[test]
fn pc06_readers_keep_old_root_and_reclaim_is_delayed() {
    let st = fresh();
    st.open_session(1, 100);
    let r0 = st.load();
    for i in 0..3 {
        let p = format!("/f{i}");
        st.admit(1, &p, 10, 0).unwrap();
        st.ingest(&p, 10).unwrap();
        st.complete(&p).unwrap();
        let b = st.freeze(&[]).unwrap();
        st.publish(&b, evidence(&b)).unwrap();
    }
    assert_eq!(r0.committed.count_files(), 0, "读者视图不变");
    assert_eq!(r0.epoch, 2);
    assert_eq!(st.load().committed.count_files(), 3);
    let retired = st.retired_count();
    assert!(retired >= 3);
    let freed = st.reclaim();
    assert_eq!(st.retired_count(), 1, "r0 仍被读者持有，不回收");
    assert_eq!(freed, retired - 1);
    drop(r0);
    assert_eq!(st.reclaim(), 1);
    assert_eq!(st.retired_count(), 0);
}

#[test]
fn pr01_publish_cost_is_proportional_to_changed_paths_not_tree_size() {
    // 100 个目录 × 1000 个文件 = 10 万文件的已提交视图
    let mut root = DirNode::default();
    for d in 0..100 {
        let mut dir = DirNode::default();
        for f in 0..1000 {
            dir.files.insert(
                format!("f{f}"),
                Arc::new(FileVersion {
                    version: 1,
                    len: 1,
                    partial: false,
                }),
            );
        }
        root.subdirs.insert(format!("d{d}"), Arc::new(dir));
    }
    let st = VolumeState::new(INSTANCE, Arc::new(root), 1, 1 << 40, true);
    st.open_session(1, 100);
    for i in 0..10 {
        let p = format!("/d42/new{i}");
        st.admit(1, &p, 1, 0).unwrap();
        st.ingest(&p, 1).unwrap();
        st.complete(&p).unwrap();
    }
    let b = st.freeze(&[]).unwrap();
    st.publish(&b, evidence(&b)).unwrap();
    let s = st.stats();
    assert!(
        s.last_publish_nodes_copied <= 10 * 2,
        "复制节点 {} 应与 10 条路径×深度 2 成正比",
        s.last_publish_nodes_copied
    );
    assert_eq!(st.load().committed.count_files(), 100_010);
}

#[test]
fn pr04_stale_expiry_check_is_discarded_after_renewal() {
    let st = fresh();
    st.open_session(1, 100);
    st.admit(1, "/a", 10, 0).unwrap();
    let observed = st.load().epoch;
    st.renew(1, 200).unwrap();
    assert!(
        st.tick(150, observed).is_empty(),
        "携带旧 epoch 的到期检查作废"
    );
    assert!(st.tick(150, st.load().epoch).is_empty(), "新期限内不过期");
    assert_eq!(st.tick(250, st.load().epoch), vec![1]);
    let r = st.load();
    assert_eq!(r.sessions[&1].state, SessionState::Expired);
    assert!(
        r.pending.is_empty(),
        "未进入批次的 in_progress 条目随会话结束"
    );
    assert!(r.ledger.reservations[&1].released);
    assert!(
        matches!(st.renew(1, 300), Err(StateError::SessionInvalid(1))),
        "到期不复活"
    );
}

#[test]
fn pr06_batch_frozen_at_older_epoch_publishes_on_current_state() {
    let st = fresh();
    st.open_session(1, 100);
    st.admit(1, "/a", 10, 0).unwrap();
    st.ingest("/a", 10).unwrap();
    st.complete("/a").unwrap();
    let b = st.freeze(&[]).unwrap();
    // 冻结后又准入了新路径：发布只转换本批覆盖，保留新条目
    st.admit(1, "/later", 5, 1).unwrap();
    st.publish(&b, evidence(&b)).unwrap();
    let r = st.load();
    assert!(r.committed.get("/a").is_some());
    assert!(r.committed.get("/later").is_none());
    assert_eq!(r.pending.len(), 1);
    assert!(r.pending.contains_key("/later"));
}

#[test]
fn admission_respects_capacity_and_path_exclusivity() {
    let st = VolumeState::new(INSTANCE, Arc::new(DirNode::default()), 1, 100, true);
    st.open_session(1, 100);
    st.admit(1, "/a", 60, 0).unwrap();
    assert!(matches!(
        st.admit(1, "/b", 50, 0),
        Err(StateError::InsufficientCapacity { available: 40, .. })
    ));
    assert_eq!(
        st.admit(1, "/a", 1, 0).unwrap_err(),
        StateError::PathBusy("/a".into())
    );
    assert!(matches!(
        st.ingest("/a", 61),
        Err(StateError::InsufficientCapacity { .. })
    ));
    let mut cover = BTreeMap::new();
    cover.insert(
        "/zzz".to_string(),
        CoverEntry {
            version: 9,
            len: 1,
            complete: true,
        },
    );
    let _ = cover;
}
