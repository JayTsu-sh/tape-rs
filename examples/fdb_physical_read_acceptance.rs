//! 隔离实带只读验收。源清单和参考文件不能充当三节点冻结/生产迁移证明。
//! 介质索引和参考文件必须从指定卷实际读取；此程序没有格式化或写带入口。

#[cfg(not(feature = "fdb"))]
fn main() {
    eprintln!("须使用 --features fdb 构建现场验收程序");
    std::process::exit(2);
}

#[cfg(feature = "fdb")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    acceptance::run()
}

#[cfg(feature = "fdb")]
mod acceptance {
    use clap::{Parser, Subcommand};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use std::fs::{self, File};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use tape_rs::catalog::volume_extents::{ExpectedIndex, VolumeExtentCache};
    use tape_rs::error::{Result as TapeResult, TapeError};
    use tape_rs::ltfs::mam::{ATTR_BARCODE, Mam};
    use tape_rs::ltfs::{
        recovery::{RecoveryBudget, TailKind, capture_full_recovery, recover},
        volume::{LtfsVolume, read_label_only},
    };
    use tape_rs::managed_catalog::{ImportSpool, NodeKind};
    use tape_rs::scsi::{
        device::{ScsiDevice, ScsiResult},
        inquiry::read_unit_serial,
        reservation::{ReservationKey, verify_holder},
        transport::TapeTransport,
    };
    use tape_rs::tape::commands::TapeDrive;
    use uuid::Uuid;

    type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

    #[derive(Parser)]
    #[command(about = "隔离实带只读验收；源清单不作为三节点生产迁移证据")]
    struct Args {
        #[command(subcommand)]
        command: Action,
    }

    #[derive(Subcommand)]
    enum Action {
        /// 仅复核已归档的本地清单与缓存；不访问磁带，不连接 FDB，不授权服务。
        VerifyLocal {
            #[arg(long)]
            source_manifest: PathBuf,
            #[arg(long)]
            directory: PathBuf,
        },
        /// 仅读取已装载卷的标签/MAM，不扫描索引；为后续扫描确定真实 UUID。
        Identity {
            #[arg(long)]
            drive: String,
            #[arg(long)]
            drive_serial: String,
            #[arg(long)]
            barcode: String,
            #[arg(long)]
            node: u8,
            #[arg(long)]
            round: u64,
        },
        /// 已装载测试卷的只读取证；要求调用者已持有指定 PR，不执行装卸。
        Scan {
            #[arg(long)]
            drive: String,
            #[arg(long)]
            drive_serial: String,
            #[arg(long)]
            barcode: String,
            #[arg(long)]
            volume_uuid: Uuid,
            #[arg(long)]
            node: u8,
            #[arg(long)]
            round: u64,
            #[arg(long)]
            output: PathBuf,
            /// 单个原文索引构造上限（MiB）；legacy-tree 模式仍保留整棵索引树。
            #[arg(long, default_value_t=64, value_parser=clap::value_parser!(u64).range(1..=8192))]
            index_mib: u64,
            /// 仅用于对照：保留旧的整树恢复路径；默认使用流式取证。
            #[arg(long)]
            legacy_tree: bool,
        },
    }

    /// Operator-held scan reservation, separate from a Raft execution permit.
    /// Reject all commands except reads and positioning, and recheck PR for
    /// every command. It cannot be used as an ActiveMediaReader execution guard.
    struct ReadOnlyScan {
        device: ScsiDevice,
        key: ReservationKey,
    }

    impl ReadOnlyScan {
        fn check(&self, cdb: &[u8]) -> TapeResult<()> {
            if !matches!(
                cdb.first(),
                Some(
                    0x00 | 0x01
                        | 0x03
                        | 0x08
                        | 0x11
                        | 0x12
                        | 0x1a
                        | 0x2b
                        | 0x34
                        | 0x44
                        | 0x5a
                        | 0x5e
                        | 0x8c
                        | 0x91
                        | 0x92
                )
            ) {
                return Err(TapeError::NotReady("只读取证拒绝非读取/定位命令".into()));
            }
            verify_holder(&self.device, self.key)
        }
    }

    impl TapeTransport for ReadOnlyScan {
        fn execute_no_data(&self, cdb: &[u8], timeout: u32) -> TapeResult<ScsiResult> {
            self.check(cdb)?;
            let result = self.device.execute_no_data(cdb, timeout)?;
            verify_holder(&self.device, self.key)?;
            Ok(result)
        }
        fn execute_read(&self, cdb: &[u8], buf: &mut [u8], timeout: u32) -> TapeResult<ScsiResult> {
            self.check(cdb)?;
            let result = self.device.execute_read(cdb, buf, timeout)?;
            verify_holder(&self.device, self.key)?;
            Ok(result)
        }
        fn execute_write(&self, _: &[u8], _: &[u8], _: u32) -> TapeResult<ScsiResult> {
            Err(TapeError::NotReady("只读取证禁止向设备发送数据".into()))
        }
        fn identity(&self) -> &str {
            self.device.identity()
        }
    }

    fn save(path: &Path, value: &Value) -> Result<()> {
        let mut file = File::create_new(path)?;
        serde_json::to_writer_pretty(&mut file, value)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        Ok(())
    }

    pub fn run() -> Result<()> {
        if std::env::var("TAPE_RS_FDB_ISOLATED_ACCEPTANCE").as_deref() != Ok("1") {
            return Err("须设置 TAPE_RS_FDB_ISOLATED_ACCEPTANCE=1，仅用于隔离验收".into());
        }
        let args = Args::parse();
        match args.command {
            Action::VerifyLocal {
                source_manifest,
                directory,
            } => {
                let value: Value = serde_json::from_slice(&fs::read(source_manifest)?)?;
                let digest = value["sha256"].as_str().ok_or("源清单缺少索引摘要")?;
                if digest.len() != 64 || !digest.is_ascii() {
                    return Err("索引摘要无效".into());
                }
                let mut sha256 = [0; 32];
                for (offset, byte) in sha256.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&digest[offset * 2..offset * 2 + 2], 16)?;
                }
                let partition = value["partition"].as_str().ok_or("缺少索引分区")?;
                if !matches!(partition, "a" | "b") {
                    return Err("索引分区无效".into());
                }
                let source = ExpectedIndex {
                    volume_uuid: value["volume_uuid"]
                        .as_str()
                        .ok_or("缺少卷 UUID")?
                        .parse()?,
                    generation: value["generation"].as_u64().ok_or("缺少代数")?,
                    location: tape_rs::ltfs::index::IndexLocation {
                        partition: partition.chars().next().unwrap(),
                        start_block: value["start_block"].as_u64().ok_or("缺少索引位置")?,
                    },
                    sha256,
                };
                let begin = std::time::Instant::now();
                let spool = ImportSpool::open(&directory.join("import.db"), source)?;
                let cache = VolumeExtentCache::open(&directory.join("extent.db"), source)?;
                if value["files"].as_u64() != Some(spool.stats().files)
                    || value["directories"].as_u64() != Some(spool.stats().directories)
                {
                    return Err("归档源清单与实际目录数量不符".into());
                }
                let open_seconds = begin.elapsed().as_secs_f64();
                let begin = std::time::Instant::now();
                spool.verify_extent_cache(&cache)?;
                println!(
                    "{}",
                    json!({"scope":"archived local projections only", "files":spool.stats().files,
                    "directories":spool.stats().directories,"open_verify_seconds":open_seconds,
                    "compare_seconds":begin.elapsed().as_secs_f64(),"source_sha256":digest})
                );
            }
            Action::Identity {
                drive,
                drive_serial,
                barcode,
                node,
                round,
            } => {
                let device = ScsiDevice::open(&drive)?;
                if read_unit_serial(&device).as_deref() != Some(&drive_serial) {
                    return Err("驱动器序列号不符".into());
                }
                if node == 0 || round == 0 || round > 0xffff_ffff_ffff {
                    return Err("扫描身份无效".into());
                }
                let device = ReadOnlyScan {
                    device,
                    key: ReservationKey::new(node, round),
                };
                let label = read_label_only(&device)?;
                let mam = Mam::new(&device);
                let actual = mam.read_attribute(ATTR_BARCODE)?.ok_or("缺少 MAM 条码")?;
                if std::str::from_utf8(&actual.value)?.trim_matches(['\0', ' ']) != barcode {
                    return Err("MAM 条码不符".into());
                }
                println!(
                    "{}",
                    json!({"barcode":barcode,"volume_uuid":label.volume_uuid.to_string(),"drive_serial":drive_serial})
                );
            }

            Action::Scan {
                drive,
                drive_serial,
                barcode,
                volume_uuid,
                node,
                round,
                output,
                index_mib,
                legacy_tree,
            } => {
                if node == 0 || round == 0 || round > 0xffff_ffff_ffff || barcode.is_empty() {
                    return Err("扫描身份无效".into());
                }
                let device = ScsiDevice::open(&drive)?;
                if read_unit_serial(&device).as_deref() != Some(&drive_serial) {
                    return Err("驱动器序列号不符".into());
                }
                let device = ReadOnlyScan {
                    device,
                    key: ReservationKey::new(node, round),
                };
                verify_holder(&device.device, device.key)?;
                let label = read_label_only(&device)?;
                if label.volume_uuid != volume_uuid {
                    return Err("实带卷 UUID 不符".into());
                }
                let mam = Mam::new(&device);
                let actual = mam.read_attribute(ATTR_BARCODE)?.ok_or("缺少 MAM 条码")?;
                let actual = std::str::from_utf8(&actual.value)?.trim_matches(['\0', ' ']);
                if actual != barcode {
                    return Err("MAM 条码与目标测试带不符".into());
                }
                let vcr_before = mam.read_vcr()?.ok_or("缺少 VCR")?;
                fs::create_dir(&output)?;
                // The legacy comparison path is opt-in. Neither path grants
                // write authority or activates a catalog route.
                let budget = RecoveryBudget {
                    max_index_bytes: usize::try_from(
                        index_mib.checked_mul(1 << 20).ok_or("索引预算溢出")?,
                    )?,
                    ..Default::default()
                };
                let tape = TapeDrive::new(&device);
                let (source, dp_eod, ip_generation, history_depth, history_truncated) =
                    if legacy_tree {
                        let report = recover(&device, &label, &budget)?;
                        if report.dp.tail != TailKind::Complete
                            || report.ip.tail != TailKind::Complete
                            || !report.chain_ok
                            || report.ip_ahead()
                        {
                            return Err(format!(
                                "测试卷尾部未通过只读核验：{:?}",
                                report.restricted_reason()
                            )
                            .into());
                        }
                        let candidate = report.dp.last_index.as_ref().ok_or("缺少 DP 索引")?;
                        if candidate.index.incremental {
                            return Err(
                                "此取证入口要求当前 DP Full 索引，禁止改写介质以满足夹具".into()
                            );
                        }
                        let source = ExpectedIndex {
                            volume_uuid,
                            generation: candidate.index.generation,
                            location: candidate.index.self_location,
                            sha256: candidate.construct_sha256,
                        };

                        ImportSpool::build_confirmed(
                            &tape,
                            &label,
                            candidate,
                            &budget,
                            &output.join("import.db"),
                        )?;
                        VolumeExtentCache::build_confirmed(
                            &tape,
                            &label,
                            candidate,
                            &budget,
                            &output.join("extent.db"),
                        )?;

                        (
                            source,
                            report.dp.eod,
                            report
                                .ip
                                .last_index
                                .as_ref()
                                .ok_or("IP 索引缺失")?
                                .index
                                .generation,
                            report.chain_depth,
                            report.chain_truncated_by_budget,
                        )
                    } else {
                        eprintln!("取证阶段：流式 DP Full、历史链及 DP/IP 核验");
                        let provisional = output.join("index.xml.provisional");
                        let mut xml = File::create_new(&provisional)?;
                        let proof = capture_full_recovery(&tape, &label, &budget, &mut xml)?;
                        xml.sync_all()?;
                        verify_holder(&device.device, device.key)?;
                        drop(xml);
                        fs::rename(&provisional, output.join("index.xml"))?;
                        let source = ExpectedIndex {
                            volume_uuid: proof.construct.volume_uuid,
                            generation: proof.construct.generation,
                            location: proof.construct.location,
                            sha256: proof.construct.sha256,
                        };
                        eprintln!("取证阶段：从已核验原文流式建立导入清单");
                        ImportSpool::build(
                            File::open(output.join("index.xml"))?,
                            &output.join("import.db"),
                            source,
                        )?;
                        eprintln!("取证阶段：从同一原文流式建立 extent 缓存");
                        VolumeExtentCache::build(
                            File::open(output.join("index.xml"))?,
                            &output.join("extent.db"),
                            source,
                        )?;
                        (
                            source,
                            proof.dp_eod,
                            proof.ip_generation,
                            proof.history_depth,
                            proof.history_truncated,
                        )
                    };
                eprintln!("取证阶段：逐条核对清单与 extent 缓存");
                let spool = ImportSpool::open(&output.join("import.db"), source)?;
                let cache = VolumeExtentCache::open(&output.join("extent.db"), source)?;
                spool.verify_extent_cache(&cache)?;
                let mut after = None;
                let mut sample = None;
                loop {
                    let page = spool.candidate_page(after.as_deref(), 100)?;
                    for candidate in page.candidates {
                        let node = spool
                            .node_by_path(&candidate.physical_path)?
                            .ok_or("候选节点缺失")?;
                        if !candidate.deleted
                            && candidate.kind == NodeKind::File
                            && node.attributes.symlink().is_none()
                            && candidate.length > 0
                            && candidate.length <= 32 << 20
                        {
                            sample = Some(candidate);
                            break;
                        }
                    }
                    if sample.is_some() || page.next.is_none() {
                        break;
                    }
                    after = page.next;
                }
                let sample = sample.ok_or("测试卷没有 1..32MiB 普通参考文件")?;
                eprintln!("取证阶段：仅按参考文件的 extent 读取内容");
                let mut reference = File::create_new(output.join("reference.bin"))?;
                let bytes = if legacy_tree {
                    LtfsVolume::mount_with_budget(&device, &budget)?
                        .read_file_to_writer(&sample.physical_path, &mut reference)?
                } else {
                    cache.read_file_to_writer(
                        &device,
                        &label,
                        &sample.physical_path,
                        &mut reference,
                    )?
                };
                reference.sync_all()?;
                let mut reference = File::open(output.join("reference.bin"))?;
                let mut hash = Sha256::new();
                let mut buffer = [0; 64 << 10];
                loop {
                    let n = reference.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    hash.update(&buffer[..n]);
                }
                let digest = format!("{:x}", hash.finalize());
                if bytes != sample.length || (!sample.sha256.is_empty() && digest != sample.sha256)
                {
                    return Err("实际参考文件与带上长度/摘要不符".into());
                }
                verify_holder(&device.device, device.key)?;
                let vcr_after = mam.read_vcr()?.ok_or("缺少结束 VCR")?;
                if vcr_before != vcr_after {
                    return Err("只读扫描期间 VCR 变化".into());
                }
                let result = json!({"fixture_only":true,"barcode":barcode,"volume_uuid":volume_uuid.to_string(),
                    "drive_serial":drive_serial,"generation":source.generation,
                    "partition":source.location.partition.to_string(),"start_block":source.location.start_block,
                    "sha256":source.sha256.iter().map(|b|format!("{b:02x}")).collect::<String>(),
                    "files":spool.stats().files,"directories":spool.stats().directories,
                    "reference_path":sample.logical_path,"reference_bytes":bytes,"reference_sha256":digest,"reference_index_sha256_known":!sample.sha256.is_empty(),"index_budget_mib":index_mib,
                    "dp_eod":dp_eod,"ip_generation":ip_generation,"history_depth":history_depth,"history_truncated":history_truncated,
                    "mode":if legacy_tree{"legacy_tree"}else{"streaming"},"source":"actual read-only media scan","vcr":vcr_before.iter().map(|b|format!("{b:02x}")).collect::<String>()});
                save(&output.join("source.json"), &result)?;
                println!("{result}");
            }
        }
        Ok(())
    }
}
