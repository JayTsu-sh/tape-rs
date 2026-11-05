#![cfg(feature = "fdb")]
//! 当前软件对现场控制数据的只读核对；不创建namespace、不修复或写回记录。
use serde_json::{Value, json};
use tape_rs::managed_catalog::ManagedCatalog;
use uuid::Uuid;

#[test]
#[ignore = "显式指定真实FDB cluster、带库、owner epoch、现有pool；隔离单元禁止设备访问"]
fn current_control_readers_audit_real_library_metadata_without_writes() {
    let cluster = std::env::var("TAPE_RS_FDB_LIVE_CLUSTER").expect("须指定现场cluster文件");
    let library = std::env::var("TAPE_RS_FDB_LIVE_LIBRARY").expect("须指定真实带库序列号");
    let owner: u64 = std::env::var("TAPE_RS_FDB_LIVE_OWNER_EPOCH")
        .expect("须指定现场owner epoch")
        .parse()
        .unwrap();
    let pool: Uuid = std::env::var("TAPE_RS_FDB_LIVE_POOL")
        .expect("须指定已存在的测试库pool")
        .parse()
        .unwrap();
    assert!(owner > 0 && !pool.is_nil());
    // The network guard outlives the runtime and every database/read future.
    let _network = unsafe { foundationdb::boot() };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (report, errors) = runtime.block_on(async {
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let mut report = json!({"library":library,"expected_owner":owner,"expected_pool":pool.to_string(),"writes":false,"readers_decodable":false,"producer_currentness_verified":false,"backend_end_to_end_verified":false});
        let mut errors = 0;
        let route = match catalog.active_library_route(&library).await {
            Ok(Some(route)) => {
                report["route"] = json!({"namespace":route.route.namespace.to_string(),"revision":route.route.revision,"frozen_index":route.frozen_index,"initial_owner_epoch":route.initial_owner_epoch});
                route
            }
            other => {
                report["route_error"] = json!(format!("{other:?}"));
                report["read_errors"] = json!(1);
                return (report, 1);
            }
        };
        match catalog.active_library_owner(&library).await {
            Ok(Some((actual_route, actual_owner))) if actual_route == route && actual_owner == owner => {
                report["owner"] = json!({"matches":true,"epoch":actual_owner});
            }
            other => { report["owner_error"] = json!(format!("{other:?}"));errors += 1; }
        }
        match catalog.list_active_pools_for_route(&library, route, None, 100).await {
            Ok(page) => {
                let values: std::result::Result<Vec<Value>, _> = page.items.iter().map(|p|p.to_json()).collect();
                match values {
                    Ok(values) => report["pools_first_page"] = json!({"items":values,"more":page.next.is_some()}),
                    Err(error) => { report["pools_error"] = json!(error.to_string());errors += 1; }
                }
                // Only the exact pool read below determines whether it exists;
                // a first page need not contain that pool or even this library.
            }
            Err(error) => { report["pools_error"] = json!(error.to_string());errors += 1; }
        }
        match catalog.active_pool_for_route(&library, route, pool).await {
            Ok(Some(value)) => match value.to_json() {
                Ok(value) => report["pool"] = value,
                Err(error) => { report["pool_error"] = json!(error.to_string());errors += 1; }
            },
            other => { report["pool_error"] = json!(format!("{other:?}"));errors += 1; }
        }
        match catalog.active_library_inventory(&library, route).await {
            Ok(value) => report["inventory"] = json!({"present":value.is_some(),"value":value}),
            Err(error) => { report["inventory_error"] = json!(error.to_string());errors += 1; }
        }
        match catalog.active_library_monitor(&library, route).await {
            Ok(value) => report["monitor"] = json!({"present":value.is_some(),"value":value}),
            Err(error) => { report["monitor_error"] = json!(error.to_string());errors += 1; }
        }
        report["read_errors"] = json!(errors);
        report["readers_decodable"] = json!(errors == 0);
        (report, errors)
    });
    println!("LIVE_FDB_CONTROL_READ_AUDIT {report}");
    assert_eq!(
        errors, 0,
        "现场旧数据与当前严格读取规则不一致；保留报告，不自动修复"
    );
}
