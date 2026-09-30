//! 纯内存增量索引基准：不访问设备，校验差分重放与原视图一致。
use std::time::Instant;
use tape_rs::ltfs::index::{FileNode, IndexLocation, LtfsIndex};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let count: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "10000".into())
        .parse()?;
    let mut base = LtfsIndex::empty(uuid::Uuid::new_v4(), "delta-perf".into(), 'b');
    base.self_location = IndexLocation {
        partition: 'b',
        start_block: 5,
    };
    for i in 0..count {
        let mut meta = base.root.meta.clone();
        meta.file_uid = i as u64 + 2;
        base.root.files.push(FileNode {
            name: format!("f{i:08}"),
            meta,
            ..Default::default()
        });
    }
    base.highest_file_uid = count as u64 + 1;
    let mut working = base.clone();
    working.generation += 1;
    working.self_location.start_block = 100;
    if let Some(file) = working.root.files.last_mut() {
        file.set_xattr("benchmark", "changed");
    }
    let start = Instant::now();
    let delta = working.incremental_since(&base)?;
    let seconds = start.elapsed().as_secs_f64();
    let xml = delta.to_xml()?;
    let replayed = base.apply_incremental(&LtfsIndex::parse(&xml)?)?;
    assert_eq!(replayed.root, working.root);
    println!(
        "{}",
        serde_json::json!({"files":count,"changed":delta.root.files.len(),"seconds":seconds,"xml_bytes":xml.len(),"replay_verified":true})
    );
    Ok(())
}
