//! 普通管理入口不可绕过 EE 对象保护；只接触模拟 transport。
#[path = "support/library_fixture.rs"]
mod support;
use tape_rs::library::LibraryBackend;
use tape_rs::tapefs::Backend;

#[test]
fn fuse_metadata_operations_cannot_delete_or_reparent_ee_aliases() {
    let fixture = support::Fixture::new();
    let mut lib = fixture.formatted();
    lib.with_volume("/pool/LIB001L8", |v, _| {
        v.append_bytes(".LTFSEE_DATA/object", b"EE data")?;
        v.add_symlink("gpfs/original", "../.LTFSEE_DATA/object")
    })
    .unwrap();
    let shared = std::sync::Arc::new(std::sync::Mutex::new(lib));
    let backend = LibraryBackend(shared.clone());
    assert!(backend.delete("/pool/LIB001L8/gpfs/original").is_err());
    assert!(
        backend
            .rename("/pool/LIB001L8/gpfs", "/pool/LIB001L8/renamed", false)
            .is_err()
    );
    assert!(
        backend
            .change_xattr("/pool/LIB001L8/gpfs/original", "user.custom", Some(b"x"), 0)
            .is_err()
    );
    backend.mkdir("/pool/LIB001L8/ordinary").unwrap();
    let mut guard = shared.lock().unwrap();
    let mut data = Vec::new();
    guard
        .read_to("/pool/LIB001L8/gpfs/original", &mut data)
        .unwrap();
    assert_eq!(data, b"EE data");
    guard.close().unwrap();
}
