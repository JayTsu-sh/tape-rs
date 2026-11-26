//! Real SDK/Sim S4 and public publication phases; corruption is injected only
//! after the ordinary v2 state has been proven reachable under the real owner.
use super::*;
use crate::daemon::fdb_media::{ActiveMediaUpload, commit_active_media_batch};
use crate::ltfs::volume::LtfsVolume;
use crate::scsi::reservation::{self, ReservationKey};

struct Batch {
    fixture: Fixture,
    receipt: CommitReceipt,
    batch: Uuid,
    generation: u64,
}

fn batch_key_bytes(kind: &[u8], volume: Uuid, generation: u64) -> Vec<u8> {
    let mut key = crate::managed_catalog::key(b'p');
    key.extend_from_slice(kind);
    key.extend_from_slice(volume.as_bytes());
    key.extend_from_slice(&generation.to_be_bytes());
    key
}

async fn ready_for_materialization(c: &ManagedCatalog) -> Batch {
    let f = Box::pin(fixture(c)).await;
    let device = f._library.drive(0);
    let key = ReservationKey::new(1, 100);
    reservation::fence(&device, key).unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.set_reservation_guard(Some(key));
    let bytes = b"real-sdk-body";
    let intent = c
        .prepare_active_upload(
            &f.serial,
            f.route,
            100,
            f.spec.barcode(),
            &ActiveUploadSpec {
                pool_id: f.spec.pool(),
                path: b"/public-gate".to_vec(),
                operation_id: Uuid::new_v4(),
                expected_version: None,
                size: bytes.len() as u64,
                digest: ContentDigest::Sha256(Sha256::digest(bytes).into()),
            },
        )
        .await
        .unwrap();
    let spool = f._root.join("public-gate-body");
    std::fs::write(&spool, bytes).unwrap();
    let batch = Uuid::new_v4();
    let committed = Box::pin(commit_active_media_batch(
        c,
        volume,
        batch,
        &[ActiveMediaUpload { intent, spool }],
        1 << 20,
    ))
    .await
    .unwrap();
    let file = committed.files()[0].clone();
    let r = &file.receipt;
    let generation = r.volume_generation;
    assert_eq!(
        c.prepare_observed_active_file(&f.serial, 100, batch, r, &file.attributes)
            .await
            .unwrap(),
        ActivePreparationOutcome::Prepared
    );
    assert_eq!(
        c.mark_observed_active_batch_page_prepared(
            &f.serial,
            100,
            r.volume_id,
            generation,
            batch,
            0
        )
        .await
        .unwrap(),
        ActivePreparationOutcome::Prepared
    );
    assert_eq!(
        c.seal_observed_active_preparation(&f.serial, 100, r.volume_id, generation, batch)
            .await
            .unwrap(),
        ActivePreparationOutcome::Prepared
    );
    assert_eq!(
        c.publish_observed_active_batch(&f.serial, 100, r.volume_id, generation, batch)
            .await
            .unwrap(),
        ActivePublicationOutcome::Published
    );
    assert_eq!(
        c.finalize_published_active_batch_page(&f.serial, 100, r.volume_id, generation, batch, 0)
            .await
            .unwrap(),
        ActiveFinalizationOutcome::Finalized
    );
    drop(committed.into_volume());
    Batch {
        fixture: f,
        receipt: file.receipt,
        batch,
        generation: generation,
    }
}

async fn ready_for_cleanup(c: &ManagedCatalog) -> Batch {
    let b = Box::pin(ready_for_materialization(c)).await;
    let f = &b.fixture;
    assert_eq!(
        c.materialize_published_active_file(&f.serial, 100, b.batch, 0, &b.receipt)
            .await
            .unwrap(),
        ActiveMaterializationOutcome::Materialized
    );
    assert_eq!(
        c.finish_materialized_active_batch_page(
            &f.serial,
            100,
            b.receipt.volume_id,
            b.generation,
            b.batch,
            0
        )
        .await
        .unwrap(),
        ActiveCompactionOutcome::Completed
    );
    assert_eq!(
        c.seal_materialized_active_batch(
            &f.serial,
            100,
            b.receipt.volume_id,
            b.generation,
            b.batch
        )
        .await
        .unwrap(),
        ActiveCompactionOutcome::Completed
    );
    b
}

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;

const APPROVED_PRIVATE_CLUSTER: &[u8] =
    b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.1:44247\n";

struct PrivateCluster {
    path: String,
    _sealed: std::fs::File,
}

impl PrivateCluster {
    fn seal(bytes: &[u8]) -> std::io::Result<Self> {
        if !private_cluster_bytes_valid(bytes) {
            return Err(std::io::Error::other(
                "只允许原任务完整固定private FDB cluster bytes",
            ));
        }
        let name = std::ffi::CString::new("reclaim-benchmark-private-cluster").unwrap();
        let fd = nix::sys::memfd::memfd_create(
            name.as_c_str(),
            nix::sys::memfd::MemFdCreateFlag::MFD_CLOEXEC
                | nix::sys::memfd::MemFdCreateFlag::MFD_ALLOW_SEALING,
        )
        .map_err(std::io::Error::other)?;
        let mut file = std::fs::File::from(fd);
        file.write_all(bytes)?;
        use nix::fcntl::{FcntlArg, SealFlag, fcntl};
        fcntl(
            file.as_raw_fd(),
            FcntlArg::F_ADD_SEALS(
                SealFlag::F_SEAL_WRITE
                    | SealFlag::F_SEAL_GROW
                    | SealFlag::F_SEAL_SHRINK
                    | SealFlag::F_SEAL_SEAL,
            ),
        )
        .map_err(std::io::Error::other)?;
        Ok(Self {
            path: format!("/proc/self/fd/{}", file.as_raw_fd()),
            _sealed: file,
        })
    }

    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
            .open(path)?;
        let before = file.metadata()?;
        if !before.is_file() || before.len() != APPROVED_PRIVATE_CLUSTER.len() as u64 {
            return Err(std::io::Error::other("private cluster文件类型/长度不符"));
        }
        let mut bytes = Vec::new();
        std::io::Read::take(&mut file, 4096).read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        if (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
        ) != (
            after.dev(),
            after.ino(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
        ) {
            return Err(std::io::Error::other("读取private cluster期间文件改变"));
        }
        Self::seal(&bytes)
    }
}

fn private_cluster_bytes_valid(raw: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(raw) else {
        return false;
    };
    let Some((description, coordinators)) = text
        .strip_suffix('\n')
        .and_then(|text| text.split_once('@'))
    else {
        return false;
    };
    if description.split(':').count() != 2 || coordinators.split(',').count() != 1 {
        return false;
    }
    coordinators
        .parse::<std::net::SocketAddr>()
        .is_ok_and(|address| address == std::net::SocketAddr::from(([127, 0, 0, 1], 44247)))
        && raw == APPROVED_PRIVATE_CLUSTER
}
fn private_cluster() -> PrivateCluster {
    let path = std::env::var("FDB_TEST_CLUSTER").unwrap();
    PrivateCluster::open(std::path::Path::new(&path)).unwrap()
}

async fn require_group_header(c: &ManagedCatalog, b: &Batch, compacted: bool) {
    let key = batch_key_bytes(
        if compacted { b"d/" } else { b"b/" },
        b.receipt.volume_id,
        b.generation,
    );
    let tr = c.db.create_trx().unwrap();
    let mut raw = tr.get(&key, false).await.unwrap().unwrap().to_vec();
    assert_eq!(raw[0], 2, "fixture came from real ordinary SDK S4");
    raw[0] = 3;
    tr.set(&key, &raw);
    tr.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "私有44247+真实SDK/Sim可达compacted状态；注入required3缺route，验证public Already和清理门禁"]
async fn required_compacted_public_paths_reject_missing_route_without_clearing_pages() {
    let cluster = private_cluster();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster.path)).unwrap();
    let b = Box::pin(ready_for_cleanup(&c)).await;
    let f = &b.fixture;
    // First establish that this is the genuine owner and fully reachable state.
    assert_eq!(
        c.publish_observed_active_batch(&f.serial, 100, b.receipt.volume_id, b.generation, b.batch)
            .await
            .unwrap(),
        ActivePublicationOutcome::AlreadyPublished
    );
    require_group_header(&c, &b, true).await;
    let mut page_key = batch_key_bytes(b"g/", b.receipt.volume_id, b.generation);
    page_key.extend_from_slice(&0u16.to_be_bytes());
    let tr = c.db.create_trx().unwrap();
    let before = tr.get(&page_key, false).await.unwrap().unwrap().to_vec();
    drop(tr);
    let publish = c
        .publish_observed_active_batch(&f.serial, 100, b.receipt.volume_id, b.generation, b.batch)
        .await;
    let finalize = c
        .finalize_published_active_batch_page(
            &f.serial,
            100,
            b.receipt.volume_id,
            b.generation,
            b.batch,
            0,
        )
        .await;
    let materialize = c
        .materialize_published_active_file(&f.serial, 100, b.batch, 0, &b.receipt)
        .await;
    let seal = c
        .seal_materialized_active_batch(&f.serial, 100, b.receipt.volume_id, b.generation, b.batch)
        .await;
    let cleanup = c
        .cleanup_materialized_active_batch_page(
            &f.serial,
            100,
            b.receipt.volume_id,
            b.generation,
            b.batch,
            0,
        )
        .await;
    let tr = c.db.create_trx().unwrap();
    let after = tr.get(&page_key, false).await.unwrap().map(|v| v.to_vec());
    eprintln!(
        "PUBLIC_REQUIRED_GATE={}",
        json!({"namespace":f.route.route.namespace.to_string(),"batch":b.batch.to_string(),"volume":b.receipt.volume_id.to_string(),"generation":b.generation,
        "publish":format!("{publish:?}"),"finalize":format!("{finalize:?}"),"materialize":format!("{materialize:?}"),"seal":format!("{seal:?}"),"cleanup":format!("{cleanup:?}"),"page_before":before.len(),"page_after":after.as_ref().map(Vec::len),"hardware":false,"injection":"ordinary-real-S4 Header2->required Header3, no route"})
    );
    assert!(
        publish.is_err()
            && finalize.is_err()
            && materialize.is_err()
            && seal.is_err()
            && cleanup.is_err(),
        "required proof missing public paths returned success"
    );
    assert_eq!(
        after.as_deref(),
        Some(before.as_slice()),
        "required proof missing cleanup removed real page"
    );
}

#[tokio::test]
#[ignore = "私有44247+真实SDK/Sim terminal后未物化；required3缺route经真实lookup拒绝，原inode/copy不写"]
async fn required_materialization_rejects_missing_route_before_inode_write() {
    let cluster = private_cluster();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster.path)).unwrap();
    let b = Box::pin(ready_for_materialization(&c)).await;
    let inode = crate::managed_catalog::node_key(b.receipt.namespace, b.receipt.file_id);
    let tr = c.db.create_trx().unwrap();
    let before = tr.get(&inode, false).await.unwrap().map(|v| v.to_vec());
    drop(tr);
    assert!(
        before.is_none(),
        "fixture must be terminal but not materialized"
    );
    require_group_header(&c, &b, false).await;
    let result = c
        .materialize_published_active_file(&b.fixture.serial, 100, b.batch, 0, &b.receipt)
        .await;
    let tr = c.db.create_trx().unwrap();
    let after = tr.get(&inode, false).await.unwrap().map(|v| v.to_vec());
    eprintln!(
        "PUBLIC_REQUIRED_MATERIALIZE={}",
        json!({"namespace":b.receipt.namespace.to_string(),"batch":b.batch.to_string(),"result":format!("{result:?}"),"inode_unchanged":before==after,"hardware":false})
    );
    assert!(
        result.is_err(),
        "required materialization accepted missing route"
    );
    assert_eq!(before, after);
}

#[tokio::test]
#[ignore = "私有44247+真实SDK/Sim逐file marker及compacted-page重试；缺required证明不能清逐file回执"]
async fn required_page_and_file_markers_reject_missing_route() {
    let cluster = private_cluster();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster.path)).unwrap();
    let b = Box::pin(ready_for_materialization(&c)).await;
    let f = &b.fixture;
    assert_eq!(
        c.materialize_published_active_file(&f.serial, 100, b.batch, 0, &b.receipt)
            .await
            .unwrap(),
        ActiveMaterializationOutcome::Materialized
    );
    let mut marker_key = crate::managed_catalog::key(b'p');
    marker_key.extend_from_slice(b"m/");
    marker_key.extend_from_slice(b.receipt.namespace.as_bytes());
    marker_key.extend_from_slice(&b.receipt.request_epoch.to_be_bytes());
    marker_key.extend_from_slice(b.receipt.operation_id.as_bytes());
    let tr = c.db.create_trx().unwrap();
    let before = tr.get(&marker_key, false).await.unwrap().unwrap().to_vec();
    drop(tr);
    require_group_header(&c, &b, false).await;
    let materialize = c
        .materialize_published_active_file(&f.serial, 100, b.batch, 0, &b.receipt)
        .await;
    let finish = c
        .finish_materialized_active_batch_page(
            &f.serial,
            100,
            b.receipt.volume_id,
            b.generation,
            b.batch,
            0,
        )
        .await;
    let tr = c.db.create_trx().unwrap();
    let after = tr
        .get(&marker_key, false)
        .await
        .unwrap()
        .map(|v| v.to_vec());
    eprintln!(
        "PUBLIC_REQUIRED_FILE_MARKER={}",
        json!({
            "namespace": b.receipt.namespace.to_string(), "materialize": format!("{materialize:?}"),
            "finish": format!("{finish:?}"), "marker_unchanged":after.as_deref()==Some(before.as_slice()),
            "hardware":false
        })
    );
    assert!(materialize.is_err() && finish.is_err());
    assert_eq!(after.as_deref(), Some(before.as_slice()));
}

#[tokio::test]
#[ignore = "私有44247+真实SDK prepare/append/complete，尚未stage且无terminal必须保持NotFinalized"]
async fn ordinary_unstaged_receipt_remains_not_finalized() {
    let cluster = private_cluster();
    let _network = unsafe { foundationdb::boot() };
    let c = ManagedCatalog::open(Some(&cluster.path)).unwrap();
    let f = Box::pin(fixture(&c)).await;
    let device = f._library.drive(0);
    let key = ReservationKey::new(1, 100);
    reservation::fence(&device, key).unwrap();
    let mut volume = LtfsVolume::mount(&device).unwrap();
    volume.set_reservation_guard(Some(key));
    let bytes = b"unstaged-body";
    let intent = c
        .prepare_active_upload(
            &f.serial,
            f.route,
            100,
            f.spec.barcode(),
            &ActiveUploadSpec {
                pool_id: f.spec.pool(),
                path: b"/unstaged".to_vec(),
                operation_id: Uuid::new_v4(),
                expected_version: None,
                size: bytes.len() as u64,
                digest: ContentDigest::Sha256(Sha256::digest(bytes).into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        c.reserve_active_file(&intent).await.unwrap(),
        PublishOutcome::Registered
    );
    let path = intent.ltfs_path().unwrap();
    volume
        .append_file(&path, &mut std::io::Cursor::new(bytes))
        .unwrap();
    let prepared = crate::managed_catalog::prepare_active_file(&volume, &path, &intent).unwrap();
    assert_eq!(
        c.complete_active_file(&prepared.receipt).await.unwrap(),
        PublishOutcome::Registered
    );
    let result = c
        .materialize_published_active_file(&f.serial, 100, Uuid::new_v4(), 0, &prepared.receipt)
        .await;
    eprintln!(
        "ORDINARY_UNSTAGED={}",
        json!({"namespace":intent.namespace.to_string(),"result":format!("{result:?}"),"hardware":false})
    );
    assert_eq!(result.unwrap(), ActiveMaterializationOutcome::NotFinalized);
}

#[test]
fn public_gate_cluster_pin_accepts_only_original_bytes() {
    assert!(private_cluster_bytes_valid(APPROVED_PRIVATE_CLUSTER));
    for bytes in [
        b"other:other@127.0.0.1:44247\n".as_slice(),
        b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.1:44247,10.131.7.207:4500\n"
            .as_slice(),
        b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.1:442470\n".as_slice(),
        b"reclaimperf:1abcb7e07fe680bf589bc53bc375c62b@127.0.0.1:44247\r\n".as_slice(),
    ] {
        assert!(!private_cluster_bytes_valid(bytes));
        assert!(PrivateCluster::seal(bytes).is_err());
    }
    let pin = PrivateCluster::seal(APPROVED_PRIVATE_CLUSTER).unwrap();
    assert_eq!(std::fs::read(&pin.path).unwrap(), APPROVED_PRIVATE_CLUSTER);
    use nix::fcntl::{FcntlArg, fcntl};
    assert_eq!(
        fcntl(pin._sealed.as_raw_fd(), FcntlArg::F_GET_SEALS).unwrap(),
        15
    );
}
