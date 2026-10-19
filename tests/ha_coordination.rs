//! HA 协议的公开控制状态接口；不打开任何真实设备。
use tape_rs::daemon::state::{Command, ControlState};

fn command(json: &str) -> Command {
    Command::decode(json.as_bytes()).expect("HA 控制命令必须能被节点解码")
}

#[test]
fn adding_fc_admits_a_control_node_without_changing_raft_membership() {
    let mut state = ControlState::default();
    state.apply(1, &command(r#"{"op":"candidate","node":3,"instance":1,"sequence":1,"group":"lab","drives":0,"eligible":false}"#));
    state.apply(2, &command(r#"{"op":"assign_executor","node":3,"instance":1,"term":7,"expected_round":null,"group":"lab"}"#));
    assert!(state.executor.is_none(), "没有 FC 的控制节点不能执行");
    state.apply(3, &command(r#"{"op":"candidate","node":3,"instance":1,"sequence":2,"group":"lab","drives":1,"eligible":true}"#));
    state.apply(4, &command(r#"{"op":"assign_executor","node":3,"instance":1,"term":7,"expected_round":null,"group":"lab"}"#));
    let owner = state
        .executor
        .as_ref()
        .expect("FC 准入后可被同一控制 Leader 选中");
    assert_eq!((owner.node, owner.round, owner.fenced), (3, 4, false));
}

#[test]
fn snapshot_preserves_candidate_instances_and_rejects_a_delayed_old_process() {
    use tape_rs::daemon::state::ControlSnapshot;
    let mut state = ControlState::default();
    state.apply(1, &command(r#"{"op":"candidate","node":2,"instance":9,"sequence":2,"group":"lab","drives":1,"eligible":true}"#));
    let snapshot = ControlSnapshot {
        control: state.clone(),
        source: 3,
        directory_index: 0,
    };
    let mut restored = ControlSnapshot::decode(&snapshot.encode()).unwrap().control;
    assert_eq!(restored, state);
    restored.apply(2, &command(r#"{"op":"candidate","node":2,"instance":8,"sequence":999,"group":"lab","drives":1,"eligible":true}"#));
    restored.apply(3, &command(r#"{"op":"assign_executor","node":2,"instance":8,"term":7,"expected_round":null,"group":"lab"}"#));
    assert!(restored.executor.is_none(), "旧进程能力消息不能复活旧实例");
}

#[test]
fn a_delayed_old_owner_report_cannot_change_the_new_round() {
    let mut state = ControlState::default();
    state.apply(1, &command(r#"{"op":"candidate","node":1,"instance":1,"sequence":1,"group":"lab","drives":1,"eligible":true}"#));
    state.apply(2, &command(r#"{"op":"candidate","node":2,"instance":1,"sequence":1,"group":"lab","drives":1,"eligible":true}"#));
    state.apply(3, &command(r#"{"op":"assign_executor","node":1,"instance":1,"term":7,"expected_round":null,"group":"lab"}"#));
    state.apply(4, &command(r#"{"op":"assign_executor","node":2,"instance":1,"term":7,"expected_round":3,"group":"lab"}"#));
    state.apply(5, &command(r#"{"op":"executor_report","node":1,"instance":1,"round":3,"sequence":1,"command":{"op":"abandoned","node":1,"round":3,"reason":"迟到故障"}}"#));
    assert_eq!(
        state.executor.as_ref().map(|e| (e.node, e.round)),
        Some((2, 4))
    );
}

#[test]
fn expired_runtime_permission_cannot_be_revived_by_a_late_reply() {
    use std::time::{Duration, Instant};
    use tape_rs::daemon::permission::{Permission, Phase};
    let permission = Permission::default();
    assert!(permission.renew(4, Phase::Fencing, Instant::now() + Duration::from_secs(1)));
    permission.close(4);
    assert!(!permission.renew(
        4,
        Phase::Execution,
        Instant::now() + Duration::from_secs(30)
    ));
    assert!(!permission.allows(4, Phase::Fencing));
    assert!(permission.renew(5, Phase::Fencing, Instant::now() + Duration::from_secs(1)));
    assert!(!permission.allows(4, Phase::Fencing));
    assert!(!permission.renew(6, Phase::Fencing, Instant::now() - Duration::from_secs(1)));
}

#[test]
fn closing_a_new_round_before_its_first_grant_blocks_delayed_grants() {
    use std::time::{Duration, Instant};
    use tape_rs::daemon::permission::{Permission, Phase};
    let permission = Permission::default();
    assert!(permission.renew(4, Phase::Fencing, Instant::now() + Duration::from_secs(1)));
    permission.close(6);
    assert!(!permission.renew(5, Phase::Fencing, Instant::now() + Duration::from_secs(1)));
    assert!(!permission.renew(6, Phase::Fencing, Instant::now() + Duration::from_secs(1)));
    assert!(permission.renew(7, Phase::Fencing, Instant::now() + Duration::from_secs(1)));
}

#[test]
fn a_closed_permission_blocks_even_a_previously_open_device() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tape_rs::daemon::permission::{GuardedTransport, Permission, Phase};
    use tape_rs::scsi::sim::SimLibrary;
    use tape_rs::scsi::transport::TapeTransport;
    let lib = SimLibrary::new(1, 2, 0);
    let permission = Arc::new(Permission::default());
    permission.renew(4, Phase::Execution, Instant::now() + Duration::from_secs(1));
    let dev = GuardedTransport::new(Box::new(lib.drive(0)), permission.clone(), 4);
    permission.close(4);
    let result = dev.execute_no_data(&[0x01, 0, 0, 0, 0, 0], 1000);
    assert!(matches!(
        result,
        Err(tape_rs::error::TapeError::OwnershipLost { .. })
    ));
}

#[test]
fn restarting_a_node_never_reuses_its_execution_instance() {
    use tape_rs::daemon::store::RaftStore;
    let dir = std::env::temp_dir().join(format!("ha-instance-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("raft.db");
    let first = RaftStore::open(&path, &[1, 2, 3])
        .unwrap()
        .next_instance()
        .unwrap();
    let second = RaftStore::open(&path, &[1, 2, 3])
        .unwrap()
        .next_instance()
        .unwrap();
    assert!(second > first);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_out_of_order_ready_report_cannot_skip_fencing() {
    let mut state = ControlState::default();
    state.apply(1,&command(r#"{"op":"candidate","node":1,"instance":1,"sequence":1,"group":"lab","drives":1,"eligible":true}"#));
    state.apply(2,&command(r#"{"op":"assign_executor","node":1,"instance":1,"term":1,"expected_round":null,"group":"lab"}"#));
    state.apply(3,&command(r#"{"op":"executor_report","node":1,"instance":1,"round":2,"sequence":2,"command":{"op":"fenced","node":1,"round":2}}"#));
    assert!(
        !state.executor.as_ref().unwrap().fenced,
        "缺少第一份报告时不能越序确认隔离"
    );
}
