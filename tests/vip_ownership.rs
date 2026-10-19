//! VIP 释放必须来自成功撤址后的显式确认；不接触真实网络或设备。
use tape_rs::daemon::state::{Command, ControlSnapshot, ControlState};

fn address() -> std::net::Ipv4Addr {
    "192.0.2.100".parse().unwrap()
}

fn acquire(node: u64, round: u64, expected_token: u64) -> Command {
    Command::VipAcquire {
        address: address(),
        node,
        round,
        expected_token,
    }
}

fn release(node: u64, round: u64, token: u64) -> Command {
    Command::VipRelease {
        address: address(),
        node,
        round,
        token,
    }
}

#[test]
fn owner_replacement_or_unbounded_wait_cannot_clear_an_unacknowledged_vip() {
    let mut state = ControlState::default();
    state.apply(1, &Command::Takeover { node: 1, term: 1 });
    state.apply(2, &acquire(1, 1, 0));
    state.apply(3, &Command::Takeover { node: 2, term: 2 });
    state.apply(4, &acquire(2, 3, 2));
    state.apply(1_000_000, &acquire(2, 3, 2));
    let held = &state.vips[&address()];
    assert_eq!(held.token, 2);
    assert_eq!(
        held.holder.as_ref().unwrap().node,
        1,
        "旧节点失联时宁可停止新 VIP"
    );
}

#[test]
fn release_matches_node_round_and_token_and_stale_ack_cannot_clear_the_next_grant() {
    let mut state = ControlState::default();
    state.apply(1, &Command::Takeover { node: 1, term: 1 });
    state.apply(2, &acquire(1, 1, 0));
    state.apply(3, &Command::Takeover { node: 2, term: 2 });
    for (index, cmd) in [
        (4, release(2, 1, 2)),
        (5, release(1, 3, 2)),
        (6, release(1, 1, 3)),
    ] {
        state.apply(index, &cmd);
        assert!(state.vips[&address()].holder.is_some());
    }
    state.apply(7, &release(1, 1, 2));
    assert!(state.vips[&address()].holder.is_none());
    state.apply(8, &acquire(2, 3, 2));
    state.apply(9, &release(1, 1, 2));
    assert_eq!(state.vips[&address()].token, 8);
    assert_eq!(state.vips[&address()].holder.as_ref().unwrap().node, 2);
}

#[test]
fn a_queued_acquire_from_before_release_cannot_reopen_the_old_grant() {
    let mut state = ControlState::default();
    state.apply(1, &Command::Takeover { node: 1, term: 1 });
    state.apply(2, &acquire(1, 1, 0));
    state.apply(3, &release(1, 1, 2));
    state.apply(4, &acquire(1, 1, 0));
    assert!(state.vips[&address()].holder.is_none());
    state.apply(5, &acquire(1, 1, 2));
    assert_eq!(state.vips[&address()].token, 5);
}

#[test]
fn snapshot_preserves_unreleased_ownership_and_released_token_tombstones() {
    let mut state = ControlState::default();
    state.apply(1, &Command::Takeover { node: 1, term: 1 });
    state.apply(2, &acquire(1, 1, 0));
    for released in [false, true] {
        if released {
            state.apply(3, &release(1, 1, 2));
        }
        let snapshot = ControlSnapshot {
            control: state.clone(),
            source: 1,
            directory_index: 0,
        };
        let restored = ControlSnapshot::decode(&snapshot.encode()).unwrap();
        assert_eq!(snapshot, restored);
        assert_eq!(restored.control.vips[&address()].holder.is_none(), released);
        assert_eq!(restored.control.vips[&address()].token, 2);
    }
}

#[test]
fn sqlite_snapshot_reopen_preserves_a_crashed_holders_record() {
    use tape_rs::daemon::store::RaftStore;
    let dir = std::env::temp_dir().join(format!("vip-snapshot-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("raft.db");
    let mut state = ControlState::default();
    let commands = [Command::Takeover { node: 1, term: 1 }, acquire(1, 1, 0)];
    let entries: Vec<_> = commands
        .iter()
        .enumerate()
        .map(|(i, cmd)| {
            let index = i as u64 + 1;
            state.apply(index, cmd);
            let mut entry = raft::eraftpb::Entry::default();
            entry.set_index(index);
            entry.set_term(1);
            entry.set_data(cmd.encode().into());
            entry
        })
        .collect();
    {
        let mut store = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        store.append(&entries).unwrap();
        store.set_commit(2).unwrap();
        let bytes = ControlSnapshot {
            control: state,
            source: 1,
            directory_index: 0,
        }
        .encode();
        let snapshot = store.build_snapshot(2, bytes).unwrap();
        store.compact_to(&snapshot).unwrap();
    }
    {
        let store = RaftStore::open(&path, &[1, 2, 3]).unwrap();
        let mut restored = ControlSnapshot::decode(store.loaded_snapshot().unwrap().get_data())
            .unwrap()
            .control;
        restored.apply(3, &Command::Takeover { node: 2, term: 2 });
        restored.apply(4, &acquire(2, 3, 2));
        assert_eq!(
            restored.vips[&address()].holder.as_ref().unwrap().node,
            1,
            "重启/压缩不能抹去未确认旧占用"
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn vip_commands_round_trip_and_malformed_snapshot_cannot_silently_free_a_holder() {
    for cmd in [acquire(1, 4, 2), release(1, 4, 5)] {
        assert_eq!(Command::decode(&cmd.encode()), Some(cmd));
    }
    assert!(
        Command::decode(
            br#"{"op":"vip_acquire","address":"bad","node":1,"round":1,"expected_token":0}"#
        )
        .is_none()
    );
    let snapshot = ControlSnapshot {
        control: ControlState::default(),
        source: 1,
        directory_index: 0,
    };
    let mut value: serde_json::Value = serde_json::from_slice(&snapshot.encode()).unwrap();
    value["vips"] = serde_json::json!([["192.0.2.100", 2, [1, 1]]]);
    assert!(ControlSnapshot::decode(&serde_json::to_vec(&value).unwrap()).is_none());
    value.as_object_mut().unwrap().remove("vips");
    assert!(
        ControlSnapshot::decode(&serde_json::to_vec(&value).unwrap())
            .unwrap()
            .control
            .vips
            .is_empty()
    );
}
