use super::*;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;

#[test]
fn raw_spool_is_anonymous_and_enforces_reserved_bytes() {
    let directory = std::env::temp_dir().join(format!("c1-spool-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let budget = crate::local_read_storage::ReadStorageBudget {
        download_bytes: 3,
        headroom_bytes: 0,
        ..Default::default()
    };
    assert!(RawBodySpool::new(&directory, 4, budget).is_err());
    assert!(RawBodySpool::new(&directory.join("missing"), 3, budget).is_err());
    let mut spool = RawBodySpool::new(&directory, 3, budget).unwrap();
    assert_eq!(spool.file.metadata().unwrap().nlink(), 0);
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    spool.write_all(b"abc").unwrap();
    assert!(spool.write_all(b"d").is_err());
    assert_eq!(spool.written, 3);
    assert_eq!(spool.file.metadata().unwrap().len(), 3);
    spool.file.rewind().unwrap();
    let mut bytes = [0; 3];
    spool.file.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"abc");
    drop(spool);
    std::fs::remove_dir(directory).unwrap();
}

#[test]
fn raw_spool_streams_above_eight_mib_with_fixed_memory() {
    let directory = std::env::temp_dir().join(format!("c1-large-spool-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let length = (8 << 20) + 17;
    let budget = crate::local_read_storage::ReadStorageBudget {
        download_bytes: length,
        headroom_bytes: 0,
        ..Default::default()
    };
    let mut spool = RawBodySpool::new(&directory, length, budget).unwrap();
    let block = [0xa5; 32 << 10];
    while spool.written < length {
        let count = (length - spool.written).min(block.len() as u64) as usize;
        spool.write_all(&block[..count]).unwrap();
    }
    assert_eq!(spool.written, length);
    assert_eq!(spool.file.metadata().unwrap().len(), length);
    assert_eq!(spool.file.metadata().unwrap().nlink(), 0);
    assert!(spool.write_all(b"x").is_err());
    drop(spool);
    assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
    std::fs::remove_dir(directory).unwrap();
}
