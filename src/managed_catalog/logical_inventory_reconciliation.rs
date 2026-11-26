use super::*;
use std::collections::BTreeMap;

// The numeric address alone is not an identity: a changed start address can
// leave old addresses in range while changing their logical slot numbers.
fn layout(observation: &Value) -> Value {
    let mut drives: Vec<_> = observation["drives"]
        .as_array()
        .expect("validated drives")
        .iter()
        .map(|d| {
            (
                d["address"].as_u64().expect("validated address"),
                d["serial"].as_str().expect("validated serial").to_owned(),
            )
        })
        .collect();
    drives.sort();
    json!({"version":1,"slot_layout":observation["slot_layout"],
        "num_of_slots":observation["num_of_slots"],"num_of_ieslots":observation["num_of_ieslots"],"drives":drives})
}

fn scoped_layout_changed(
    original: &Value,
    current: &Value,
    resources: &LogicalLibraryResources,
) -> bool {
    let drive_row = |layout: &Value, serial: &str| {
        layout["drives"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|drive| drive[1] == serial)
            .and_then(|drive| drive[0].as_u64())
    };
    (!resources.storage_slots.is_empty()
        && (original["slot_layout"]["storage_start"] != current["slot_layout"]["storage_start"]
            || original["num_of_slots"] != current["num_of_slots"]))
        || (!resources.ie_slots.is_empty()
            && (original["slot_layout"]["ie_start"] != current["slot_layout"]["ie_start"]
                || original["num_of_ieslots"] != current["num_of_ieslots"]))
        || resources
            .drives
            .iter()
            .any(|id| drive_row(original, id) != drive_row(current, id))
}

pub(super) async fn establish_layout(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    observation: &Value,
) -> Result<()> {
    let key = at(ns, serial, b'B');
    let current = layout(observation);
    if let Some(raw) = tr.get(&key, false).await? {
        let original: Value =
            serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("原始元素布局损坏"))?;
        if original != current {
            return Err(CatalogError::Invalid("当前元素布局与原始布局不符"));
        }
    } else {
        // Never infer a baseline for an already configured legacy library.
        if !read_page(tr, &at(ns, serial, b'l'), None, 1, false)
            .await?
            .items
            .is_empty()
        {
            return Err(CatalogError::Invalid(
                "缺少可信的原始元素布局，须先受控核验",
            ));
        }
        tr.set(&key, current.to_string().as_bytes());
    }
    Ok(())
}

async fn definitions_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
) -> Result<Vec<CreateLogicalLibrary>> {
    let prefix = at(ns, serial, b'l');
    let page = read_page(tr, &prefix, None, MAX_RESOURCES, false).await?;
    if page.next.is_some() {
        return Err(CatalogError::Corrupt("逻辑带库数量超过库存容量上限"));
    }
    page.items
        .into_iter()
        .map(|(key, value)| decode(uuid_from(&key[prefix.len()..])?, &value))
        .collect()
}

pub(super) async fn conflict_at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    logical: Uuid,
) -> Result<Option<Value>> {
    let key = by_id(at(ns, serial, b'R'), logical);
    let Some(raw) = tr.get(&key, false).await? else {
        return Ok(None);
    };
    let row: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("逻辑带库对账记录损坏"))?;
    if row["detail"].as_str().is_none_or(str::is_empty) || row["operation"].is_null() {
        return Err(CatalogError::Corrupt("逻辑带库对账记录缺少原因或原操作"));
    }
    Ok(Some(row))
}

pub(super) async fn require_ready(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    logical: Uuid,
) -> Result<()> {
    if let Some(row) = conflict_at(tr, ns, serial, logical).await? {
        return Err(CatalogError::LogicalInventoryBlocked(
            row["detail"].as_str().expect("已校验对账原因").to_owned(),
        ));
    }
    Ok(())
}

fn proof_key(handoff: &LibraryHandoff) -> Vec<u8> {
    by_id(
        at(handoff.namespace, &handoff.library_serial, b'A'),
        handoff.operation,
    )
}

pub(in crate::managed_catalog) async fn require_acquire_reconciliation(
    tr: &Transaction,
    handoff: &LibraryHandoff,
) -> Result<()> {
    let ns = handoff.namespace;
    let serial = &handoff.library_serial;
    let prefix = at(ns, serial, b'l');
    if read_page(tr, &prefix, None, 1, false)
        .await?
        .items
        .is_empty()
    {
        return Ok(());
    }
    if tr.get(&proof_key(handoff), false).await?.as_deref()
        != Some(handoff.owner_epoch.to_be_bytes().as_slice())
    {
        return Err(CatalogError::Incomplete);
    }
    Ok(())
}

pub(in crate::managed_catalog) async fn reconcile_after_acquire(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    owner: u64,
    observation: &Value,
) -> Result<()> {
    let handoff = management::handoff::read_at(tr, ns, serial, None).await?;
    let Some(handoff) = handoff else {
        return Ok(());
    };
    let blocked = at(ns, serial, b'R');
    match handoff.phase {
        LibraryHandoffPhase::Loading if handoff.owner_epoch == owner => {}
        LibraryHandoffPhase::Online => {}
        _ => return Ok(()),
    }
    if observation["slot_layout"].is_null() {
        if !requests::prefix_has_rows(tr, &at(ns, serial, b'l')).await? {
            return Ok(());
        }
        return Err(CatalogError::Incomplete);
    }
    let current = layout(observation);
    let layout_key = at(ns, serial, b'B');
    let mut original: Option<Value> = tr
        .get(&layout_key, false)
        .await?
        .map(|raw| serde_json::from_slice(&raw))
        .transpose()
        .map_err(|_| CatalogError::Corrupt("原始元素布局损坏"))?;
    if handoff.phase == LibraryHandoffPhase::Online
        && original.as_ref() == Some(&current)
        && !requests::prefix_has_rows(tr, &blocked).await?
    {
        return Ok(());
    }
    let definitions = definitions_at(tr, ns, serial).await?;
    if definitions.is_empty() {
        tr.clear_range(&blocked, &prefix_end(&blocked));
        return Ok(());
    }
    let actual = observed_resources(observation);
    // Qualify the previous owner's inventory against the activation receipt.
    // Pin that map before comparing the first acquire, where drives may have
    // moved while the library was released.
    if original.is_none()
        && handoff.phase == LibraryHandoffPhase::Loading
        && let Some(release) = handoff.release
        && let Some(release) = management::handoff::read_at(tr, ns, serial, Some(release)).await?
        && release.phase == LibraryHandoffPhase::Released
        && let (Some(previous), _) = library_observations::observations_at(tr, ns, serial).await?
        && previous["round"].as_u64() == Some(release.owner_epoch)
        && !previous["slot_layout"].is_null()
        && let Some(resources) = activation::original_activation_resources(tr, ns, serial).await?
    {
        let mut original_resources = observed_resources(&previous);
        original_resources.drives.sort_unstable();
        if original_resources == resources {
            let prior_layout = layout(&previous);
            tr.set(&layout_key, prior_layout.to_string().as_bytes());
            original = Some(prior_layout);
        }
    }
    let prefix = at(ns, serial, b'r');
    let mut claims = BTreeMap::new();
    let mut conflicts = BTreeMap::new();
    for record in &definitions {
        let layout_conflict = match &original {
            Some(layout) => scoped_layout_changed(layout, &current, &record.resources)
                .then_some("当前元素布局或驱动器身份与原始布局不符"),
            None => Some("缺少可信的原始元素布局，须先受控核验"),
        };
        if let Some(detail) = layout_conflict {
            conflicts.insert(record.id, detail.to_owned());
        }
        if ensure_subset(&record.resources, &actual).is_err() {
            conflicts.insert(record.id, "当前库存缺少已分配的驱动器或槽位".to_owned());
        }
        for key in resource_keys(&prefix, &record.resources) {
            if claims.insert(key, record.id).is_some() {
                return Err(CatalogError::Corrupt("逻辑带库对账发现重复资源归属"));
            }
        }
    }
    let drives: BTreeMap<_, _> = observation["drives"]
        .as_array()
        .expect("validated drives")
        .iter()
        .map(|row| {
            (
                row["address"].as_u64().expect("validated address") as u16,
                row["serial"].as_str().expect("validated serial"),
            )
        })
        .collect();
    for volume in observation["volumes"]
        .as_array()
        .expect("validated volumes")
    {
        let barcode = volume["barcode"].as_str().expect("validated barcode");
        let Some(assignment) = ManagedCatalog::verified_assignment(tr, serial, barcode).await?
        else {
            continue;
        };
        if assignment.namespace != ns {
            return Err(CatalogError::Corrupt("物理库库存中的磁带归属其他命名空间"));
        }
        let Some(expected) = pool_library_at(tr, ns, serial, assignment.pool_id).await? else {
            continue;
        };
        let address = volume["address"].as_u64().expect("validated address") as u16;
        let kind = volume["element_type"]
            .as_u64()
            .expect("validated element type");
        let key = match kind {
            2 => Some(slot_key(&prefix, b's', address)),
            3 => Some(slot_key(&prefix, b'i', address)),
            4 => drives
                .get(&address)
                .map(|serial| drive_key(&prefix, serial)),
            _ => None,
        };
        let actual = key.and_then(|key| claims.get(&key).copied());
        if actual != Some(expected) {
            let detail = format!("磁带 {barcode} 位于池所属逻辑带库之外的元素 {address:#06x}");
            conflicts.entry(expected).or_insert_with(|| detail.clone());
            if let Some(actual) = actual {
                conflicts.entry(actual).or_insert(detail);
            }
        }

        let planned =
            ManagedCatalog::native_device_home_at(tr, ns, serial, barcode, Some(&assignment))
                .await?;
        let reported = if kind == 4 {
            volume["source_address"].as_u64().map(|value| value as u16)
        } else {
            None
        };
        if reported
            .zip(planned)
            .is_some_and(|(left, right)| left != right)
        {
            conflicts
                .entry(expected)
                .or_insert_with(|| format!("磁带 {barcode} 的驱动器来源与已登记 home 不符"));
        }
        for home in [planned, reported].into_iter().flatten() {
            let home_library = claims.get(&slot_key(&prefix, b's', home)).copied();
            if home_library != Some(expected) {
                let detail = format!("磁带 {barcode} 的 home {home:#06x} 不属于池的逻辑带库");
                conflicts.entry(expected).or_insert_with(|| detail.clone());
                if let Some(other) = home_library {
                    conflicts.entry(other).or_insert(detail);
                }
            }
        }
    }

    tr.clear_range(&blocked, &prefix_end(&blocked));
    let proof = proof_key(&handoff);
    for (logical, detail) in conflicts {
        let row = json!({"operation":handoff.operation,"observed_at":observation["observed_at"],"detail":detail}).to_string();
        tr.set(&by_id(blocked.clone(), logical), row.as_bytes());
        if handoff.phase == LibraryHandoffPhase::Loading {
            let original = by_id(proof.clone(), logical);
            if tr.get(&original, false).await?.is_none() {
                tr.set(&original, row.as_bytes());
            }
        }
    }
    if handoff.phase == LibraryHandoffPhase::Loading {
        tr.set(&proof, &owner.to_be_bytes());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changer::commands::MediumChanger;
    use crate::scsi::{
        reservation::ReservationKey,
        sim::{SimCartridge, SimLibrary},
    };

    #[test]
    fn layout_tracks_slot_indices_and_drive_identity() {
        let original = json!({"slot_layout":{"storage_start":1000,"ie_start":100},
            "num_of_slots":4,"num_of_ieslots":1,
            "drives":[{"address":1,"serial":"A"},{"address":2,"serial":"B"}]});
        let mut shifted = original.clone();
        shifted["slot_layout"]["storage_start"] = json!(999);
        shifted["num_of_slots"] = json!(5);
        assert_ne!(layout(&original), layout(&shifted));
        let mut swapped = original.clone();
        swapped["drives"][0]["serial"] = json!("B");
        swapped["drives"][1]["serial"] = json!("A");
        assert_ne!(layout(&original), layout(&swapped));
        let mut reordered = original.clone();
        reordered["drives"].as_array_mut().unwrap().reverse();
        assert_eq!(layout(&original), layout(&reordered));
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；使用模拟库存及设备交还回执"]
    async fn acquire_reconciles_external_move_before_exposing_logical_pools() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let ns = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let serial = format!("RECONCILE-{ns}");
        let challenge = Uuid::new_v4();
        let barriers: Vec<_> = (1..=3).map(|id| json!({"version":1,"library_serial":serial,
            "challenge":challenge,"node_id":id,"term":7,"leader":2,"role":if id==2 {"Leader"} else {"Follower"},
            "members":{"1":"a:1","2":"b:2","3":"c:3"},"quiesce_index":89,"frozen_index":90,
            "applied_index":92,"executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true})).collect();
        let route = catalog
            .initialize_native_library(
                ns,
                &verify_native_barriers(&serial, challenge, &barriers).unwrap(),
            )
            .await
            .unwrap();
        catalog
            .enable_library_handoff_offline(&serial, ns)
            .await
            .unwrap();
        catalog
            .enable_logical_business_fixture(&serial)
            .await
            .unwrap();
        let owner = route.initial_owner_epoch;
        let sim = SimLibrary::new(1, 4, 1);
        let transport = sim.changer();
        let mut changer = MediumChanger::new(&transport);
        let map = changer.load_address_map().unwrap().clone();
        let storage = map.storage_start;
        let drive = "SIMDRV0000";
        let observation = |round| {
            let rows = changer.read_all_status().unwrap();
            let volumes: Vec<_> = rows.iter().filter_map(|row| row.volume_tag.as_ref().map(|barcode| {
                json!({"barcode":barcode,"address":row.address,"element_type":row.element_type as u8,
                    "medium_type":row.medium_type,"source_address":row.source_address,"home_slot":null})
            })).collect();
            json!({"library_serial":serial,"owner":2,"round":round,
                "observed_at":crate::ltfs::label::ltfs_time_now(),"volumes":volumes,
                "drives":[{"serial":drive,"address":map.dt_start,"barcode":null}],
                "num_of_slots":map.storage_count,"num_of_ieslots":map.ie_count,
                "scsi_vendor_id":"SIM","scsi_product_id":"SIM CHANGER","scsi_firmware_revision":"1",
                "slot_layout":{"storage_start":storage,"ie_start":map.ie_start}})
        };
        catalog
            .record_active_library_inventory(&serial, route, owner, &observation(owner))
            .await
            .unwrap();
        let mut scopes = Vec::new();
        for offset in 0..3 {
            let logical = Uuid::new_v4();
            let pool = PoolRecord {
                id: Uuid::new_v4(),
                library_serial: serial.clone(),
                name: format!("pool-{offset}"),
                file_limit: 100,
                created_at_millis: 1,
                revision: 1,
                policy: PoolPolicy::default(),
            };
            let request = CreateLogicalLibrary {
                version: 1,
                id: logical,
                name: format!("logical-{offset}"),
                resources: LogicalLibraryResources {
                    drives: if offset == 0 {
                        vec![drive.into()]
                    } else {
                        Vec::new()
                    },
                    storage_slots: vec![storage + offset],
                    ie_slots: Vec::new(),
                },
            };
            assert!(matches!(
                catalog
                    .create_logical_library(&serial, route, owner, request)
                    .await
                    .unwrap(),
                Creation::Created(_)
            ));
            assert!(matches!(
                catalog
                    .create_admin_pool(&serial, route, owner, Some(logical), &pool)
                    .await
                    .unwrap(),
                ActivePoolCreation::Created(_)
            ));
            scopes.push((logical, pool.id));
        }
        let barcode = "MOVED0L8";
        let tr = catalog.db.create_trx().unwrap();
        // Simulate the previous activation format, whose receipt captured the
        // full resource set before any later logical allocation.
        let activation = Uuid::new_v4();
        let original_resources = observed_resources(&observation(owner));
        tr.set(
            &by_id(at(ns, &serial, b'E'), activation),
            json!({"library_serial":serial,"namespace":ns,"operation":activation,
                "status":"enabled","logical_library":{"id":activation,"name":"default",
                    "revision":1,"resources":original_resources}})
            .to_string()
            .as_bytes(),
        );
        tr.clear(&at(ns, &serial, b'B'));
        tr.set(
            &assignment_key(&serial, barcode),
            &encode_assignment(ns, scopes[0].1, None),
        );
        tr.set(
            &namespace_assignment_key(ns, scopes[0].1, &serial, barcode),
            &encode_namespace_assignment(&serial, barcode),
        );
        tr.commit().await.unwrap();
        let release = LibraryHandoff {
            library_serial: serial.clone(),
            namespace: ns,
            operation: Uuid::new_v4(),
            release: None,
            phase: LibraryHandoffPhase::Releasing,
            release_key: ReservationKey::new(2, owner).0,
            release_retry_used: false,
            attempt_key: None,
            owner_epoch: owner,
            devices: vec![serial.clone(), drive.into()],
            detail: None,
        };
        catalog
            .begin_library_handoff(&release, route)
            .await
            .unwrap();
        let mut released = release.clone();
        released.phase = LibraryHandoffPhase::Released;
        catalog
            .update_library_handoff(route, &release, &released)
            .await
            .unwrap();

        sim.insert_cartridge(SimCartridge::blank(barcode, 1 << 20), 0)
            .unwrap();
        changer.move_medium(storage, storage + 1).unwrap();
        let loading = catalog
            .begin_library_reload(&serial, route, operation, release.operation)
            .await
            .unwrap();
        let mut attempt = loading.clone();
        let next_owner = owner + 1;
        attempt.owner_epoch = next_owner;
        attempt.attempt_key = Some(ReservationKey::new(2, next_owner).0);
        catalog
            .update_library_handoff(route, &loading, &attempt)
            .await
            .unwrap();
        catalog
            .claim_active_owner_epoch(&serial, next_owner)
            .await
            .unwrap();
        let mut online = attempt.clone();
        online.phase = LibraryHandoffPhase::Online;
        assert!(
            catalog
                .update_library_handoff(route, &attempt, &online)
                .await
                .is_err()
        );
        let mut incomplete = observation(next_owner);
        incomplete.as_object_mut().unwrap().remove("slot_layout");
        assert!(matches!(
            catalog
                .record_active_library_inventory(&serial, route, next_owner, &incomplete)
                .await
                .unwrap_err()
                .domain(),
            CatalogError::Incomplete
        ));
        assert!(
            catalog
                .update_library_handoff(route, &attempt, &online)
                .await
                .is_err()
        );

        let mut shifted_acquire = observation(next_owner);
        shifted_acquire["slot_layout"]["storage_start"] = json!(storage - 1);
        shifted_acquire["num_of_slots"] = json!(map.storage_count + 1);
        shifted_acquire["drives"][0]["address"] = json!(map.dt_start + 1);
        catalog
            .record_active_library_inventory(&serial, route, next_owner, &shifted_acquire)
            .await
            .unwrap();
        let tr = catalog.db.create_trx().unwrap();
        let pinned = tr
            .get(&at(ns, &serial, b'B'), false)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&pinned).unwrap(),
            layout(&observation(owner))
        );
        for (logical, _) in &scopes {
            let conflict = conflict_at(&tr, ns, &serial, *logical)
                .await
                .unwrap()
                .unwrap();
            assert!(conflict["detail"].as_str().unwrap().contains("布局"));
        }
        catalog
            .record_active_library_inventory(&serial, route, next_owner, &observation(next_owner))
            .await
            .unwrap();
        catalog
            .update_library_handoff(route, &attempt, &online)
            .await
            .unwrap();
        for (logical, pool) in &scopes[..2] {
            assert!(
                matches!(catalog.check_active_pool_gate(&serial, route, next_owner, *pool)
                .await.unwrap_err().domain(), CatalogError::LogicalInventoryBlocked(detail) if detail.contains(barcode))
            );
            let view = catalog
                .logical_library_view(&serial, route, Some(&logical.to_string()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(view["reconciliation"]["operation"], json!(operation));
        }
        catalog
            .check_active_pool_gate(&serial, route, next_owner, scopes[2].1)
            .await
            .unwrap();
        assert_eq!(
            catalog
                .tape_assignment(&serial, barcode)
                .await
                .unwrap()
                .unwrap()
                .pool_id,
            scopes[0].1
        );

        changer.move_medium(storage + 1, storage).unwrap();
        catalog
            .record_active_library_inventory(&serial, route, next_owner, &observation(next_owner))
            .await
            .unwrap();
        catalog
            .check_active_pool_gate(&serial, route, next_owner, scopes[0].1)
            .await
            .unwrap();
        // All allocated addresses remain present, but their logical indices shift.
        let mut shifted = observation(next_owner);
        shifted["slot_layout"]["storage_start"] = json!(storage - 1);
        shifted["num_of_slots"] = json!(map.storage_count + 1);
        catalog
            .record_active_library_inventory(&serial, route, next_owner, &shifted)
            .await
            .unwrap();
        for (_, pool) in &scopes {
            assert!(matches!(
                catalog
                    .check_active_pool_gate(&serial, route, next_owner, *pool)
                    .await
                    .unwrap_err()
                    .domain(),
                CatalogError::LogicalInventoryBlocked(_)
            ));
        }
        catalog
            .record_active_library_inventory(&serial, route, next_owner, &observation(next_owner))
            .await
            .unwrap();
        for (_, pool) in &scopes {
            catalog
                .check_active_pool_gate(&serial, route, next_owner, *pool)
                .await
                .unwrap();
        }
        let original = catalog
            .library_handoff(&serial, route, Some(release.operation))
            .await
            .unwrap();
        assert_eq!(original, Some(released));
        let current = catalog
            .library_handoff(&serial, route, Some(operation))
            .await
            .unwrap();
        assert_eq!(current, Some(online.clone()));
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            tr.get(&by_id(proof_key(&online), scopes[0].0), false)
                .await
                .unwrap()
                .is_some()
        );
    }
}
