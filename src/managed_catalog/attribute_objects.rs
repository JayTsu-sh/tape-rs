//! Immutable namespace-owned attribute objects. Uploads are private until a
//! small inode/preparation transaction checks the completed object reference.
//!
//! Objects are retained for the namespace lifetime. In particular there is no
//! age-based GC racing readers or uploads. Discarded namespace cleanup owns M.

use super::*;

const CHUNKS_PER_TRANSACTION: usize = 4;
const PROGRESS_LEN: usize = 9;

#[derive(Clone, Copy)]
pub(super) struct VerifiedShape {
    pub symlink: bool,
}

/// Each chunk is fetched in a fresh short transaction. An immutable reference
/// survives inode updates; no old read version is held while transferring it.
struct Reader<'a> {
    catalog: &'a ManagedCatalog,
    namespace: Uuid,
    manifest: AttributeManifest,
    next: u16,
    chunk: Vec<u8>,
    offset: usize,
    consumed: u64,
    hash: sha2::Sha256,
}

impl<'a> Reader<'a> {
    async fn new(
        catalog: &'a ManagedCatalog,
        namespace: Uuid,
        manifest: AttributeManifest,
    ) -> Result<Self> {
        use sha2::Digest;
        let tr = catalog.db.create_trx()?;
        require_complete(&tr, namespace, manifest).await?;
        let mut hash = sha2::Sha256::new();
        hash.update(b"tape-rs/node-attributes/1");
        Ok(Self {
            catalog,
            namespace,
            manifest,
            next: 0,
            chunk: Vec::new(),
            offset: 0,
            consumed: 0,
            hash,
        })
    }

    async fn available(&mut self) -> Result<()> {
        use sha2::Digest;
        if self.offset < self.chunk.len() {
            return Ok(());
        }
        if self.next == self.manifest.chunks {
            return Err(CatalogError::Corrupt("LTFS 属性对象截断"));
        }
        let first = self.next;
        let count =
            (usize::from(self.manifest.chunks) - usize::from(first)).min(CHUNKS_PER_TRANSACTION);
        let mut prefix = prefix(self.namespace, self.manifest);
        prefix.push(1);
        let after = (first > 0).then(|| chunk_key(self.namespace, self.manifest, first - 1));
        let page = self
            .catalog
            .db
            .run(|tr, _| {
                let prefix = prefix.clone();
                let after = after.clone();
                async move { read_page(&tr, &prefix, after.as_deref(), count, true).await }
            })
            .await?;
        if page.items.len() != count
            || (usize::from(first) + count == usize::from(self.manifest.chunks)
                && page.next.is_some())
        {
            return Err(CatalogError::Corrupt("LTFS 属性对象分块数量"));
        }
        self.chunk.clear();
        for (offset, (key, bytes)) in page.items.into_iter().enumerate() {
            let index = first + offset as u16;
            let expected =
                (self.manifest.len as usize - usize::from(index) * CHUNK_SIZE).min(CHUNK_SIZE);
            if key != chunk_key(self.namespace, self.manifest, index) || bytes.len() != expected {
                return Err(CatalogError::Corrupt("LTFS 属性对象分块顺序或长度"));
            }
            self.chunk.extend_from_slice(&bytes);
        }
        self.hash.update(&self.chunk);
        self.next += count as u16;
        self.offset = 0;
        Ok(())
    }

    async fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0; N];
        let mut at = 0;
        while at < N {
            self.available().await?;
            let count = (N - at).min(self.chunk.len() - self.offset);
            out[at..at + count].copy_from_slice(&self.chunk[self.offset..self.offset + count]);
            at += count;
            self.offset += count;
            self.consumed += count as u64;
        }
        Ok(out)
    }

    async fn text(&mut self, mut remaining: usize) -> Result<()> {
        // Carry at most the incomplete final UTF-8 code point between chunks.
        let mut pending = Vec::with_capacity(CHUNKS_PER_TRANSACTION * CHUNK_SIZE + 3);
        while remaining > 0 {
            self.available().await?;
            let count = remaining.min(self.chunk.len() - self.offset);
            pending.extend_from_slice(&self.chunk[self.offset..self.offset + count]);
            self.offset += count;
            self.consumed += count as u64;
            remaining -= count;
            match std::str::from_utf8(&pending) {
                Ok(_) => pending.clear(),
                Err(error) if error.error_len().is_none() => {
                    pending.drain(..error.valid_up_to());
                }
                Err(_) => return Err(CatalogError::Corrupt("LTFS 属性对象文本编码")),
            }
        }
        if !pending.is_empty() {
            return Err(CatalogError::Corrupt("LTFS 属性对象文本截断"));
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        use sha2::Digest;
        if self.consumed != u64::from(self.manifest.len)
            || self.next != self.manifest.chunks
            || self.offset != self.chunk.len()
            || <[u8; 32]>::from(self.hash.finalize()) != self.manifest.digest
        {
            return Err(CatalogError::Corrupt("LTFS 属性对象长度或摘要"));
        }
        Ok(())
    }
}

impl ManagedCatalog {
    /// Validate the entire encoded object, including text, flags and EOF, with
    /// memory bounded by a fixed number of chunks. No giant field needs to be materialized.
    pub(super) async fn verify_attribute_object(
        &self,
        namespace: Uuid,
        manifest: AttributeManifest,
    ) -> Result<VerifiedShape> {
        let mut reader = Reader::new(self, namespace, manifest).await?;
        if reader.fixed::<1>().await? != [1]
            || reader.fixed::<1>().await?[0] > 1
            || u64::from_be_bytes(reader.fixed().await?) == 0
        {
            return Err(CatalogError::Corrupt("LTFS 属性对象头"));
        }
        for _ in 0..5 {
            let len = u16::from_be_bytes(reader.fixed().await?) as usize;
            reader.text(len).await?;
        }
        let count = u16::from_be_bytes(reader.fixed().await?);
        for _ in 0..count {
            let len = u16::from_be_bytes(reader.fixed().await?) as usize;
            reader.text(len).await?;
            let len = u32::from_be_bytes(reader.fixed().await?) as usize;
            reader.text(len).await?;
            if reader.fixed::<1>().await?[0] > 1 {
                return Err(CatalogError::Corrupt("LTFS 属性对象 xattr 标志"));
            }
        }
        let symlink = match reader.fixed::<1>().await? {
            [0] => false,
            [1] => {
                let len = u32::from_be_bytes(reader.fixed().await?) as usize;
                reader.text(len).await?;
                true
            }
            _ => return Err(CatalogError::Corrupt("LTFS 属性对象符号链接标志")),
        };
        reader.finish()?;
        Ok(VerifiedShape { symlink })
    }

    pub(super) async fn attribute_bytes_to<W: std::io::Write>(
        &self,
        namespace: Uuid,
        attributes: &NodeAttributes,
        output: &mut W,
    ) -> Result<()> {
        if let Some(bytes) = attributes.bytes() {
            return output
                .write_all(bytes)
                .map_err(|error| CatalogError::Cache(error.to_string()));
        }
        let manifest = attributes
            .manifest()
            .ok_or(CatalogError::Corrupt("LTFS 属性对象引用缺失"))?;
        let mut reader = Reader::new(self, namespace, manifest).await?;
        while reader.next < manifest.chunks {
            reader.available().await?;
            output
                .write_all(&reader.chunk)
                .map_err(|error| CatalogError::Cache(error.to_string()))?;
            reader.consumed += reader.chunk.len() as u64;
            reader.offset = reader.chunk.len();
        }
        reader.finish()
    }

    pub(super) async fn hydrate_attributes(
        &self,
        namespace: Uuid,
        attributes: NodeAttributes,
    ) -> Result<NodeAttributes> {
        if !attributes.is_external() {
            return Ok(attributes);
        }
        let mut bytes = Vec::new();
        self.attribute_bytes_to(namespace, &attributes, &mut bytes)
            .await?;
        NodeAttributes::decode_owned(bytes)
    }

    /// Stream the exact encoded attributes selected by one inode read. Output
    /// is valid only on success; callers should use a private temporary file
    /// and publish it after length/hash verification, never expose partial data.
    pub async fn node_attributes_to<W: std::io::Write>(
        &self,
        namespace: Uuid,
        id: Uuid,
        output: &mut W,
    ) -> Result<bool> {
        let tr = self.db.create_trx()?;
        require_namespace_readable(&tr, namespace).await?;
        let attributes = if let Some((locator, receipt, _)) =
            lookup::published_file_receipt_at(&tr, namespace, id).await?
        {
            active_batch::prepared_attributes_at(&tr, locator.batch_id, &receipt).await?
        } else {
            let Some(raw) = tr.get(&node_key(namespace, id), true).await? else {
                return Ok(false);
            };
            decode_node(&raw)?.attributes
        };
        drop(tr);
        let Some(attributes) = attributes else {
            return Ok(false);
        };
        self.attribute_bytes_to(namespace, &attributes, output)
            .await?;
        Ok(true)
    }
}

fn prefix(namespace: Uuid, manifest: AttributeManifest) -> Vec<u8> {
    let mut out = key(b'M');
    out.extend_from_slice(namespace.as_bytes());
    out.extend_from_slice(&manifest.digest);
    out
}

fn progress_key(namespace: Uuid, manifest: AttributeManifest) -> Vec<u8> {
    let mut out = prefix(namespace, manifest);
    out.push(0);
    out
}

pub(super) fn chunk_key(namespace: Uuid, manifest: AttributeManifest, chunk: u16) -> Vec<u8> {
    let mut out = prefix(namespace, manifest);
    out.push(1);
    out.extend_from_slice(&chunk.to_be_bytes());
    out
}

fn progress(manifest: AttributeManifest, next: u16) -> [u8; PROGRESS_LEN] {
    let mut out = [0; PROGRESS_LEN];
    out[0] = 1;
    out[1..5].copy_from_slice(&manifest.len.to_be_bytes());
    out[5..7].copy_from_slice(&manifest.chunks.to_be_bytes());
    out[7..9].copy_from_slice(&next.to_be_bytes());
    out
}

fn decode_progress(value: &[u8], manifest: AttributeManifest) -> Result<u16> {
    if value.len() != PROGRESS_LEN || value[..7] != progress(manifest, 0)[..7] {
        return Err(CatalogError::Corrupt("LTFS 属性对象上传身份"));
    }
    let next = u16::from_be_bytes(value[7..9].try_into().expect("length checked"));
    if next > manifest.chunks {
        return Err(CatalogError::Corrupt("LTFS 属性对象上传进度"));
    }
    Ok(next)
}

pub(super) async fn require_complete(
    tr: &Transaction,
    namespace: Uuid,
    manifest: AttributeManifest,
) -> Result<()> {
    let raw = tr
        .get(&progress_key(namespace, manifest), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if decode_progress(&raw, manifest)? != manifest.chunks {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

impl ManagedCatalog {
    /// Stage an immutable object in transactions containing at most four
    /// 64,000-byte chunks, plus keys and a tiny progress record. A crash leaves
    /// a resumable prefix, never a reader-visible partial inode. The final
    /// publication still checks its own namespace/source/version/owner fences.
    pub(super) async fn upload_attributes(
        &self,
        namespace: Uuid,
        attributes: &NodeAttributes,
    ) -> Result<()> {
        let Some(manifest) = attributes.manifest() else {
            return Ok(());
        };
        let bytes = attributes
            .bytes()
            .ok_or(CatalogError::Invalid("不能上传未读取的 LTFS 属性"))?;
        let marker = progress_key(namespace, manifest);
        let mut next = 0;
        while next < manifest.chunks {
            let expected = next;
            // Borrow the immutable bytes across retries, never clone the
            // complete payload or collect every chunk in an extra Vec.
            next = self
                .db
                .run(|tr, _| {
                    let marker = marker.clone();
                    async move {
                        tr.set_option(foundationdb::options::TransactionOption::SizeLimit(
                            512_000,
                        ))?;
                        requests::retry_result(
                            async {
                                let saved = tr.get(&marker, false).await?;
                                let first = saved
                                    .as_deref()
                                    .map(|value| decode_progress(value, manifest))
                                    .transpose()?
                                    .unwrap_or(0);
                                if first == manifest.chunks {
                                    return Ok(first);
                                }
                                let state = tr.get(&namespace_key(namespace), false).await?;
                                if !matches!(
                                    state.as_deref().map(decode_namespace_state).transpose()?,
                                    Some(
                                        NamespaceState::Building
                                            | NamespaceState::Ready
                                            | NamespaceState::Active
                                    )
                                ) {
                                    return Err(CatalogError::Incomplete);
                                }
                                if first > expected {
                                    return Ok(first);
                                }
                                if first < expected {
                                    return Err(CatalogError::Corrupt("LTFS 属性对象上传进度倒退"));
                                }
                                let end = (usize::from(first) + CHUNKS_PER_TRANSACTION)
                                    .min(usize::from(manifest.chunks))
                                    as u16;
                                for index in first..end {
                                    let start = usize::from(index) * CHUNK_SIZE;
                                    let chunk =
                                        &bytes[start..(start + CHUNK_SIZE).min(bytes.len())];
                                    let key = chunk_key(namespace, manifest, index);
                                    if let Some(saved) = tr.get(&key, false).await? {
                                        if saved.as_ref() != chunk {
                                            return Err(CatalogError::Corrupt(
                                                "LTFS 属性对象分块与重试冲突",
                                            ));
                                        }
                                    } else {
                                        tr.set(&key, chunk);
                                    }
                                }
                                tr.set(&marker, &progress(manifest, end));
                                #[cfg(test)]
                                assert!(tr.get_approximate_size().await? < 512_000);
                                Ok(end)
                            }
                            .await,
                        )
                    }
                })
                .await??;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER；隔离随机命名空间大属性上传/恢复测试，不访问设备"]
    async fn large_attributes_resume_and_publish_atomically() {
        use crate::ltfs::index::{NodeMeta, Xattr};
        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        catalog.begin_import(ns).await.unwrap();
        let attributes = NodeAttributes::new(
            NodeMeta {
                file_uid: 1,
                ..Default::default()
            },
            vec![Xattr {
                key: "large".into(),
                value: "中🙂".repeat(1_800_000),
                base64: false,
            }],
            None,
        )
        .unwrap();
        let manifest = attributes.manifest().unwrap();
        let raw_root = catalog
            .db
            .create_trx()
            .unwrap()
            .get(&node_key(ns, Uuid::nil()), true)
            .await
            .unwrap()
            .unwrap();
        // Publication refuses absent and partially uploaded objects.
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            write_attribute_chunks(&tr, ns, Uuid::nil(), false, Some(&attributes))
                .await
                .is_err()
        );
        drop(tr);
        // A process interrupted after its first committed chunk leaves this exact durable prefix.
        let tr = catalog.db.create_trx().unwrap();
        tr.set(
            &chunk_key(ns, manifest, 0),
            &attributes.bytes().unwrap()[..CHUNK_SIZE],
        );
        tr.set(&progress_key(ns, manifest), &progress(manifest, 1));
        tr.commit().await.unwrap();
        let tr = catalog.db.create_trx().unwrap();
        assert!(require_complete(&tr, ns, manifest).await.is_err());
        assert_eq!(
            tr.get(&node_key(ns, Uuid::nil()), true)
                .await
                .unwrap()
                .unwrap()
                .as_ref(),
            raw_root.as_ref()
        );
        drop(tr);
        // Reopening the client resumes the committed prefix. The production uploader
        // enforces a 512KB transaction limit and the test checks approximate size.
        let resumed = ManagedCatalog::open(Some(&cluster)).unwrap();
        assert!(
            resumed
                .set_import_root_attributes(ns, &attributes)
                .await
                .unwrap()
        );
        assert!(
            !resumed
                .set_import_root_attributes(ns, &attributes)
                .await
                .unwrap()
        );
        let tr = catalog.db.create_trx().unwrap();
        let raw = tr
            .get(&node_key(ns, Uuid::nil()), true)
            .await
            .unwrap()
            .unwrap();
        let node = hydrate_node(&tr, ns, Uuid::nil(), decode_node(&raw).unwrap(), true)
            .await
            .unwrap();
        assert_eq!(node.attributes, Some(attributes.clone()));
        drop(tr);
        assert!(
            !catalog
                .verify_attribute_object(ns, manifest)
                .await
                .unwrap()
                .symlink
        );
        struct SlowHash {
            hash: sha2::Sha256,
            first: bool,
            bytes: usize,
        }
        impl std::io::Write for SlowHash {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                use sha2::Digest;
                if self.first {
                    self.first = false;
                    std::thread::sleep(std::time::Duration::from_secs(6));
                }
                self.hash.update(bytes);
                self.bytes += bytes.len();
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        use sha2::Digest;
        let mut slow = SlowHash {
            hash: sha2::Sha256::new(),
            first: true,
            bytes: 0,
        };
        slow.hash.update(b"tape-rs/node-attributes/1");
        catalog
            .attribute_bytes_to(
                ns,
                &NodeAttributes::from_manifest(manifest).unwrap(),
                &mut slow,
            )
            .await
            .unwrap();
        assert_eq!(slow.bytes, manifest.len as usize);
        assert_eq!(<[u8; 32]>::from(slow.hash.finalize()), manifest.digest);

        // Missing/corrupt chunks cannot pass full hydration, even after completion.
        let first = chunk_key(ns, manifest, 0);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&first, b"damaged");
        tr.commit().await.unwrap();
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            hydrate_node(&tr, ns, Uuid::nil(), decode_node(&raw).unwrap(), true)
                .await
                .is_err()
        );
        drop(tr);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&first, &attributes.bytes().unwrap()[..CHUNK_SIZE]);
        tr.commit().await.unwrap();
        // Discard is fenced by namespace state; uploads cannot race cleanup and resurrect objects.
        assert!(catalog.discard_import(ns).await.unwrap());
        while !catalog
            .purge_discarded_namespace(ns, 100)
            .await
            .unwrap()
            .complete
        {}
        assert!(catalog.upload_attributes(ns, &attributes).await.is_err());
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            tr.get(&progress_key(ns, manifest), true)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "需要 FDB_TEST_CLUSTER 和 FDB_ATTRIBUTE_XML/SHA256；真实归档 XML 回放，不访问设备"]
    async fn archived_large_root_roundtrip() {
        use crate::catalog::volume_extents::ExpectedIndex;
        use crate::ltfs::index::LtfsIndex;
        use sha2::{Digest, Sha256};
        let cluster = std::env::var("FDB_TEST_CLUSTER").expect("FDB_TEST_CLUSTER");
        let xml = std::path::PathBuf::from(
            std::env::var("FDB_ATTRIBUTE_XML").expect("FDB_ATTRIBUTE_XML"),
        );
        let expected_sha = std::env::var("FDB_ATTRIBUTE_SHA256").expect("FDB_ATTRIBUTE_SHA256");
        let mut file = std::fs::File::open(&xml).unwrap();
        let mut digest = Sha256::new();
        std::io::copy(&mut file, &mut digest).unwrap();
        let sha: [u8; 32] = digest.finalize().into();
        assert_eq!(
            sha.iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>(),
            expected_sha
        );
        let index = LtfsIndex::scan_full_reader(
            std::io::BufReader::new(std::fs::File::open(&xml).unwrap()),
            |_| Ok(()),
        )
        .unwrap();
        let expected = ExpectedIndex {
            volume_uuid: index.volume_uuid,
            generation: index.generation,
            location: index.self_location,
            sha256: sha,
        };
        drop(index);
        let root_dir = std::env::temp_dir().join(format!("tape-rs-large-root-{}", Uuid::new_v4()));
        std::fs::create_dir(&root_dir).unwrap();
        let path = root_dir.join("import.db");
        let stats =
            ImportSpool::build(std::fs::File::open(&xml).unwrap(), &path, expected).unwrap();
        let spool = ImportSpool::open(&path, expected).unwrap();
        let root = spool.node_by_path("").unwrap().unwrap();
        assert!(stats.max_attribute_bytes > 10_000_000);
        let count = root.attributes.xattrs().unwrap().len();
        let attributes = root.attributes;
        let manifest = attributes.manifest().unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        catalog.begin_import(namespace).await.unwrap();
        let begin = std::time::Instant::now();
        catalog
            .set_import_root_attributes(namespace, &attributes)
            .await
            .unwrap();
        let upload_seconds = begin.elapsed().as_secs_f64();
        let begin = std::time::Instant::now();
        assert!(
            !catalog
                .verify_attribute_object(namespace, manifest)
                .await
                .unwrap()
                .symlink
        );
        let verify_seconds = begin.elapsed().as_secs_f64();
        let mut expected_namespace = NamespaceDigest::new(namespace).unwrap();
        expected_namespace
            .push(
                Uuid::nil(),
                &Node {
                    kind: NodeKind::Directory,
                    parent: Uuid::nil(),
                    name: Vec::new(),
                    attributes: Some(attributes.clone()),
                },
                None,
            )
            .unwrap();
        assert_eq!(
            catalog
                .freeze_namespace(namespace, expected_namespace.summary())
                .await
                .unwrap(),
            NamespaceVerificationOutcome::Frozen
        );
        loop {
            match catalog.verify_namespace_page(namespace, 100).await.unwrap() {
                NamespaceVerificationOutcome::Verified(actual) => {
                    assert_eq!(actual, expected_namespace.summary());
                    break;
                }
                NamespaceVerificationOutcome::Progress { .. } => {}
                outcome => panic!("unexpected {outcome:?}"),
            }
        }
        // Explicit large read and streaming read both preserve the original bytes.
        assert!(catalog.node(namespace, Uuid::nil()).await.is_err());
        let reference = NodeAttributes::from_manifest(manifest).unwrap();
        assert_eq!(
            catalog
                .hydrate_attributes(namespace, reference.clone())
                .await
                .unwrap(),
            attributes
        );
        let mut out = std::fs::File::create(root_dir.join("attributes.bin")).unwrap();
        catalog
            .attribute_bytes_to(namespace, &reference, &mut out)
            .await
            .unwrap();
        drop(out);
        let mut actual_sha = Sha256::new();
        actual_sha.update(b"tape-rs/node-attributes/1");
        std::io::copy(
            &mut std::fs::File::open(root_dir.join("attributes.bin")).unwrap(),
            &mut actual_sha,
        )
        .unwrap();
        assert_eq!(<[u8; 32]>::from(actual_sha.finalize()), attributes.digest());
        println!(
            "{}",
            serde_json::json!({"scope":"archived real-media XML, private FDB; no devices",
            "source_sha256":expected_sha,"volume_uuid":expected.volume_uuid.to_string(),
            "files":stats.files,"directories":stats.directories,"root_xattrs":count,
            "encoded_bytes":manifest.len,"chunks":manifest.chunks,"upload_seconds":upload_seconds,
            "stream_verify_seconds":verify_seconds,"namespace":namespace.to_string()})
        );
        catalog.discard_import(namespace).await.unwrap();
        while !catalog
            .purge_discarded_namespace(namespace, 100)
            .await
            .unwrap()
            .complete
        {}
        drop(spool);
        std::fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn progress_is_exact_and_never_wraps() {
        let manifest = AttributeManifest {
            digest: [1; 32],
            len: 12_000_001,
            chunks: 188,
        };
        for next in [0, 1, 4, 188] {
            assert_eq!(
                decode_progress(&progress(manifest, next), manifest).unwrap(),
                next
            );
        }
        assert!(decode_progress(&progress(manifest, 189), manifest).is_err());
        for len in 0..PROGRESS_LEN {
            assert!(decode_progress(&progress(manifest, 0)[..len], manifest).is_err());
        }
        assert!(
            decode_progress(
                &progress(manifest, 0),
                AttributeManifest {
                    len: 12_000_002,
                    ..manifest
                }
            )
            .is_err()
        );
        assert_ne!(
            chunk_key(Uuid::new_v4(), manifest, 0),
            chunk_key(Uuid::new_v4(), manifest, 0)
        );
    }
}
