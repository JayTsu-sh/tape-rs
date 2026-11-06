use super::*;

fn serving() -> ControlState {
    let mut s = ControlState::default();
    for node in [1, 2] {
        s.apply(
            node,
            &Command::Candidate {
                node,
                instance: 10 + node,
                sequence: 1,
                group: "library".into(),
                drives: 1,
                eligible: true,
            },
        );
    }
    s.apply(
        3,
        &Command::AssignExecutor {
            node: 1,
            instance: 11,
            term: 1,
            expected_round: None,
            group: "library".into(),
        },
    );
    s.apply(4, &report(1, Command::Fenced { node: 1, round: 3 }));
    s.apply(5, &report(2, Command::ExecutorReady { node: 1, round: 3 }));
    s.apply(
        6,
        &report(
            3,
            Command::DeviceActivity {
                sequence: 1,
                busy: false,
            },
        ),
    );
    assert!(s.executor_ready);
    assert!(!s.device_activity.as_ref().unwrap().busy);
    s
}
fn report(sequence: u64, command: Command) -> Command {
    Command::ExecutorReport {
        node: 1,
        instance: 11,
        round: 3,
        sequence,
        command: Box::new(command),
    }
}
fn rebalance() -> Command {
    Command::RebalanceExecutor {
        node: 2,
        instance: 12,
        term: 1,
        expected_round: 3,
        expected_instance: 11,
        idle_sequence: 1,
        group: "library".into(),
    }
}
#[test]
fn busy_committed_first_rejects_the_already_proposed_healthy_move() {
    for frozen in [false, true] {
        let mut s = serving();
        s.catalog_quiesce_index = frozen.then_some(1);
        s.catalog_frozen_index = frozen.then_some(2);
        let old_proposal = rebalance();
        s.apply(
            7,
            &report(
                4,
                Command::DeviceActivity {
                    sequence: 2,
                    busy: true,
                },
            ),
        );
        assert!(s.device_activity.as_ref().unwrap().busy);
        s.apply(8, &old_proposal);
        assert_eq!(s.executor.as_ref().unwrap().round, 3);
        // Completing work does not revive the stale idle proof.
        s.apply(
            9,
            &report(
                5,
                Command::DeviceActivity {
                    sequence: 3,
                    busy: false,
                },
            ),
        );
        s.apply(10, &old_proposal);
        assert_eq!(s.executor.as_ref().unwrap().round, 3);
        let mut fresh = rebalance();
        if let Command::RebalanceExecutor { idle_sequence, .. } = &mut fresh {
            *idle_sequence = 3;
        }
        s.apply(11, &fresh);
        assert_eq!(s.executor.as_ref().unwrap().node, 2);
        assert!(!s.executor_ready);
        assert!(s.device_activity.is_none());
    }
}
#[test]
fn healthy_move_committed_first_cannot_acknowledge_the_old_operations_busy_claim() {
    let mut s = serving();
    s.apply(7, &rebalance());
    s.apply(
        8,
        &report(
            4,
            Command::DeviceActivity {
                sequence: 2,
                busy: true,
            },
        ),
    );
    assert_eq!(s.executor.as_ref().unwrap().node, 2);
    assert!(s.device_activity.is_none());
    assert_eq!(s.executor_report_sequence, 0);
}
#[test]
fn failure_takeover_still_works_while_the_failed_owner_was_busy() {
    let mut s = serving();
    s.apply(
        7,
        &report(
            4,
            Command::DeviceActivity {
                sequence: 2,
                busy: true,
            },
        ),
    );
    s.apply(
        8,
        &Command::AssignExecutor {
            node: 2,
            instance: 12,
            term: 2,
            expected_round: Some(3),
            group: "library".into(),
        },
    );
    assert_eq!(s.executor.as_ref().unwrap().node, 2);
    assert!(!s.executor_ready);
    assert!(s.device_activity.is_none());
}
#[test]
fn busy_reports_are_instance_round_sequence_bound_and_survive_snapshot() {
    let mut s = serving();
    let before = s.device_activity.clone();
    let good = report(
        4,
        Command::DeviceActivity {
            sequence: 2,
            busy: true,
        },
    );
    for field in ["instance", "round", "sequence"] {
        let mut forged = good.clone();
        if let Command::ExecutorReport {
            instance,
            round,
            command,
            ..
        } = &mut forged
        {
            match field {
                "instance" => *instance = 99,
                "round" => *round = 99,
                _ => {
                    **command = Command::DeviceActivity {
                        sequence: 99,
                        busy: false,
                    }
                }
            }
        }
        s.apply(7, &forged);
        assert_eq!(s.device_activity, before);
    }
    s.apply(
        8,
        &Command::DeviceActivity {
            sequence: 2,
            busy: true,
        },
    );
    assert_eq!(
        s.device_activity, before,
        "direct report cannot change activity"
    );
    s.apply(9, &good);
    assert!(s.device_activity.as_ref().unwrap().busy);
    assert_eq!(Command::decode(&good.encode()), Some(good));
    assert_eq!(Command::decode(&rebalance().encode()), Some(rebalance()));
    let snapshot = ControlSnapshot {
        control: s,
        source: 1,
        directory_index: 9,
    };
    assert_eq!(ControlSnapshot::decode(&snapshot.encode()), Some(snapshot));
}
