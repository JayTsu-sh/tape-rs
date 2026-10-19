//! 真实调度调用必须执行角色约束，不能只有配置字段。
#[path = "support/library_fixture.rs"]
mod support;

#[test]
fn read_only_role_rejects_writes_before_moving_a_tape() {
    let f = support::Fixture::new();
    let mut lib = f.formatted();
    lib.write_from("/pool/LIB001L8/file", &mut &b"payload"[..], true)
        .unwrap();
    lib.drive_set_role("SIMDRV0000", "r").unwrap();
    let before = f.changer.command_log().len();
    assert!(
        lib.write_from("/pool/LIB002L8/new", &mut &b"no"[..], true)
            .is_err()
    );
    assert!(!f.changer.command_log()[before..].contains(&0xa5));
    let mut output = Vec::new();
    lib.read_to("/pool/LIB001L8/file", &mut output).unwrap();
    assert_eq!(output, b"payload");
    lib.close().unwrap();
}
