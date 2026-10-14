//! 实机（Holo-VTL 专属库 TAPERS）上的 D02 恢复场景，镜像 `tests/sim_recovery.rs`。
//!
//! **破坏性**：每个用例都会对驱动器内的磁带执行 mkltfs。做成 example 而不是 `#[test]`：libtest 依赖 GLIBC_2.39 的 `pidfd_spawnp`，
//! 无法在 Rocky 9.4（glibc 2.34）上运行。只有在
//! 设置了以下环境变量、且驱动器 VPD 0x80 序列号与期望值完全一致时才会运行：
//!
//! ```text
//! TAPE_RS_HW_DRIVE=/dev/sg6 TAPE_RS_HW_SERIAL=IBMtaper2287 \
//!   ./hw_tapers            # cargo build --release --example hw_tapers
//! ```
//!
//! 前置：目标磁带已装入该驱动器；不得指向 EE 使用的驱动器。真实设备没有"掉电丢缓冲"
//! 的注入手段，这里用库 API 直接写出各类尾部（未索引数据、残缺索引、假索引、IP 落后）。

use std::io::Cursor;

use tape_rs::ltfs::index::LtfsIndex;
use tape_rs::ltfs::mkltfs::{MkltfsOptions, mkltfs};
use tape_rs::ltfs::recovery::TailKind;
use tape_rs::ltfs::volume::LtfsVolume;
use tape_rs::scsi::device::ScsiDevice;
use tape_rs::scsi::inquiry::read_unit_serial;
use tape_rs::tape::commands::TapeDrive;

const BLOCK: u32 = 512 * 1024;

fn open_guarded() -> Option<ScsiDevice> {
    let path = std::env::var("TAPE_RS_HW_DRIVE").ok()?;
    let expect = std::env::var("TAPE_RS_HW_SERIAL").ok()?;
    let dev = ScsiDevice::open(&path).expect("open drive");
    let serial = read_unit_serial(&dev).unwrap_or_default();
    assert_eq!(
        serial, expect,
        "驱动器序列号守卫：{} 的序列号是 {:?}，期望 {:?}，拒绝运行",
        path, serial, expect
    );
    Some(dev)
}

fn fresh(dev: &ScsiDevice) {
    let opts = MkltfsOptions {
        volume_id: "TR8000".into(),
        block_size: BLOCK,
        ..Default::default()
    };
    mkltfs(dev, &opts).expect("mkltfs");
}

fn payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed))
        .collect()
}

fn commit_file(dev: &ScsiDevice, path: &str, data: &[u8]) -> u64 {
    let mut vol = LtfsVolume::mount(dev).expect("mount");
    assert!(vol.writable(), "{:?}", vol.recovery().notes);
    vol.append_file(path, &mut Cursor::new(data)).unwrap();
    vol.commit().expect("commit");
    vol.index().generation
}

fn read(vol: &LtfsVolume<'_>, path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    vol.read_file_to_writer(path, &mut out).unwrap();
    out
}

fn hw01_fresh_volume_complete_writable_fast_path(dev: &ScsiDevice) {
    fresh(dev);
    let vol = LtfsVolume::mount(dev).unwrap();
    let r = vol.recovery();
    assert_eq!(
        (r.dp.tail, r.ip.tail),
        (TailKind::Complete, TailKind::Complete),
        "{:?} {:?}",
        r.dp.notes,
        r.ip.notes
    );
    assert!(r.chain_ok && vol.writable());
    assert_eq!(vol.index().generation, 1);
    assert!(
        r.dp.hint_used && r.ip.hint_used,
        "VCI 快速路径应命中: {:?}",
        r.notes
    );
    assert_eq!((r.dp.back_steps, r.ip.back_steps), (0, 0));
}

fn hw02_two_commits_chain_and_content(dev: &ScsiDevice) {
    fresh(dev);
    let a = payload(3 * 1024 * 1024 + 123, 1);
    let b = payload(700_000, 2);
    assert_eq!(commit_file(dev, "/d/a.bin", &a), 2);
    assert_eq!(commit_file(dev, "/d/b.bin", &b), 3);
    let vol = LtfsVolume::mount(dev).unwrap();
    let r = vol.recovery();
    assert_eq!(vol.index().generation, 3);
    assert_eq!(r.chain_depth, 2);
    assert!(r.chain_ok && !r.ip_debt && vol.writable());
    assert!(r.dp.hint_used && r.ip.hint_used, "{:?}", r.notes);
    assert_eq!(read(&vol, "d/a.bin"), a);
    assert_eq!(read(&vol, "d/b.bin"), b);
}

fn hw03_unindexed_data_tail_is_read_only_and_vci_stale(dev: &ScsiDevice) {
    fresh(dev);
    let a = payload(400_000, 3);
    commit_file(dev, "/committed.bin", &a);
    {
        let mut vol = LtfsVolume::mount(dev).unwrap();
        vol.append_file("/lost.bin", &mut Cursor::new(payload(900_000, 4)))
            .unwrap();
        // 不 commit：数据落带但没有索引
    }
    let vol = LtfsVolume::mount(dev).unwrap();
    let r = vol.recovery();
    assert_eq!(r.dp.tail, TailKind::UnindexedData, "{:?}", r.dp.notes);
    assert!(!r.dp.hint_used, "写入改变了 VCR，VCI 应过期");
    assert!(!vol.writable());
    assert_eq!(vol.index().generation, 2);
    assert_eq!(vol.list().len(), 1);
    assert_eq!(read(&vol, "committed.bin"), a);
}

fn hw04_truncated_index_tail(dev: &ScsiDevice) {
    fresh(dev);
    commit_file(dev, "/first.bin", &payload(300_000, 5));
    let eod = LtfsVolume::mount(dev).unwrap().recovery().dp.eod;
    let drive = TapeDrive::new(dev);
    drive.locate(1, eod, true).unwrap();
    drive.write_filemark(1).unwrap();
    drive
        .write_block(b"<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ltfsindex version=\"2.4.0\">\n  <creator>interrupted")
        .unwrap();
    drive.write_filemark(0).unwrap(); // 仅刷缓冲，不写结束文件标记
    let vol = LtfsVolume::mount(dev).unwrap();
    let r = vol.recovery();
    assert_eq!(r.dp.tail, TailKind::TruncatedIndex, "{:?}", r.dp.notes);
    assert_eq!(vol.index().generation, 2);
    assert!(!vol.writable());
}

fn hw05_fake_index_with_wrong_self_pointer(dev: &ScsiDevice) {
    fresh(dev);
    commit_file(dev, "/first.bin", &payload(300_000, 6));
    let (eod, xml) = {
        let vol = LtfsVolume::mount(dev).unwrap();
        (vol.recovery().dp.eod, vol.index().to_xml().unwrap())
    };
    assert!(LtfsIndex::parse(&xml).is_ok());
    let drive = TapeDrive::new(dev);
    drive.locate(1, eod, true).unwrap();
    drive.write_filemark(1).unwrap();
    drive.write_block(&xml).unwrap(); // 自指针仍指向旧位置
    drive.write_filemark(1).unwrap();
    let vol = LtfsVolume::mount(dev).unwrap();
    let r = vol.recovery();
    assert_eq!(r.dp.tail, TailKind::FakeIndex, "{:?}", r.dp.notes);
    assert_eq!(r.dp.back_steps, 3, "假索引、空区域、真索引");
    assert_eq!(vol.index().generation, 2);
    assert!(!vol.writable());
}

fn hw06_ip_lagging_dp_is_debt_and_still_writable(dev: &ScsiDevice) {
    fresh(dev);
    commit_file(dev, "/a.bin", &payload(200_000, 7));
    // 取 IP 上 gen 2 的索引记录
    let drive = TapeDrive::new(dev);
    let mut buf = vec![0u8; BLOCK as usize];
    // IP 内容区：块 4 是 Index Construct 的开头文件标记，索引在块 5
    drive.locate(0, 5, true).unwrap();
    let n = drive.read_block(&mut buf).unwrap();
    let ip_gen2 = buf[..n].to_vec();
    assert_eq!(LtfsIndex::parse(&ip_gen2).unwrap().generation, 2);
    let b = payload(250_000, 8);
    commit_file(dev, "/b.bin", &b);
    // 把 IP 写回 gen 2（模拟 IP 维护落后于 DP）
    drive.locate(0, 4, true).unwrap();
    drive.write_filemark(1).unwrap();
    drive.write_block(&ip_gen2).unwrap();
    drive.write_filemark(1).unwrap();
    let vol = LtfsVolume::mount(dev).unwrap();
    let r = vol.recovery();
    assert!(r.ip_debt, "{:?}", r.notes);
    assert_eq!(vol.index().generation, 3, "视图来自 DP");
    assert_eq!(r.ip.last_index.as_ref().unwrap().index.generation, 2);
    assert_eq!(read(&vol, "b.bin"), b);
    assert!(vol.writable());
    // 在债务状态下继续提交，IP 被补齐
    drop(vol);
    assert_eq!(commit_file(dev, "/c.bin", &payload(10_000, 9)), 4);
    let vol = LtfsVolume::mount(dev).unwrap();
    assert!(!vol.recovery().ip_debt);
    assert_eq!(vol.list().len(), 3);
}

type Scenario = (&'static str, fn(&ScsiDevice));

fn main() {
    let Some(dev) = open_guarded() else {
        eprintln!("跳过：未设置 TAPE_RS_HW_DRIVE / TAPE_RS_HW_SERIAL");
        std::process::exit(2);
    };
    let scenarios: &[Scenario] = &[
        (
            "hw01_fresh_volume_complete_writable_fast_path",
            hw01_fresh_volume_complete_writable_fast_path,
        ),
        (
            "hw02_two_commits_chain_and_content",
            hw02_two_commits_chain_and_content,
        ),
        (
            "hw03_unindexed_data_tail_is_read_only_and_vci_stale",
            hw03_unindexed_data_tail_is_read_only_and_vci_stale,
        ),
        ("hw04_truncated_index_tail", hw04_truncated_index_tail),
        (
            "hw05_fake_index_with_wrong_self_pointer",
            hw05_fake_index_with_wrong_self_pointer,
        ),
        (
            "hw06_ip_lagging_dp_is_debt_and_still_writable",
            hw06_ip_lagging_dp_is_debt_and_still_writable,
        ),
    ];
    let filter = std::env::args().nth(1);
    let mut failed = 0;
    for (name, f) in scenarios {
        if let Some(pat) = &filter
            && !name.contains(pat.as_str())
        {
            continue;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&dev)));
        match result {
            Ok(()) => println!("PASS {}", name),
            Err(_) => {
                failed += 1;
                println!("FAIL {}", name);
            }
        }
    }
    println!("{} scenarios, {} failed", scenarios.len(), failed);
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
