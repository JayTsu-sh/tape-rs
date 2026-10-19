//! EE 对象恢复的公共 API 测试；只使用本机临时目录和模拟卷。
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tape_rs::ltfs::ee::{EeManifest, EeObjectKind};
use tape_rs::ltfs::index::{DirectoryNode, FileNode, LtfsIndex};

// Oracle: .scratch/ltfs-ha/probes/results/ee-basic-interop-20261016/
// RC0017L9-after-native-ee-mount.schema (IBM EE/LE，generation 12)。
// SHA256: d6ca6aa48414c1e4d555b2054602c98e756be029a402307702ff9c1aba87ca1c
const NATIVE: &[u8] = include_bytes!("fixtures/ee-rc17-generation12.schema");

#[test]
fn actual_ee_schema_exposes_saved_objects_instead_of_raw_ltfs_types() {
    let index = LtfsIndex::parse(NATIVE).unwrap();
    let before = index.to_xml().unwrap();
    let manifest = EeManifest::from_index(&index).unwrap();
    assert_eq!(manifest.objects().len(), 5);
    let directory = manifest
        .objects()
        .iter()
        .find(|o| o.saved_path.ends_with("/empty-dir"))
        .unwrap();
    assert_eq!(directory.kind, EeObjectKind::Directory);
    assert!(directory.aliases.is_empty());
    let link = manifest
        .objects()
        .iter()
        .find(|o| o.saved_path.ends_with("/marker-link"))
        .unwrap();
    assert_eq!(
        link.kind,
        EeObjectKind::Symlink {
            target: "../../ee_rea_20261015/migration-marker.bin".into()
        }
    );
    let marker = manifest
        .objects()
        .iter()
        .find(|o| o.length == 1048576)
        .unwrap();
    assert_eq!(marker.kind, EeObjectKind::RegularFile);
    assert_eq!(
        marker.aliases,
        ["gpfs/ee_rea_20261016/reimport/RC0017L9/gpfs/ee_rea_20261015/migration-marker.bin"]
    );
    assert!(
        manifest
            .objects()
            .iter()
            .all(|o| o.object_path.starts_with(".LTFSEE_DATA/"))
    );
    assert_eq!(
        index.to_xml().unwrap(),
        before,
        "读取视图不能改写 objgen 或其他索引元数据"
    );
}

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = std::env::temp_dir().join(format!(
                "tape-rs-ee-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("临时目录: {e}"),
            }
        }
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn restores_actual_saved_directory_regular_files_and_literal_links() {
    let manifest = EeManifest::from_index(&LtfsIndex::parse(NATIVE).unwrap()).unwrap();
    let temp = TestDir::new();
    let destination = temp.path().join("restore");
    let report = manifest
        .restore_with(&destination, |_path, out| {
            let block = [0x5a; 16384];
            for _ in 0..64 {
                out.write_all(&block)?;
            }
            Ok(1048576)
        })
        .unwrap();
    let saved = destination.join("gpfs/ee_rea_20261016/reimport/RC0017L9/gpfs");
    let fixtures = saved.join("ee_rea_20261016/save-fixtures");
    assert!(fixtures.join("empty-dir").is_dir());
    assert_eq!(
        std::fs::read(fixtures.join("empty with space")).unwrap(),
        b""
    );
    assert_eq!(
        std::fs::read(saved.join("ee_rea_20261015/migration-marker.bin")).unwrap(),
        vec![0x5a; 1048576]
    );
    assert_eq!(
        std::fs::read_link(fixtures.join("marker-link")).unwrap(),
        Path::new("../../ee_rea_20261015/migration-marker.bin")
    );
    assert_eq!(
        std::fs::read_link(fixtures.join("missing-link")).unwrap(),
        Path::new("does-not-exist")
    );
    assert_eq!(
        (
            report.directories,
            report.regular_files,
            report.symlinks,
            report.bytes_written
        ),
        (1, 2, 2, 1048576)
    );
    assert!(!destination.join(".LTFSEE_DATA").exists());
}

fn data_mut(index: &mut LtfsIndex) -> &mut DirectoryNode {
    index
        .root
        .subdirs
        .iter_mut()
        .find(|d| d.name == ".LTFSEE_DATA")
        .unwrap()
}

#[test]
fn rejects_malformed_saved_type_before_creating_destination() {
    let mut index = LtfsIndex::parse(NATIVE).unwrap();
    data_mut(&mut index).files[0].set_xattr("ibm.ltfsee.gpfs.objtype", "dir");
    let temp = TestDir::new();
    let destination = temp.path().join("restore");
    let result = EeManifest::from_index(&index)
        .and_then(|m| m.restore_with(&destination, |_, _| panic!("类型错误必须在读取前拒绝")));
    assert!(result.is_err(), "带数据的 regfile 不能解释成保存目录");
    assert!(!destination.exists());
}

fn objects_only_index() -> LtfsIndex {
    let mut index = LtfsIndex::parse(NATIVE).unwrap();
    index.root.subdirs.retain(|d| d.name == ".LTFSEE_DATA");
    index
}

#[test]
fn rejects_ambiguous_or_unsafe_raw_index_names() {
    for name in ["", ".", "..", "a/b", "a\0b"] {
        let mut index = objects_only_index();
        data_mut(&mut index).files[0].name = name.into();
        assert!(
            EeManifest::from_index(&index).is_err(),
            "原始索引组件必须拒绝 {name:?}"
        );
    }
    let mut index = objects_only_index();
    let duplicate = data_mut(&mut index).files[0].clone();
    data_mut(&mut index).files.push(duplicate);
    assert!(
        EeManifest::from_index(&index).is_err(),
        "重复原始路径不能覆盖前一对象"
    );
}

#[test]
fn unrelated_native_dangling_external_and_cyclic_links_do_not_block_ee_view() {
    let mut index = LtfsIndex::parse(NATIVE).unwrap();
    for (name, target) in [
        ("native-dangling", "missing"),
        ("native-outside", "/outside"),
        ("native-a", "native-b"),
        ("native-b", "native-a"),
    ] {
        index.root.files.push(FileNode {
            name: name.into(),
            symlink: Some(target.into()),
            ..Default::default()
        });
    }
    let manifest = EeManifest::from_index(&index).unwrap();
    assert_eq!(manifest.objects().len(), 5);
    assert!(
        manifest
            .objects()
            .iter()
            .all(|o| o.aliases.iter().all(|a| !a.starts_with("native-")))
    );
}

#[test]
fn ee_aliases_chain_from_the_root_user_namespace() {
    let mut index = objects_only_index();
    let object_path = format!(".LTFSEE_DATA/{}", data_mut(&mut index).files[0].name);
    index.root.files.extend([
        FileNode {
            name: "root-alias".into(),
            symlink: Some("second-alias".into()),
            ..Default::default()
        },
        FileNode {
            name: "second-alias".into(),
            symlink: Some(object_path),
            ..Default::default()
        },
    ]);
    let manifest = EeManifest::from_index(&index).unwrap();
    let marker = manifest
        .objects()
        .iter()
        .find(|o| o.length == 1048576)
        .unwrap();
    assert_eq!(marker.aliases, ["root-alias", "second-alias"]);
}

#[test]
fn ee_raw_aliases_reject_cycles_escapes_absolute_and_missing_targets() {
    for target in [
        "alias",
        "../.LTFSEE_DATA/object",
        "/.LTFSEE_DATA/object",
        ".LTFSEE_DATA/missing",
    ] {
        let mut index = objects_only_index();
        let mut alias = FileNode {
            name: "alias".into(),
            symlink: Some(target.into()),
            ..Default::default()
        };
        alias.set_xattr("ltfs.vendor.IBM.prefixLength", "0");
        index.root.files.push(alias);
        assert!(
            EeManifest::from_index(&index).is_err(),
            "不安全原始别名: {target}"
        );
    }
}

#[test]
fn saved_link_target_is_literal_data_even_when_absolute_or_outside() {
    for target in [
        "/outside/dangling",
        "../../../../outside",
        "does-not-exist",
        "./a//b/../c",
    ] {
        let mut index = objects_only_index();
        data_mut(&mut index)
            .files
            .retain(|f| f.xattr("ibm.ltfsee.gpfs.objtype") == Some("symlink"));
        data_mut(&mut index).files.truncate(1);
        let file = &mut data_mut(&mut index).files[0];
        file.set_xattr("ibm.ltfsee.gpfs.path", "/saved-link");
        file.set_xattr("ibm.ltfsee.gpfs.target", target);
        let manifest = EeManifest::from_index(&index).unwrap();
        let temp = TestDir::new();
        let report = manifest
            .restore_with(temp.path(), |_, _| panic!("保存链接不能读取目标"))
            .unwrap();
        assert_eq!(report.symlinks, 1);
        assert_eq!(
            std::fs::read_link(temp.path().join("saved-link"))
                .unwrap()
                .as_os_str(),
            target
        );
    }
}

#[test]
fn destination_symlinks_files_or_nonempty_directories_are_never_overwritten() {
    let manifest = EeManifest::from_index(&LtfsIndex::parse(NATIVE).unwrap()).unwrap();
    let temp = TestDir::new();
    let outside = temp.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let file = temp.path().join("file");
    std::fs::write(&file, b"keep").unwrap();
    let nonempty = temp.path().join("nonempty");
    std::fs::create_dir(&nonempty).unwrap();
    std::os::unix::fs::symlink(&outside, nonempty.join("existing-link")).unwrap();
    for destination in [&link, &link.join("child"), &file, &nonempty] {
        assert!(
            manifest
                .restore_with(destination, |_, _| panic!("拒绝的目标不得读取数据"))
                .is_err()
        );
    }
    assert_eq!(std::fs::read(&file).unwrap(), b"keep");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&nonempty).unwrap().count(), 1);
}

#[test]
fn path_collisions_traversal_and_nondirectory_parents_fail_before_creation() {
    for path in [
        "/../escape",
        "/a/../../escape",
        "/a/./b",
        "//absolute",
        "/",
        "relative",
        "/a\0b",
        "/a//b",
        "/same",
        "/same/child",
    ] {
        let mut index = objects_only_index();
        data_mut(&mut index).files[0].set_xattr("ibm.ltfsee.gpfs.path", "/same");
        data_mut(&mut index).files[1].set_xattr("ibm.ltfsee.gpfs.path", path);
        let temp = TestDir::new();
        let destination = temp.path().join("restore");
        let result = EeManifest::from_index(&index)
            .and_then(|m| m.restore_with(&destination, |_, _| panic!("路径错误必须在读取前拒绝")));
        assert!(result.is_err(), "不安全保存路径: {path:?}");
        assert!(!destination.exists(), "预检失败不能创建目标: {path:?}");
    }
}

#[test]
fn base64_semantic_attributes_are_decoded_without_rewriting_raw_attributes() {
    let mut index = objects_only_index();
    data_mut(&mut index)
        .files
        .retain(|f| f.xattr("ibm.ltfsee.gpfs.objtype") == Some("symlink"));
    data_mut(&mut index).files.truncate(1);
    let file = &mut data_mut(&mut index).files[0];
    for (key, value) in [
        ("ibm.ltfsee.gpfs.objtype", "c3ltbGluaw=="),
        ("ibm.ltfsee.gpfs.path", "L3NhdmVkLWxpbms="),
        ("ibm.ltfsee.gpfs.target", "Li4vLi4vb3V0c2lkZQ=="),
    ] {
        file.set_xattr(key, value);
        file.xattrs
            .iter_mut()
            .find(|x| x.key == key)
            .unwrap()
            .base64 = true;
    }
    let original = file.xattrs.clone();
    let manifest = EeManifest::from_index(&index).unwrap();
    assert_eq!(manifest.objects()[0].saved_path, "/saved-link");
    assert_eq!(
        manifest.objects()[0].kind,
        EeObjectKind::Symlink {
            target: "../../outside".into()
        }
    );
    assert_eq!(manifest.objects()[0].xattrs, original);
}

#[test]
fn incomplete_or_corrupt_ee_metadata_is_not_silently_skipped() {
    let mut index = objects_only_index();
    data_mut(&mut index).files[0]
        .xattrs
        .retain(|x| x.key != "ibm.ltfsee.gpfs.objtype");
    assert!(
        EeManifest::from_index(&index).is_err(),
        "EE 保存路径存在时不能忽略缺失的 objtype"
    );
    for key in ["ibm.ltfsee.gpfs.objtype", "ibm.ltfsee.gpfs.path"] {
        let mut index = objects_only_index();
        let file = &mut data_mut(&mut index).files[0];
        let attr = file.xattrs.iter().find(|x| x.key == key).unwrap().clone();
        file.xattrs.push(attr);
        assert!(
            EeManifest::from_index(&index).is_err(),
            "重复语义字段: {key}"
        );
        let mut index = objects_only_index();
        let attr = data_mut(&mut index).files[0]
            .xattrs
            .iter_mut()
            .find(|x| x.key == key)
            .unwrap();
        attr.base64 = true;
        attr.value = "!!!!".into();
        assert!(
            EeManifest::from_index(&index).is_err(),
            "损坏语义字段: {key}"
        );
    }
}
