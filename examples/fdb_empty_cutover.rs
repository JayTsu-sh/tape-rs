//! 隔离现场验收辅助程序：只接受三份真实节点导出的空目录，准备 FDB 路由或读取 owner epoch。
//! 不访问设备。须显式设置 TAPE_RS_FDB_ISOLATED_ACCEPTANCE=1；禁止指向正式 FDB。

#[cfg(not(feature = "fdb"))]
fn main() {
    eprintln!("须使用 --features fdb 构建现场验收程序");
    std::process::exit(2);
}

#[cfg(feature = "fdb")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("TAPE_RS_FDB_ISOLATED_ACCEPTANCE").as_deref() != Ok("1") {
        return Err("须显式设置 TAPE_RS_FDB_ISOLATED_ACCEPTANCE=1，并使用隔离 FDB".into());
    }
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 || !["prepare", "prepare-external", "owner"].contains(&args[1].as_str()) {
        return Err(
            "用法：fdb_empty_cutover prepare|prepare-external|owner CLUSTER_FILE EXPORT_MANIFEST|LIBRARY_SERIAL"
                .into(),
        );
    }
    let _network = unsafe { foundationdb::boot() };
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run(&args[1], &args[2], &args[3]))
}

#[cfg(feature = "fdb")]
async fn run(mode: &str, cluster: &str, input: &str) -> Result<(), Box<dyn std::error::Error>> {
    use serde_json::json;
    use std::path::PathBuf;
    use tape_rs::managed_catalog::{
        ControlOutcome, ManagedCatalog, NamespaceVerificationOutcome, PrepareRouteOutcome,
        inspect_frozen_snapshot, verify_node_export,
    };
    use uuid::Uuid;

    let catalog = ManagedCatalog::open(Some(cluster))?;
    if mode == "owner" {
        println!(
            "{}",
            json!({"serial":input,"epoch":catalog.active_library_owner(input).await?.map(|(_, epoch)| epoch)})
        );
        return Ok(());
    }
    struct Export {
        node_id: u64,
        path: PathBuf,
        applied_index: u64,
        frozen_index: u64,
        sha256: String,
    }
    struct Manifest {
        serial: String,
        source_node: u64,
        legacy_owner_epoch: u64,
        work_dir: PathBuf,
        exports: [Export; 3],
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(input)?)?;
    let exports = value["exports"]
        .as_array()
        .ok_or("缺少 exports 数组")?
        .iter()
        .map(|v| {
            Ok(Export {
                node_id: v["node_id"].as_u64().ok_or("缺少导出节点编号")?,
                path: PathBuf::from(v["path"].as_str().ok_or("缺少导出路径")?),
                applied_index: v["applied_index"].as_u64().ok_or("缺少导出应用水位")?,
                frozen_index: v["frozen_index"].as_u64().ok_or("缺少冻结索引")?,
                sha256: v["sha256"].as_str().ok_or("缺少导出摘要")?.into(),
            })
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    let manifest = Manifest {
        serial: value["serial"].as_str().ok_or("缺少带库序列号")?.into(),
        source_node: value["source_node"].as_u64().ok_or("缺少源节点编号")?,
        legacy_owner_epoch: value["legacy_owner_epoch"]
            .as_u64()
            .filter(|epoch| *epoch > 0 && *epoch <= 0xFFFF_FFFF_FFFF)
            .ok_or("缺少已确认的旧 Raft/PR owner 轮次")?,
        work_dir: PathBuf::from(value["work_dir"].as_str().ok_or("缺少私有工作目录")?),
        exports: exports.try_into().map_err(|_| "必须提供三节点导出")?,
    };
    let mut evidence = Vec::new();
    for export in &manifest.exports {
        let identity = inspect_frozen_snapshot(&export.path)?;
        if identity.row_counts != [0; 4] {
            return Err("此验收程序只接受无池、磁带、文件和任务的空目录导出".into());
        }
        if export.sha256.len() != 64 || !export.sha256.is_ascii() {
            return Err("无效节点导出 SHA-256".into());
        }
        let mut digest = [0; 32];
        for (offset, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&export.sha256[offset * 2..offset * 2 + 2], 16)?;
        }
        evidence.push(verify_node_export(
            &export.path,
            export.node_id,
            export.applied_index,
            export.frozen_index,
            digest,
        )?);
    }
    let source = manifest
        .exports
        .iter()
        .find(|e| e.node_id == manifest.source_node)
        .ok_or("源节点没有导出记录")?;
    if manifest.legacy_owner_epoch >= source.frozen_index {
        return Err("旧 owner 轮次必须早于真实目录冻结索引".into());
    }
    if catalog
        .active_library_route(&manifest.serial)
        .await?
        .is_some()
    {
        return Err("隔离带库已经激活，请先核对现有验收进程".into());
    }
    let certificate = if let Some(route) = catalog.prepared_library_route(&manifest.serial).await? {
        let certificate = catalog
            .certified_prepared_cutover(&manifest.serial)
            .await?
            .ok_or("已有预备路由缺少完整证书，拒绝覆盖")?;
        let origin = catalog
            .staged_import_summary(route.namespace)
            .await?
            .ok_or("预备导入清单缺失")?
            .audit
            .origin;
        match origin {
            tape_rs::managed_catalog::ImportOrigin::LegacySnapshot if mode == "prepare" => {}
            tape_rs::managed_catalog::ImportOrigin::ExternalMedia { .. }
                if mode == "prepare-external" =>
            {
                let expected: Uuid = value["namespace"]
                    .as_str()
                    .ok_or("缺少外部 namespace")?
                    .parse()?;
                if expected != route.namespace {
                    return Err("预备外部命名空间不一致".into());
                }
            }
            _ => return Err("预备路由的导入来源类型不符".into()),
        }
        let ids: std::collections::BTreeSet<_> =
            evidence.iter().map(|proof| proof.node_id).collect();
        if ids.len() != 3
            || certificate.route != route
            || certificate.selected_node != manifest.source_node
            || certificate.frozen_index != source.frozen_index
            || !evidence.iter().all(|proof| {
                let identity = proof.identity();
                identity.frozen_index == certificate.frozen_index
                    && identity.catalog_sha256 == certificate.catalog_sha256
                    && certificate.nodes.iter().any(|node| {
                        node.node_id == proof.node_id
                            && node.applied_index == identity.applied_index
                            && node.snapshot_sha256 == identity.snapshot_sha256
                    })
            })
        {
            return Err("预备证书与当前三节点导出不一致，拒绝覆盖或激活".into());
        }
        certificate
    } else {
        let namespace = if mode == "prepare-external" {
            value["namespace"]
                .as_str()
                .ok_or("外部导入须提供稳定 namespace")?
                .parse()?
        } else {
            Uuid::new_v4()
        };
        eprintln!("验收阶段：begin managed；namespace={namespace}");
        if !catalog.begin_managed_import(namespace).await?
            && catalog.namespace_state(namespace).await?
                != Some(tape_rs::managed_catalog::NamespaceState::Building)
        {
            return Err("此导入不能从当前命名空间状态继续".into());
        }
        if !matches!(
            catalog
                .register_physical_library(namespace, &manifest.serial)
                .await?,
            ControlOutcome::Created | ControlOutcome::AlreadyExists
        ) {
            return Err("物理带库登记失败".into());
        }
        eprintln!("验收阶段：stage audited reconcile；namespace={namespace}");
        let staged = if mode == "prepare-external" {
            let sources = external_sources(
                &catalog,
                namespace,
                &manifest.serial,
                manifest.legacy_owner_epoch,
                &value["sources"],
            )
            .await?;
            let bindings: Vec<_> = sources.iter().map(|(b, _)| b.clone()).collect();
            let volumes = sources
                .iter()
                .map(|(binding, spool)| tape_rs::managed_catalog::ImportVolume {
                    pool_id: binding.placement.pool_id,
                    barcode: binding.placement.barcode.clone(),
                    volume_id: binding.placement.volume_id,
                    generation: binding.source.generation,
                    spool,
                })
                .collect();
            catalog
                .stage_external_import(
                    namespace,
                    &source.path,
                    &manifest.work_dir,
                    volumes,
                    &bindings,
                )
                .await?
        } else {
            let staged = catalog
                .stage_audited_reconcile(
                    namespace,
                    &source.path,
                    &manifest.work_dir,
                    Vec::new(),
                    &[],
                )
                .await?;
            assert_eq!(staged.paths, 0);
            staged
        };
        eprintln!(
            "验收阶段：已暂存 {} 路径，来源 {:?}",
            staged.paths, staged.audit.origin
        );
        eprintln!("验收阶段：confirm bound import；namespace={namespace}");
        catalog.confirm_bound_staged_import(namespace).await?;
        eprintln!("验收阶段：materialize bound import；namespace={namespace}");
        while !catalog
            .materialize_bound_import_page(namespace, 100)
            .await?
            .complete()
        {}
        eprintln!("验收阶段：freeze projection；namespace={namespace}");
        assert_eq!(
            catalog.freeze_bound_staged_import(namespace).await?,
            NamespaceVerificationOutcome::Frozen
        );
        eprintln!("验收阶段：verify namespace；namespace={namespace}");
        loop {
            match catalog.verify_namespace_page(namespace, 100).await? {
                NamespaceVerificationOutcome::Progress { .. } => continue,
                NamespaceVerificationOutcome::Verified(_) => break,
                other => return Err(format!("空目录核验失败：{other:?}").into()),
            }
        }
        eprintln!("验收阶段：verify copy fence；namespace={namespace}");
        let copies = catalog.verify_bound_import_copies(namespace).await?;
        if mode == "prepare" {
            assert_eq!(copies, 0);
        }
        eprintln!("验收阶段：已核验 {copies} 份介质副本");
        eprintln!("验收阶段：inspect copy fence；namespace={namespace}");
        catalog.inspect_bound_import_copy_fence(namespace).await?;
        eprintln!("验收阶段：seal import；namespace={namespace}");
        assert!(catalog.seal_bound_staged_import(namespace).await?);
        eprintln!("验收阶段：prepare route；namespace={namespace}");
        let route = match catalog
            .prepare_library_route(&manifest.serial, namespace, None)
            .await?
        {
            PrepareRouteOutcome::Prepared(route) => route,
            other => return Err(format!("路由准备失败：{other:?}").into()),
        };
        eprintln!("验收阶段：certify route；namespace={namespace}");
        let evidence = evidence
            .clone()
            .try_into()
            .map_err(|_| "必须提供三节点导出")?;
        assert!(
            catalog
                .certify_prepared_cutover(&manifest.serial, route, manifest.source_node, evidence)
                .await?
        );
        catalog
            .certified_prepared_cutover(&manifest.serial)
            .await?
            .ok_or("证书缺失")?
    };
    let namespace = certificate.route.namespace;
    // This is the old round already fenced by the real Raft owner, before the
    // three nodes froze the empty catalog. Activation still grants no device IO.
    catalog
        .advance_owner_epoch(&manifest.serial, manifest.legacy_owner_epoch)
        .await?;
    eprintln!("验收阶段：activate route；namespace={namespace}");
    let active = catalog
        .activate_prepared_cutover(&manifest.serial, certificate)
        .await?;
    println!(
        "{}",
        json!({"serial":manifest.serial,"namespace":namespace.to_string(),"frozen_index":active.frozen_index,"initial_owner_epoch":active.initial_owner_epoch,"origin":if mode=="prepare-external"{"external_media"}else{"legacy_snapshot"}})
    );
    Ok(())
}

#[cfg(feature = "fdb")]
async fn external_sources(
    catalog: &tape_rs::managed_catalog::ManagedCatalog,
    namespace: uuid::Uuid,
    serial: &str,
    epoch: u64,
    values: &serde_json::Value,
) -> Result<
    Vec<(
        tape_rs::managed_catalog::ImportSourceBinding,
        tape_rs::managed_catalog::ImportSpool,
    )>,
    Box<dyn std::error::Error>,
> {
    use std::path::PathBuf;
    use tape_rs::catalog::volume_extents::ExpectedIndex;
    use tape_rs::ltfs::index::IndexLocation;
    use tape_rs::managed_catalog::*;
    use uuid::Uuid;
    let rows = values
        .as_array()
        .filter(|rows| !rows.is_empty())
        .ok_or("外部导入缺少 sources")?;
    catalog.advance_owner_epoch(serial, epoch).await?;
    let mut sources = Vec::new();
    for row in rows {
        let directory = PathBuf::from(row["directory"].as_str().ok_or("缺少本地源目录")?);
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.join("source.json"))?)?;
        let hex = saved["sha256"].as_str().ok_or("源清单缺少摘要")?;
        if hex.len() != 64 || !hex.is_ascii() {
            return Err("无效源摘要".into());
        }
        let mut digest = [0; 32];
        for (i, b) in digest.iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)?;
        }
        let partition = saved["partition"]
            .as_str()
            .filter(|p| matches!(*p, "a" | "b"))
            .ok_or("源分区无效")?;
        let source = ExpectedIndex {
            volume_uuid: saved["volume_uuid"]
                .as_str()
                .ok_or("源 UUID 缺失")?
                .parse()?,
            generation: saved["generation"].as_u64().ok_or("源代数缺失")?,
            location: IndexLocation {
                partition: partition.chars().next().unwrap(),
                start_block: saved["start_block"].as_u64().ok_or("源位置缺失")?,
            },
            sha256: digest,
        };
        let pool: Uuid = row["pool_id"].as_str().ok_or("目标池缺失")?.parse()?;
        let name = row["pool_name"].as_str().ok_or("目标池名称缺失")?;
        let outcome = catalog
            .create_pool(
                namespace,
                &PoolRecord {
                    id: pool,
                    name: name.into(),
                    library_serial: serial.into(),
                    file_limit: 100_000_000,
                    policy: PoolPolicy::default(),
                    revision: 1,
                },
            )
            .await?;
        if !matches!(
            outcome,
            ControlOutcome::Created | ControlOutcome::AlreadyExists
        ) {
            return Err(format!("外部池登记失败：{outcome:?}").into());
        }
        let placement = VolumePlacement {
            volume_id: source.volume_uuid,
            namespace,
            pool_id: pool,
            library_serial: serial.into(),
            barcode: saved["barcode"].as_str().ok_or("源条码缺失")?.into(),
        };
        let outcome = catalog
            .assign_tape(namespace, pool, serial, &placement.barcode)
            .await?;
        if !matches!(
            outcome,
            ControlOutcome::Created | ControlOutcome::AlreadyExists
        ) {
            return Err(format!("外部磁带分配失败：{outcome:?}").into());
        }
        let outcome = catalog.register_volume_placement(&placement, epoch).await?;
        if !matches!(
            outcome,
            ControlOutcome::Created | ControlOutcome::AlreadyExists
        ) {
            return Err(format!("卷放置失败：{outcome:?}").into());
        }
        let scan = VolumeScan {
            generation: source.generation,
            id: row["scan_id"].as_str().ok_or("扫描 ID 缺失")?.parse()?,
        };
        let binding = ImportSourceBinding {
            placement,
            scan,
            source,
        };
        if catalog
            .verify_import_source_binding(&binding)
            .await
            .is_err()
        {
            let outcome = catalog
                .begin_volume_reconcile(serial, epoch, source.volume_uuid, scan)
                .await?;
            if outcome != VolumeOutcome::Reconciling {
                return Err(format!("来源扫描无法接续：{outcome:?}").into());
            }
            let spool = directory.join("import.db");
            let cache = directory.join("extent.db");
            let outcome = catalog
                .record_volume_source(
                    serial,
                    epoch,
                    source.volume_uuid,
                    scan,
                    source,
                    LocalVolumeSources {
                        import_spool: &spool,
                        extent_cache: &cache,
                    },
                )
                .await?;
            if !matches!(
                outcome,
                SourceOutcome::Recorded | SourceOutcome::AlreadyRecorded
            ) {
                return Err(format!("来源记录失败：{outcome:?}").into());
            }
            let empty = ScanDigest::new(source.volume_uuid, scan)?.summary();
            if !matches!(
                catalog
                    .freeze_volume_scan(serial, epoch, source.volume_uuid, scan, empty)
                    .await?,
                VerificationOutcome::Frozen | VerificationOutcome::Verified(_)
            ) {
                return Err("来源扫描封存失败".into());
            }
            if catalog
                .verify_volume_scan_page_with_cache(
                    serial,
                    epoch,
                    source.volume_uuid,
                    scan,
                    100,
                    &cache,
                )
                .await?
                != VerificationOutcome::Verified(empty)
            {
                return Err("来源扫描核验失败".into());
            }
            if !matches!(
                catalog
                    .mark_volume_ready(serial, epoch, source.volume_uuid, scan)
                    .await?,
                VolumeOutcome::Ready | VolumeOutcome::AlreadyReady
            ) {
                return Err("来源卷无法就绪".into());
            }
        }
        catalog.verify_import_source_binding(&binding).await?;
        let spool = ImportSpool::open(&directory.join("import.db"), source)?;
        if saved["files"].as_u64() != Some(spool.stats().files)
            || saved["directories"].as_u64() != Some(spool.stats().directories)
        {
            return Err("源清单数量不符".into());
        }
        sources.push((binding, spool));
    }
    Ok(sources)
}
