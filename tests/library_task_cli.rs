//! 任务管理不打开设备；默认测试只用模拟器创建资源库。
#[allow(dead_code)]
#[path = "support/library_fixture.rs"]
mod support;

#[test]
fn task_queries_only_require_the_state_file_even_while_controller_is_active() {
    let fixture = support::Fixture::new();
    let _controller = fixture.open();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_tape-library"))
        .arg("--library-state")
        .arg(&fixture.path)
        .args(["task", "list"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let tasks: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(tasks, serde_json::json!([]));
}

#[test]
fn cancellation_before_dispatch_does_not_touch_media_and_survives_restart() {
    use tape_rs::library::tasks::{TaskState, TaskStore};
    let f = support::Fixture::new();
    let mut lib = f.formatted();
    let id = lib.begin_task("write", "/pool/LIB001L8/new").unwrap();
    let store = TaskStore::open(&f.path).unwrap();
    assert!(store.cancel(id).unwrap());
    let before = f.changer.command_log().len();
    assert!(
        lib.write_from("/pool/LIB001L8/new", &mut &b"no"[..], true)
            .is_err()
    );
    assert!(!f.changer.command_log()[before..].contains(&0xa5));
    lib.close().unwrap();
    assert_eq!(
        lib.finish_task(false, "安全点取消").unwrap().unwrap()["state"],
        "cancelled"
    );
    drop(lib);
    assert_eq!(store.show(id).unwrap().state, TaskState::Cancelled);
    let mut lib = f.open();
    let id = lib.begin_task("read", "/pool/LIB001L8/file").unwrap();
    drop(lib); // 异常退出，不伪装成成功，也不重放。
    assert_eq!(store.show(id).unwrap().state, TaskState::Running);
    let _new_controller = f.open();
    assert_eq!(store.show(id).unwrap().state, TaskState::Interrupted);
}

#[test]
fn cancel_after_append_starts_does_not_interrupt_or_hide_the_full_checkpoint() {
    use std::io::Read;
    use tape_rs::library::tasks::{TaskState, TaskStore};
    struct Input {
        data: std::io::Cursor<Vec<u8>>,
        tasks: TaskStore,
        id: i64,
    }
    impl Read for Input {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.tasks.cancel(self.id).unwrap();
            self.data.read(bytes)
        }
    }
    let f = support::Fixture::new();
    let mut lib = f.formatted();
    let id = lib.begin_task("write", "/pool/LIB001L8/file").unwrap();
    let mut source = Input {
        data: std::io::Cursor::new(vec![0x5a; 131073]),
        tasks: TaskStore::open(&f.path).unwrap(),
        id,
    };
    lib.write_from("/pool/LIB001L8/file", &mut source, true)
        .unwrap();
    lib.close().unwrap();
    lib.finish_task(true, "完整提交完成").unwrap();
    let task = source.tasks.show(id).unwrap();
    assert_eq!(task.state, TaskState::Success);
    assert!(task.cancel_requested);
    assert_eq!(task.outcome.unwrap()["commit"], "full_checkpoint");
    drop(lib);
    let mut lib = f.open();
    let mut data = Vec::new();
    lib.read_to("/pool/LIB001L8/file", &mut data).unwrap();
    assert_eq!(data, vec![0x5a; 131073]);
    lib.close().unwrap();
}
