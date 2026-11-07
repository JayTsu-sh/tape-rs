//! Explicit lab-build checkpoint after successful S4 and before FDB observation.
//! Local files coordinate the experiment; they never authorize media recovery.
use super::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, Instant};

const ENVIRONMENT: &str = "TAPE_RS_TEST_S4_GATE_DIR";
const MAX_FILE: u64 = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u8,
    nonce: String,
    namespace: String,
    library_serial: String,
    volume: String,
    previous_generation: u64,
    request_epoch: u64,
    operations: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Release {
    version: u8,
    nonce: String,
    armed_sha256: String,
    hit_sha256: String,
}

fn invalid(detail: impl std::fmt::Display) -> TapeError {
    TapeError::Catalog(format!(
        "实验 S4 检查点失败；保留原批次，不能重写正文: {detail}"
    ))
}

fn canonical_uuid(text: &str) -> bool {
    Uuid::parse_str(text).is_ok_and(|id| !id.is_nil() && id.to_string() == text)
}

impl Plan {
    fn matches(
        &self,
        previous: ActiveIndexIdentity,
        uploads: &[ActiveMediaUpload],
    ) -> Result<bool> {
        if self.version != 1
            || !canonical_uuid(&self.nonce)
            || !canonical_uuid(&self.namespace)
            || !canonical_uuid(&self.volume)
            || self.library_serial.is_empty()
            || self.previous_generation == 0
            || self.request_epoch == 0
            || self.operations.is_empty()
            || self.operations.len() > 128
            || self.operations.iter().any(|id| !canonical_uuid(id))
            || self.operations.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(invalid("原计划格式或原请求集合无效"));
        }
        let mut actual: Vec<_> = uploads
            .iter()
            .map(|upload| upload.intent.operation_id.to_string())
            .collect();
        actual.sort();
        if !actual
            .iter()
            .any(|id| self.operations.binary_search(id).is_ok())
        {
            return Ok(false);
        }
        if actual != self.operations
            || previous.volume_uuid.to_string() != self.volume
            || previous.generation != self.previous_generation
            || uploads.iter().any(|upload| {
                upload.intent.namespace.to_string() != self.namespace
                    || upload.intent.library_serial != self.library_serial
                    || upload.intent.request_epoch != self.request_epoch
                    || upload.intent.volume_id != previous.volume_uuid
                    || Some(upload.intent.volume_generation) != previous.generation.checked_add(1)
            })
        {
            return Err(invalid(
                "原请求被拆批或其卷、代次和作用域改变；尚未追加正文",
            ));
        }
        Ok(true)
    }
}

struct Directory {
    file: File,
}

impl Directory {
    fn open(path: &std::path::Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(invalid("测试目录必须是绝对路径"));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_DIRECTORY | nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(path)?;
        Self::private(&file, true)?;
        Ok(Self { file })
    }

    fn private(file: &File, directory: bool) -> Result<()> {
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no arguments or memory access across this boundary.
        let uid = unsafe { nix::libc::geteuid() };
        if metadata.uid() != uid
            || metadata.mode() & 0o077 != 0
            || metadata.is_dir() != directory
            || !directory && !metadata.is_file()
        {
            return Err(invalid("测试文件须由当前用户独占且不能是特殊设备"));
        }
        Ok(())
    }

    fn path(&self, name: &str) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.file.as_raw_fd())).join(name)
    }

    fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
            .open(self.path(name))
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Self::private(&file, false)?;
        let mut raw = Vec::new();
        file.take(MAX_FILE + 1).read_to_end(&mut raw)?;
        if raw.len() as u64 > MAX_FILE {
            return Err(invalid("测试文件超过字节预算"));
        }
        Ok(Some(raw))
    }

    fn save(&self, name: &str, value: &Value) -> Result<String> {
        let raw = serde_json::to_vec(value).map_err(invalid)?;
        if raw.len() as u64 > MAX_FILE {
            return Err(invalid("测试回执超过字节预算"));
        }
        let temporary = self.path(&format!(".gate-{}", Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC)
            .open(&temporary)?;
        file.write_all(&raw)?;
        file.sync_all()?;
        // A reader sees the whole synced record, or no record. hard_link is
        // exclusive publication: an existing original intent is never replaced.
        fs::hard_link(&temporary, self.path(name))?;
        self.file.sync_all()?;
        fs::remove_file(temporary)?;
        self.file.sync_all()?;
        Ok(format!("{:x}", Sha256::digest(&raw)))
    }
}

pub(super) struct Gate {
    directory: Directory,
    plan: Plan,
    armed_sha256: String,
}

pub(super) fn arm_from_environment(
    previous: ActiveIndexIdentity,
    batch: Uuid,
    uploads: &[ActiveMediaUpload],
) -> Result<Option<Gate>> {
    let Some(path) = std::env::var_os(ENVIRONMENT) else {
        return Ok(None);
    };
    arm(std::path::Path::new(&path), previous, batch, uploads)
}

fn arm(
    path: &std::path::Path,
    previous: ActiveIndexIdentity,
    batch: Uuid,
    uploads: &[ActiveMediaUpload],
) -> Result<Option<Gate>> {
    let directory = Directory::open(path)?;
    let raw = directory
        .read("plan.json")?
        .ok_or_else(|| invalid("缺少原测试计划"))?;
    let plan: Plan = serde_json::from_slice(&raw).map_err(invalid)?;
    if !plan.matches(previous, uploads)? {
        return Ok(None);
    }
    // No tape I/O has occurred in this call. An existing marker stops a second
    // physical attempt rather than silently reusing or overwriting the test.
    if directory.read("hit.json")?.is_some() || directory.read("release.json")?.is_some() {
        return Err(invalid("测试已消费或出现提前释放，不得重放"));
    }
    let requests: Vec<_> = uploads
        .iter()
        .map(|upload| {
            let i = &upload.intent;
            json!({"operation":i.operation_id.to_string(),"epoch":i.request_epoch,
            "file":i.file_id.to_string(),"owner_epoch":i.owner_epoch,
            "bytes":i.expected_size,"digest":format!("{:?}",i.expected_digest)})
        })
        .collect();
    let armed_sha256 = directory.save(
        "armed.json",
        &json!({"version":1,"plan":plan,
        "batch":batch.to_string(),"requests":requests,"previous_index_sha256":previous.sha256}),
    )?;
    Ok(Some(Gate {
        directory,
        plan,
        armed_sha256,
    }))
}

impl Gate {
    pub(super) fn after_s4(self, current: ActiveIndexIdentity) -> Result<()> {
        self.wait(current, Duration::from_secs(120))
    }

    fn wait(self, current: ActiveIndexIdentity, budget: Duration) -> Result<()> {
        if current.volume_uuid.to_string() != self.plan.volume
            || Some(current.generation) != self.plan.previous_generation.checked_add(1)
        {
            return Err(invalid("S4 后的实际索引与原计划不符"));
        }
        let deadline = Instant::now() + budget;
        let hit_sha256 = self.directory.save(
            "hit.json",
            &json!({"version":1,
            "nonce":self.plan.nonce,"armed_sha256":self.armed_sha256,
            "generation":current.generation,"volume":current.volume_uuid.to_string(),
            "partition":current.location.partition,"block":current.location.start_block,
            "index_sha256":current.sha256,"incremental":current.incremental,
            "phase":"S4-complete-before-FDB-index-observation"}),
        )?;
        log::info!("实验 S4 检查点已命中，等待原测试释放；原批次结果尚未发布");
        loop {
            if Instant::now() >= deadline {
                return Err(invalid("等待测试释放超时；S4 已完成，结果待原批次恢复"));
            }
            if let Some(raw) = self.directory.read("release.json")? {
                let release: Release = serde_json::from_slice(&raw).map_err(invalid)?;
                if release.version != 1
                    || release.nonce != self.plan.nonce
                    || release.armed_sha256 != self.armed_sha256
                    || release.hit_sha256 != hit_sha256
                    || Instant::now() >= deadline
                {
                    return Err(invalid("原检查点释放身份不符或已超时"));
                }
                return Ok(());
            }
            std::thread::sleep(
                Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ltfs::index::IndexLocation;
    use crate::managed_catalog::ContentDigest;
    use std::os::unix::fs::DirBuilderExt;

    struct Fixture {
        path: PathBuf,
        plan: Plan,
        previous: ActiveIndexIdentity,
        uploads: Vec<ActiveMediaUpload>,
    }
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("tape-s4-gate-{}", Uuid::new_v4()));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            let volume = Uuid::new_v4();
            let namespace = Uuid::new_v4();
            let uploads: Vec<_> = (0..8)
                .map(|n| ActiveMediaUpload {
                    intent: ActiveFileIntent {
                        namespace,
                        parent: Uuid::new_v4(),
                        parent_path: b"/".to_vec(),
                        name: format!("gate-{n}").into_bytes(),
                        file_id: Uuid::new_v4(),
                        expected_version: None,
                        operation_id: Uuid::new_v4(),
                        request_epoch: 1,
                        library_serial: "S4-TEST-LIBRARY".into(),
                        pool_id: Some(Uuid::new_v4()),
                        barcode_constraint: Some("S4GATEL8".into()),
                        owner_epoch: 17,
                        volume_id: volume,
                        volume_generation: 8,
                        volume_scan_id: Uuid::new_v4(),
                        expected_size: 43,
                        expected_digest: ContentDigest::Sha256([n; 32]),
                    },
                    spool: path.join("unused-payload"),
                })
                .collect();
            let mut operations: Vec<_> = uploads
                .iter()
                .map(|u| u.intent.operation_id.to_string())
                .collect();
            operations.sort();
            let plan = Plan {
                version: 1,
                nonce: Uuid::new_v4().to_string(),
                namespace: namespace.to_string(),
                library_serial: "S4-TEST-LIBRARY".into(),
                volume: volume.to_string(),
                previous_generation: 7,
                request_epoch: 1,
                operations,
            };
            Directory::open(&path)
                .unwrap()
                .save("plan.json", &json!(plan))
                .unwrap();
            Self {
                path,
                plan,
                uploads,
                previous: ActiveIndexIdentity {
                    volume_uuid: volume,
                    generation: 7,
                    location: IndexLocation {
                        partition: 'b',
                        start_block: 5,
                    },
                    sha256: [3; 32],
                    incremental: false,
                },
            }
        }
        fn arm(&self) -> Result<Gate> {
            arm(&self.path, self.previous, Uuid::new_v4(), &self.uploads)?
                .ok_or_else(|| invalid("fixture unmatched"))
        }
        fn current(&self) -> ActiveIndexIdentity {
            ActiveIndexIdentity {
                generation: 8,
                sha256: [4; 32],
                ..self.previous
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.path).unwrap();
        }
    }

    #[test]
    fn exact_batch_identity_is_required_before_arming() {
        let fixture = Fixture::new();
        assert!(
            fixture
                .plan
                .matches(fixture.previous, &fixture.uploads)
                .unwrap()
        );
        assert!(
            fixture
                .plan
                .matches(fixture.previous, &fixture.uploads[..7])
                .is_err()
        );
        for changed in [
            ActiveIndexIdentity {
                generation: 6,
                ..fixture.previous
            },
            ActiveIndexIdentity {
                volume_uuid: Uuid::new_v4(),
                ..fixture.previous
            },
        ] {
            assert!(fixture.plan.matches(changed, &fixture.uploads).is_err());
        }
        let mut changed = fixture.plan.clone();
        changed.request_epoch += 1;
        assert!(changed.matches(fixture.previous, &fixture.uploads).is_err());
        changed = fixture.plan.clone();
        changed.operations = (0..8).map(|_| Uuid::new_v4().to_string()).collect();
        changed.operations.sort();
        assert!(!changed.matches(fixture.previous, &fixture.uploads).unwrap());
        changed.operations[1] = changed.operations[0].clone();
        assert!(changed.matches(fixture.previous, &fixture.uploads).is_err());
        assert!(!fixture.path.join("armed.json").exists());
    }

    #[test]
    fn armed_intent_is_exclusive_and_early_release_is_rejected() {
        let fixture = Fixture::new();
        let gate = fixture.arm().unwrap();
        let original = fs::read(fixture.path.join("armed.json")).unwrap();
        assert!(fixture.arm().is_err());
        assert_eq!(original, fs::read(fixture.path.join("armed.json")).unwrap());
        drop(gate);
        let early = Fixture::new();
        Directory::open(&early.path)
            .unwrap()
            .save("release.json", &json!({}))
            .unwrap();
        assert!(early.arm().is_err());
        assert!(!early.path.join("armed.json").exists());
    }

    #[test]
    fn timeout_preserves_s4_hit_and_original_intent_for_unknown_result() {
        let fixture = Fixture::new();
        let gate = fixture.arm().unwrap();
        let original = fs::read(fixture.path.join("armed.json")).unwrap();
        assert!(gate.wait(fixture.current(), Duration::ZERO).is_err());
        let hit: Value =
            serde_json::from_slice(&fs::read(fixture.path.join("hit.json")).unwrap()).unwrap();
        assert_eq!(hit["phase"], "S4-complete-before-FDB-index-observation");
        assert_eq!(hit["generation"], 8);
        assert_eq!(original, fs::read(fixture.path.join("armed.json")).unwrap());
        assert!(!fixture.path.join("release.json").exists());
        assert!(fixture.arm().is_err());
    }

    #[test]
    fn release_must_bind_both_original_armed_and_actual_s4_hit() {
        for correct in [false, true] {
            let fixture = Fixture::new();
            let gate = fixture.arm().unwrap();
            let path = fixture.path.clone();
            let nonce = fixture.plan.nonce.clone();
            let producer = std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let hit = loop {
                    if let Ok(raw) = fs::read(path.join("hit.json"))
                        && serde_json::from_slice::<Value>(&raw).is_ok()
                    {
                        break raw;
                    }
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(1));
                };
                let armed = fs::read(path.join("armed.json")).unwrap();
                Directory::open(&path).unwrap().save("release.json", &json!({"version":1,"nonce":nonce,
                    "armed_sha256":format!("{:x}", Sha256::digest(armed)),
                    "hit_sha256":if correct {format!("{:x}",Sha256::digest(hit))} else {"wrong-hit".into()}})).unwrap();
            });
            let result = gate.wait(fixture.current(), Duration::from_secs(5));
            producer.join().unwrap();
            assert_eq!(result.is_ok(), correct);
            assert!(fixture.path.join("hit.json").exists());
        }
    }

    #[test]
    fn symlink_and_oversized_plan_are_rejected_before_arming() {
        let fixture = Fixture::new();
        let original = fixture.path.join("plan.json");
        let alternate = fixture.path.join("private-plan.json");
        fs::rename(&original, &alternate).unwrap();
        std::os::unix::fs::symlink(&alternate, &original).unwrap();
        assert!(fixture.arm().is_err());
        fs::remove_file(&original).unwrap();
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&original)
            .unwrap();
        file.write_all(&vec![b' '; MAX_FILE as usize + 1]).unwrap();
        assert!(fixture.arm().is_err());
        assert!(!fixture.path.join("armed.json").exists());
    }
}
