//! TaskStore 的公开 API；仅使用本地临时 SQLite，不访问设备。
use std::path::PathBuf;

use serde_json::json;
use tape_rs::library::tasks::{TaskState, TaskStore};

#[allow(dead_code)]
#[path = "support/library_fixture.rs"]
mod support;

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("tape-tasks-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> PathBuf {
        self.0.join("library.sqlite")
    }

    fn open(&self) -> TaskStore {
        TaskStore::open(&self.path()).unwrap()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn foreground_job_metadata_and_outcome_survive_reopened_connections() {
    let db = Database::new();
    let store = db.open();
    assert!(store.list().unwrap().is_empty());
    let task = store.start("write", "/pool/TAPE01/甲").unwrap();
    assert!(task.id > 0);
    assert_eq!(task.operation, "write");
    assert_eq!(task.target, "/pool/TAPE01/甲");
    assert_eq!(task.state, TaskState::Running);
    assert_eq!(task.progress, 0);
    assert!(!task.cancel_requested);
    assert!(task.finished_at.is_none());
    chrono::DateTime::parse_from_rfc3339(&task.created_at).unwrap();
    drop(store);

    let store = db.open();
    assert_eq!(store.show(task.id).unwrap(), task);
    assert!(
        store
            .checkpoint(task.id, 40, "已写数据，尚未提交", None)
            .unwrap()
    );
    let outcome = json!({"commit": "confirmed", "generation": 12});
    let finished = store
        .finish(task.id, TaskState::Success, "提交完成", Some(&outcome))
        .unwrap();
    assert_eq!(finished.state, TaskState::Success);
    assert_eq!(finished.progress, 100);
    assert_eq!(finished.message, "提交完成");
    assert_eq!(finished.outcome, Some(outcome));
    assert!(finished.finished_at.is_some());
    assert!(finished.updated_at >= task.created_at);
    assert_eq!(finished.to_json()["state"], "success");
    drop(store);
    assert_eq!(db.open().list().unwrap(), vec![finished]);
}

#[test]
fn another_connection_requests_cancel_and_worker_stops_only_at_safe_checkpoint() {
    let db = Database::new();
    let worker = db.open();
    let task = worker.start("write", "/pool/TAPE01/file").unwrap();
    let effect = json!({"effect": "started", "commit": "unknown"});
    assert!(
        worker
            .checkpoint(task.id, 20, "已开始机械操作", Some(&effect))
            .unwrap()
    );

    let cli = db.open();
    assert!(cli.cancel(task.id).unwrap());
    assert!(cli.cancel(task.id).unwrap());
    let pending = db.open().show(task.id).unwrap();
    assert_eq!(pending.state, TaskState::Running);
    assert!(pending.cancel_requested);
    assert_eq!(pending.outcome, Some(effect.clone()));
    assert!(pending.finished_at.is_none());

    assert!(
        !worker
            .checkpoint(task.id, 30, "安全点取消，既有副作用保留", None)
            .unwrap()
    );
    let stopped = cli.show(task.id).unwrap();
    assert_eq!(stopped.state, TaskState::Running);
    assert!(stopped.finished_at.is_none());
    let cancelled = worker
        .finish(
            task.id,
            TaskState::Cancelled,
            "安全点取消，既有副作用保留",
            None,
        )
        .unwrap();
    assert_eq!(cancelled.state, TaskState::Cancelled);
    assert_eq!(cancelled.progress, 30);
    assert_eq!(cancelled.message, "安全点取消，既有副作用保留");
    assert_eq!(cancelled.outcome, Some(effect));
    assert!(cancelled.finished_at.is_some());
    assert!(!cli.cancel(task.id).unwrap());
    assert!(worker.checkpoint(task.id, 40, "不允许继续", None).is_err());
    assert!(
        worker
            .finish(task.id, TaskState::Success, "不能复活", None)
            .is_err()
    );
    assert_eq!(db.open().show(task.id).unwrap(), cancelled);
}

#[test]
fn cancellation_during_commit_cannot_replace_the_actual_commit_result() {
    let db = Database::new();
    let worker = db.open();
    let cli = db.open();
    for (state, commit) in [
        (TaskState::Success, "committed"),
        (TaskState::Failed, "unknown"),
    ] {
        let task = worker.start("write", "/pool/TAPE01/file").unwrap();
        assert!(!worker.cancel_requested(task.id).unwrap());
        worker
            .checkpoint(
                task.id,
                20,
                "即将 append",
                Some(&json!({"effect": "started"})),
            )
            .unwrap();
        assert!(cli.cancel(task.id).unwrap());
        assert!(worker.cancel_requested(task.id).unwrap());
        // append/commit 期间不检查取消，结束后记录真实结果。
        let actual = worker
            .finish(
                task.id,
                state,
                "提交结果",
                Some(&json!({"effect": "started", "commit": commit})),
            )
            .unwrap();
        assert_eq!(actual.state, state);
        assert_eq!(actual.outcome.as_ref().unwrap()["commit"], commit);
        assert!(actual.cancel_requested);
        assert!(!cli.cancel(task.id).unwrap());
        assert_eq!(db.open().show(task.id).unwrap(), actual);
    }
}

#[test]
fn orphan_recovery_requires_controller_lock_and_never_replays_operations() {
    let fixture = support::Fixture::new();
    let controller = fixture.open();
    let commands = fixture.changer.command_log();
    let worker = TaskStore::open(&fixture.path).unwrap();
    let task = worker.start("write", "/pool/TAPE01/file").unwrap();
    let outcome = json!({"effect": "started", "commit": "unknown"});
    worker
        .checkpoint(task.id, 60, "提交在途", Some(&outcome))
        .unwrap();
    worker.cancel(task.id).unwrap();
    let terminal = worker.start("read", "/pool/TAPE01/other").unwrap();
    let terminal = worker
        .finish(
            terminal.id,
            TaskState::Success,
            "读取完成",
            Some(&json!("verified")),
        )
        .unwrap();
    let cli = TaskStore::open(&fixture.path).unwrap();
    assert_eq!(cli.list().unwrap().len(), 2);
    assert!(cli.recover_interrupted("前实例已退出，结果须核验").is_err());
    assert_eq!(cli.show(task.id).unwrap().state, TaskState::Running);
    assert_eq!(fixture.changer.command_log(), commands);
    drop(worker);
    drop(controller);

    let reopened = TaskStore::open(&fixture.path).unwrap();
    assert_eq!(reopened.show(task.id).unwrap().state, TaskState::Running);
    assert_eq!(
        reopened
            .recover_interrupted("前实例已退出，结果须核验")
            .unwrap(),
        1
    );
    let interrupted = cli.show(task.id).unwrap();
    assert_eq!(interrupted.state, TaskState::Interrupted);
    assert_eq!(interrupted.progress, 60);
    assert_eq!(interrupted.outcome, Some(outcome));
    assert!(interrupted.cancel_requested);
    assert!(interrupted.finished_at.is_some());
    assert_eq!(interrupted.message, "前实例已退出，结果须核验");
    assert_eq!(reopened.show(terminal.id).unwrap(), terminal);
    assert_eq!(cli.recover_interrupted("不可重写终态").unwrap(), 0);
    assert_eq!(cli.show(task.id).unwrap(), interrupted);
    assert!(
        cli.finish(task.id, TaskState::Success, "禁止重放", None)
            .is_err()
    );
}

#[test]
fn clearing_history_preserves_running_requests_and_never_reuses_a_task_id() {
    let db = Database::new();
    let store = db.open();
    let running = store.start("write", "in-flight").unwrap();
    store.cancel(running.id).unwrap();
    let running = store.show(running.id).unwrap();
    let mut last_id = running.id;
    for state in [
        TaskState::Success,
        TaskState::Failed,
        TaskState::Interrupted,
        TaskState::Cancelled,
    ] {
        let task = store.start("operation", "terminal").unwrap();
        store
            .finish(task.id, state, "已结束", Some(&json!("设备结果另行核验")))
            .unwrap();
        last_id = task.id;
    }
    let cli = db.open();
    assert_eq!(cli.clear_history().unwrap(), 4);
    assert_eq!(cli.clear_history().unwrap(), 0);
    assert_eq!(db.open().list().unwrap(), vec![running.clone()]);
    assert_eq!(store.show(running.id).unwrap(), running);
    assert!(store.show(last_id).is_err());
    drop(store);
    let next = db.open().start("read", "next").unwrap();
    assert!(next.id > last_id, "清除历史后不能重用旧 ID");
}

#[test]
fn invalid_requests_leave_existing_records_unchanged() {
    let db = Database::new();
    let store = db.open();
    for operation in ["", " ", "\n\t"] {
        assert!(store.start(operation, "target").is_err());
    }
    assert!(store.list().unwrap().is_empty());
    let task = store.start("write", "target").unwrap();
    for invalid_progress in [101, 255] {
        assert!(
            store
                .checkpoint(task.id, invalid_progress, "不能写入", Some(&json!("bad")))
                .is_err()
        );
        assert_eq!(store.show(task.id).unwrap(), task);
    }
    assert!(
        store
            .finish(task.id, TaskState::Running, "不能结束为 running", None)
            .is_err()
    );
    assert_eq!(store.show(task.id).unwrap(), task);
    for unknown in [-1, 0, i64::MAX] {
        assert!(store.show(unknown).is_err());
        assert!(store.cancel(unknown).is_err());
        assert!(store.cancel_requested(unknown).is_err());
        assert!(store.checkpoint(unknown, 10, "未知任务", None).is_err());
        assert!(
            store
                .finish(unknown, TaskState::Failed, "未知任务", None)
                .is_err()
        );
    }
    assert_eq!(store.list().unwrap(), vec![task]);
}

#[test]
fn concurrent_connections_keep_unique_ids_and_clear_only_terminal_history() {
    let db = Database::new();
    let store = db.open();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(6));
    let workers: Vec<_> = (0..6)
        .map(|worker| {
            let path = db.path();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = TaskStore::open(&path).unwrap();
                barrier.wait();
                let mut ids = Vec::new();
                for index in 0..12 {
                    let task = store.start("write", &format!("{worker}/{index}")).unwrap();
                    ids.push(task.id);
                    assert!(
                        store
                            .checkpoint(
                                task.id,
                                25,
                                "阶段完成",
                                Some(&json!({"effect": "started"}))
                            )
                            .unwrap()
                    );
                    if index % 2 == 0 {
                        store
                            .finish(task.id, TaskState::Success, "已完成", None)
                            .unwrap();
                    } else {
                        assert!(store.cancel(task.id).unwrap());
                    }
                    store.clear_history().unwrap();
                }
                ids
            })
        })
        .collect();
    let mut ids: Vec<_> = workers
        .into_iter()
        .flat_map(|worker| worker.join().unwrap())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 72);
    let remaining = db.open().list().unwrap();
    assert_eq!(remaining.len(), 36);
    for task in remaining {
        assert_eq!(task.state, TaskState::Running);
        assert!(task.cancel_requested);
        assert_eq!(task.progress, 25);
        assert_eq!(task.outcome, Some(json!({"effect": "started"})));
        assert!(task.finished_at.is_none());
    }
    assert_eq!(store.recover_interrupted("并发执行方已退出").unwrap(), 36);
    assert_eq!(store.clear_history().unwrap(), 36);
    assert!(store.list().unwrap().is_empty());
}

#[test]
fn concurrent_finish_and_cancel_have_one_immutable_terminal_result() {
    let db = Database::new();
    let store = db.open();
    for _ in 0..12 {
        let task = store.start("write", "target").unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let finishers: Vec<_> = [TaskState::Success, TaskState::Failed]
            .into_iter()
            .map(|state| {
                let path = db.path();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let store = TaskStore::open(&path).unwrap();
                    barrier.wait();
                    store.finish(
                        task.id,
                        state,
                        state.as_str(),
                        Some(&json!({"result": state.as_str()})),
                    )
                })
            })
            .collect();
        let path = db.path();
        let cancel_barrier = barrier.clone();
        let cancellation = std::thread::spawn(move || {
            let store = TaskStore::open(&path).unwrap();
            cancel_barrier.wait();
            store.cancel(task.id).unwrap()
        });
        barrier.wait();
        let results: Vec<_> = finishers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        cancellation.join().unwrap();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let winner = results.into_iter().find_map(Result::ok).unwrap();
        assert_eq!(store.show(task.id).unwrap(), winner);
        assert!(winner.finished_at.is_some());
        assert!(!store.cancel(task.id).unwrap());
        assert!(store.checkpoint(task.id, 90, "不能覆盖", None).is_err());
        assert_eq!(db.open().show(task.id).unwrap(), winner);
    }
}

#[test]
fn cancellation_waits_for_a_competing_sqlite_writer_and_remains_durable() {
    use std::sync::mpsc;
    use std::time::Duration;

    let db = Database::new();
    let store = db.open();
    let task = store.start("write", "target").unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let path = db.path();
    let cli = std::thread::spawn(move || {
        let store = TaskStore::open(&path).unwrap();
        ready_tx.send(()).unwrap();
        go_rx.recv().unwrap();
        result_tx.send(store.cancel(task.id)).unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // 数据库写锁是外部竞争条件；结果始终通过 TaskStore 公开接口验证。
    let competing = rusqlite::Connection::open(db.path()).unwrap();
    competing.execute_batch("BEGIN IMMEDIATE").unwrap();
    go_tx.send(()).unwrap();
    let waiting = result_rx.recv_timeout(Duration::from_millis(150));
    competing.execute_batch("COMMIT").unwrap();
    assert!(
        matches!(waiting, Err(mpsc::RecvTimeoutError::Timeout)),
        "写锁释放前不应返回 BUSY 或提前成功：{waiting:?}"
    );
    assert!(
        result_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
    );
    cli.join().unwrap();
    assert!(db.open().cancel_requested(task.id).unwrap());
    assert_eq!(store.show(task.id).unwrap().state, TaskState::Running);
}

#[test]
fn journal_survives_process_exit_without_replaying_or_completing_the_job() {
    const CHILD_DATABASE: &str = "TAPE_RS_TASK_STORE_EXIT_TEST_DATABASE";
    if let Some(path) = std::env::var_os(CHILD_DATABASE) {
        let store = TaskStore::open(std::path::Path::new(&path)).unwrap();
        let task = store.start("write", "/pool/TAPE01/crashed").unwrap();
        store
            .checkpoint(
                task.id,
                65,
                "提交结果尚未核验",
                Some(&json!({"effect": "started", "commit": "unknown"})),
            )
            .unwrap();
        // 绕过析构：没有 finish，也没有连接正常关闭时的 WAL 清理。
        std::process::exit(23);
    }

    let db = Database::new();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "journal_survives_process_exit_without_replaying_or_completing_the_job",
            "--nocapture",
        ])
        .env(CHILD_DATABASE, db.path())
        .output()
        .unwrap();
    assert_eq!(
        child.status.code(),
        Some(23),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    let store = db.open();
    let tasks = store.list().unwrap();
    assert_eq!(tasks.len(), 1);
    let task = &tasks[0];
    assert_eq!(task.operation, "write");
    assert_eq!(task.target, "/pool/TAPE01/crashed");
    assert_eq!(task.state, TaskState::Running);
    assert_eq!(task.progress, 65);
    assert_eq!(
        task.outcome,
        Some(json!({"effect": "started", "commit": "unknown"}))
    );
    assert!(task.finished_at.is_none());
    assert_eq!(db.open().list().unwrap(), tasks);
    assert_eq!(store.recover_interrupted("持锁后核对遗留任务").unwrap(), 1);
    let interrupted = db.open().show(task.id).unwrap();
    assert_eq!(interrupted.state, TaskState::Interrupted);
    assert_eq!(interrupted.outcome, task.outcome);
    assert_eq!(interrupted.progress, 65);
    assert!(interrupted.finished_at.is_some());
    assert_eq!(store.recover_interrupted("重复恢复不能改终态").unwrap(), 0);
    assert_eq!(store.show(task.id).unwrap(), interrupted);
}
