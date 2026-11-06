//! Durable proof of the complete original result, bound to the required header.
use super::*;
use sha2::{Digest, Sha256};
use upload_state::UploadJournal;

pub(super) const ACK: &str = "receipt.json";
pub(super) const LIMIT: usize = (64 << 20) + 1024;

fn request_digest(saved: &UploadJournal) -> String {
    format!(
        "{:x}",
        Sha256::digest(upload_intent::encode(
            &saved.original,
            saved.request.as_ref()
        ))
    )
}

fn digest(value: &Value) -> String {
    format!(
        "{:x}",
        Sha256::new()
            .chain_update(b"tape-rs/local-upload-ack/1")
            .chain_update(value.to_string())
            .finalize()
    )
}

pub(super) fn encode(saved: &UploadJournal, committed: &CommittedUpload) -> Result<Vec<u8>> {
    let request = saved
        .request
        .as_ref()
        .ok_or_else(|| invalid("未准备的请求不能保存提交回执"))?;
    receipts::verify(committed.details(), request)?;
    let mut value = serde_json::json!({"version":1,"request_digest":request_digest(saved),"receipt":committed.details()});
    value["record_digest"] = Value::String(digest(&value));
    let bytes = format!("{value}\n").into_bytes();
    if bytes.len() > LIMIT {
        return Err(invalid("本地原上传回执超过字节预算"));
    }
    Ok(bytes)
}

pub(super) fn decode(bytes: &[u8], saved: &UploadJournal) -> Result<CommittedUpload> {
    let bad = || invalid("本地原上传回执被截断、损坏或不属于该请求");
    if bytes.len() > LIMIT {
        return Err(bad());
    }
    let mut value: Value = serde_json::from_slice(bytes).map_err(|_| bad())?;
    if bytes.strip_suffix(b"\n").unwrap_or(bytes) != value.to_string().as_bytes()
        || !value.as_object().is_some_and(|v| {
            v.len() == 4
                && ["version", "request_digest", "receipt", "record_digest"]
                    .iter()
                    .all(|k| v.contains_key(*k))
        })
        || value["version"] != 1
        || value["request_digest"] != request_digest(saved)
    {
        return Err(bad());
    }
    let checksum = value
        .as_object_mut()
        .unwrap()
        .remove("record_digest")
        .unwrap();
    if checksum != digest(&value) {
        return Err(bad());
    }
    let request = saved.request.as_ref().ok_or_else(bad)?;
    CommittedUpload::parse(value["receipt"].take(), request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::PathBuf;

    // Inert wire fixtures exercise local state, not physical commit evidence.
    fn fixture() -> (PathBuf, UploadJournal, CommittedUpload) {
        let root = std::env::temp_dir().join(format!("fdb-local-ack-{}", Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        eprintln!("fdb_local_ack_workspace={}", root.display());
        let source = root.join("source");
        fs::write(&source, b"abc").unwrap();
        let request = super::super::tests::request();
        request
            .save_fixture(&root.join("request"), &source)
            .unwrap();
        let receipt = CommittedUpload::parse(receipts::tests::receipt(&request), &request).unwrap();
        let saved = UploadJournal::open_locked(&root.join("request"), true).unwrap();
        (root, saved, receipt)
    }

    #[test]
    fn acknowledgement_checksum_binds_every_byte_and_the_complete_original_header() {
        let (root, saved, receipt) = fixture();
        let bytes = encode(&saved, &receipt).unwrap();
        assert_eq!(decode(&bytes, &saved).unwrap(), receipt);
        for end in 0..bytes.len() - 1 {
            assert!(decode(&bytes[..end], &saved).is_err(), "truncated {end}");
        }
        for offset in 0..bytes.len() - 1 {
            let mut damaged = bytes.clone();
            damaged[offset] ^= 1;
            assert!(decode(&damaged, &saved).is_err(), "corrupt {offset}");
        }
        let (other_root, other, _) = fixture();
        assert!(decode(&bytes, &other).is_err());
        let mut value: Value = serde_json::from_slice(&bytes).unwrap();
        value["unexpected"] = serde_json::json!(true);
        value.as_object_mut().unwrap().remove("record_digest");
        value["record_digest"] = Value::String(digest(&value));
        assert!(decode(value.to_string().as_bytes(), &saved).is_err());
        drop(other);
        drop(saved);
        fs::remove_dir_all(other_root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn durable_ack_precedes_cleanup_and_read_queries_never_release_the_body() {
        let (root, mut saved, receipt) = fixture();
        let path = root.join("request");
        let header = fs::read(path.join("request.json")).unwrap();
        assert!(saved.release_payload().is_err());
        saved.record_acknowledged(&receipt).unwrap();
        assert_eq!(
            fs::metadata(path.join(ACK)).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read(path.join("payload")).unwrap(), b"abc");
        let ack = fs::read(path.join(ACK)).unwrap();
        saved.record_acknowledged(&receipt).unwrap();
        assert_eq!(fs::read(path.join(ACK)).unwrap(), ack);
        let mut changed = receipt.details().clone();
        changed["attributes"]["metadata"]["readonly"] = serde_json::json!(false);
        let changed = CommittedUpload::parse(changed, saved.request.as_ref().unwrap()).unwrap();
        assert!(saved.record_acknowledged(&changed).is_err());
        assert_eq!(fs::read(path.join(ACK)).unwrap(), ack);
        drop(saved);
        let client = FdbClient::new(["127.0.0.1:0"])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_millis(50));
        assert_eq!(
            client.status_saved(&path).unwrap(),
            UploadStatus::Committed(1)
        );
        assert_eq!(client.saved_receipt(&path).unwrap(), Some(receipt.clone()));
        assert_eq!(fs::read(path.join("payload")).unwrap(), b"abc");
        let mut shared = UploadJournal::open_locked(&path, false).unwrap();
        assert!(shared.record_acknowledged(&receipt).is_err());
        assert!(shared.release_payload().is_err());
        drop(shared);
        assert_eq!(client.upload_saved(&path).unwrap(), receipt);
        assert!(!path.join("payload").exists());
        assert_eq!(fs::read(path.join("request.json")).unwrap(), header);
        assert_eq!(fs::read(path.join(ACK)).unwrap(), ack);
        assert_eq!(fs::read(root.join("source")).unwrap(), b"abc");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn changed_header_foreign_payload_and_bad_ack_are_never_overwritten_or_removed() {
        let (root, mut saved, receipt) = fixture();
        let path = root.join("request");
        let header = fs::read(path.join("request.json")).unwrap();
        fs::write(path.join("request.json"), b"foreign header").unwrap();
        assert!(saved.record_acknowledged(&receipt).is_err());
        assert!(!path.join(ACK).exists());
        assert_eq!(fs::read(path.join("payload")).unwrap(), b"abc");
        fs::write(path.join("request.json"), header).unwrap();
        let foreign = root.join("foreign");
        fs::write(&foreign, b"foreign acknowledgement").unwrap();
        std::os::unix::fs::symlink(&foreign, path.join(ACK)).unwrap();
        assert!(saved.record_acknowledged(&receipt).is_err());
        assert!(saved.release_payload().is_err());
        assert_eq!(fs::read(&foreign).unwrap(), b"foreign acknowledgement");
        fs::remove_file(path.join(ACK)).unwrap();
        fs::write(path.join(ACK), b"damaged acknowledgement").unwrap();
        assert!(saved.record_acknowledged(&receipt).is_err());
        assert!(saved.release_payload().is_err());
        assert_eq!(
            fs::read(path.join(ACK)).unwrap(),
            b"damaged acknowledgement"
        );
        fs::remove_file(path.join(ACK)).unwrap();
        saved.record_acknowledged(&receipt).unwrap();
        fs::write(path.join("payload"), b"foreign body").unwrap();
        assert!(saved.release_payload().is_err());
        assert_eq!(fs::read(path.join("payload")).unwrap(), b"foreign body");
        drop(saved);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn acknowledgement_and_cleanup_follow_the_held_directory_not_a_replacement_path() {
        let (root, mut saved, receipt) = fixture();
        let path = root.join("request");
        let held = root.join("held");
        fs::rename(&path, &held).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("request.json"), b"foreign header").unwrap();
        fs::write(path.join("payload"), b"foreign body").unwrap();
        saved.record_acknowledged(&receipt).unwrap();
        saved.release_payload().unwrap();
        assert!(held.join(ACK).is_file());
        assert!(!held.join("payload").exists());
        assert!(!path.join(ACK).exists());
        assert_eq!(fs::read(path.join("payload")).unwrap(), b"foreign body");
        assert_eq!(
            fs::read(path.join("request.json")).unwrap(),
            b"foreign header"
        );
        drop(saved);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn special_oversized_and_damaged_completion_records_never_release_or_repair_data() {
        let (root, saved, _) = fixture();
        let path = root.join("request");
        let ack = path.join(ACK);
        nix::unistd::mkfifo(
            &ack,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .unwrap();
        assert!(saved.acknowledged().is_err());
        assert!(saved.release_payload().is_err());
        fs::remove_file(&ack).unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&ack)
            .unwrap();
        file.set_len(LIMIT as u64 + 1).unwrap();
        assert!(saved.acknowledged().is_err());
        assert!(saved.release_payload().is_err());
        drop(file);
        fs::write(&ack, b"damaged acknowledgement").unwrap();
        drop(saved);
        let client = FdbClient::new(["127.0.0.1:0"])
            .unwrap()
            .with_timeouts(Duration::ZERO, Duration::from_millis(50));
        assert!(
            matches!(client.upload_saved(&path), Err(ClientError::Io(error)) if error.kind()==std::io::ErrorKind::InvalidInput)
        );
        assert_eq!(fs::read(&ack).unwrap(), b"damaged acknowledgement");
        assert_eq!(fs::read(path.join("payload")).unwrap(), b"abc");
        fs::remove_dir_all(root).unwrap();
    }
}
