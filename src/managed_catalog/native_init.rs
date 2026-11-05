//! Empty-library initialization from a frozen Raft control barrier, without
//! importing a file catalog. This never grants a device execution permit.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const LIMIT: usize = 16_384;
const MAX_EPOCH: u64 = 0xFFFF_FFFF_FFFF;

#[derive(Clone, Debug)]
pub struct VerifiedNativeBarrier {
    serial: String,
    challenge: Uuid,
    frozen: u64,
    replies: Vec<Value>,
}

/// Replies must come directly from the three configured management endpoints.
/// Each reply must identify the target library in `library_serial`.
/// As with the management APIs, endpoint authentication is a deployment concern.
pub fn verify_native_barriers(
    serial: &str,
    challenge: Uuid,
    replies: &[Value],
) -> Result<VerifiedNativeBarrier> {
    control::validate_physical_serial(serial)?;
    if challenge.is_nil() || replies.len() != 3 {
        return Err(CatalogError::Invalid("原生初始化需要三节点冻结屏障"));
    }
    let mut replies = replies.to_vec();
    replies.sort_by_key(|value| value["node_id"].as_u64().unwrap_or(0));
    let first = &replies[0];
    let frozen = first["frozen_index"]
        .as_u64()
        .filter(|n| *n > 0 && *n < MAX_EPOCH)
        .ok_or(CatalogError::Invalid("冻结水位超出 owner 轮次范围"))?;
    let quiesce = first["quiesce_index"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= frozen)
        .ok_or(CatalogError::Invalid("目录暂停水位无效"))?;
    let term = first["term"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(CatalogError::Invalid("Raft term 无效"))?;
    let leader = first["leader"]
        .as_u64()
        .filter(|n| (1..=255).contains(n))
        .ok_or(CatalogError::Invalid("Raft leader 无效"))?;
    let members: BTreeMap<u64, String> = serde_json::from_value(first["members"].clone())
        .map_err(|_| CatalogError::Invalid("集群客户端成员配置无效"))?;
    if members.len() != 3
        || first["members"] != serde_json::to_value(&members).expect("member map encodes")
        || !members.contains_key(&leader)
        || members.keys().any(|n| !(1..=255).contains(n))
        || members.values().any(|address| {
            address.is_empty()
                || address.len() > 256
                || !address.is_ascii()
                || address.chars().any(char::is_whitespace)
                || address.contains('/')
        })
        || members.values().collect::<BTreeSet<_>>().len() != 3
    {
        return Err(CatalogError::Invalid("原生初始化要求三个不同的已配置节点"));
    }
    let mut observed = BTreeSet::new();
    for reply in &replies {
        let id = reply["node_id"]
            .as_u64()
            .ok_or(CatalogError::Invalid("屏障来源节点无效"))?;
        if !observed.insert(id)
            || !members.contains_key(&id)
            || reply["version"] != 1
            || reply["library_serial"].as_str() != Some(serial)
            || reply["challenge"] != challenge.to_string()
            || reply["frozen_index"].as_u64() != Some(frozen)
            || reply["quiesce_index"].as_u64() != Some(quiesce)
            || reply["term"].as_u64() != Some(term)
            || reply["leader"].as_u64() != Some(leader)
            || reply["applied_index"].as_u64().is_none_or(|n| n < frozen)
            || reply["executor_absent"] != true
            || reply["catalog_pending"] != false
            || reply["shutting_down"] != false
            || reply["catalog_empty"] != true
            || reply["role"].as_str() != Some(if id == leader { "Leader" } else { "Follower" })
            || reply["members"] != first["members"]
        {
            return Err(CatalogError::Invalid(
                "三节点冻结屏障、成员配置或来源不一致",
            ));
        }
    }
    let proof = VerifiedNativeBarrier {
        serial: serial.into(),
        challenge,
        frozen,
        replies,
    };
    if proof.value().to_string().len() > LIMIT - 128 {
        return Err(CatalogError::Invalid("原生初始化屏障过大"));
    }
    Ok(proof)
}

impl VerifiedNativeBarrier {
    fn value(&self) -> Value {
        json!({"serial":self.serial,"challenge":self.challenge.to_string(),"replies":self.replies})
    }
    fn certificate(&self, namespace: Uuid) -> Vec<u8> {
        let value = json!({"version":1,"namespace":namespace.to_string(),"barrier":self.value()});
        [b"N".as_slice(), value.to_string().as_bytes()].concat()
    }
}

fn decode(bytes: &[u8]) -> Result<(Uuid, VerifiedNativeBarrier)> {
    if bytes.len() > LIMIT {
        return Err(CatalogError::Corrupt("原生激活证明过大"));
    }
    let bytes = bytes
        .strip_prefix(b"N")
        .ok_or(CatalogError::Corrupt("原生激活证明类型"))?;
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| CatalogError::Corrupt("原生激活证明编码"))?;
    if value["version"] != 1 || value.to_string().as_bytes() != bytes {
        return Err(CatalogError::Corrupt("原生激活证明格式"));
    }
    let namespace = value["namespace"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|n| !n.is_nil())
        .ok_or(CatalogError::Corrupt("原生命名空间无效"))?;
    let barrier = &value["barrier"];
    let serial = barrier["serial"]
        .as_str()
        .ok_or(CatalogError::Corrupt("原生带库序列号缺失"))?;
    let challenge = barrier["challenge"]
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or(CatalogError::Corrupt("原生屏障请求身份缺失"))?;
    let replies = barrier["replies"]
        .as_array()
        .ok_or(CatalogError::Corrupt("原生屏障来源缺失"))?;
    let proof = verify_native_barriers(serial, challenge, replies)
        .map_err(|_| CatalogError::Corrupt("原生屏障校验失败"))?;
    if proof.certificate(namespace)[1..] != *bytes {
        return Err(CatalogError::Corrupt("原生激活证明非规范编码"));
    }
    Ok((namespace, proof))
}

fn root_value() -> Vec<u8> {
    encode_node(&Node {
        kind: NodeKind::Directory,
        parent: Uuid::nil(),
        name: vec![],
        attributes: None,
    })
}

pub(super) async fn active_context(
    tr: &Transaction,
    serial: &str,
    active: ActiveLibraryRoute,
    certificate: &[u8],
) -> Result<()> {
    let (namespace, proof) = decode(certificate)?;
    if proof.serial != serial
        || active.route.namespace != namespace
        || active.route.revision != 1
        || active.frozen_index != proof.frozen
        || active.initial_owner_epoch != proof.frozen + 1
        || Sha256::digest(certificate)[..] != active.certificate_sha256
        || tr.get(&routing::route_key(serial), false).await?.as_deref()
            != Some(routing::encode_route(active.route).as_slice())
        || tr.get(&namespace_key(namespace), false).await?.as_deref()
            != Some(&[NamespaceState::Active as u8])
        || tr
            .get(&control::managed_key(namespace), false)
            .await?
            .as_deref()
            != Some(&[])
        || tr
            .get(&control::library_key(namespace, serial), false)
            .await?
            .as_deref()
            != Some(&[])
        || tr
            .get(&node_key(namespace, Uuid::nil()), false)
            .await?
            .as_deref()
            != Some(root_value().as_slice())
    {
        return Err(CatalogError::Corrupt("原生目录激活证明或根记录不一致"));
    }
    control::require_managed_layout(tr, namespace).await?;
    let epoch = tr
        .get(&owner_key(serial), false)
        .await?
        .ok_or(CatalogError::Incomplete)?;
    if epoch.len() != 8 || read_u64(&epoch) < active.initial_owner_epoch {
        return Err(CatalogError::Corrupt("原生目录 owner epoch 无效"));
    }
    Ok(())
}

impl ManagedCatalog {
    /// Check an existing result before collecting new barriers, so retrying a
    /// completed initialization never needs to stop an already serving owner.
    pub async fn native_initialization_status(
        &self,
        serial: &str,
        namespace: Uuid,
    ) -> Result<Option<ActiveLibraryRoute>> {
        control::validate_physical_serial(serial)?;
        if namespace.is_nil() {
            return Err(CatalogError::Invalid("namespace 不能为 nil"));
        }
        let tr = self.db.create_trx()?;
        let Some(raw) = tr.get(&routing::active_key(serial), false).await? else {
            return Ok(None);
        };
        let active = routing::decode_active(&raw)?;
        let certificate = tr
            .get(&routing::certificate_key(serial), false)
            .await?
            .ok_or(CatalogError::Incomplete)?;
        active_context(&tr, serial, active, &certificate).await?;
        if active.route.namespace != namespace {
            return Err(CatalogError::Invalid("带库已由不同 namespace 初始化"));
        }
        Ok(Some(active))
    }

    /// Publish a virgin, empty namespace atomically. Existing namespaces and
    /// active authorities are never reset. No tape or device API is called.
    pub async fn initialize_native_library(
        &self,
        namespace: Uuid,
        proof: &VerifiedNativeBarrier,
    ) -> Result<ActiveLibraryRoute> {
        if namespace.is_nil() {
            return Err(CatalogError::Invalid("namespace 不能为 nil"));
        }
        let serial = &proof.serial;
        let certificate = proof.certificate(namespace);
        let active = ActiveLibraryRoute {
            route: PreparedLibraryRoute {
                namespace,
                revision: 1,
            },
            frozen_index: proof.frozen,
            certificate_sha256: Sha256::digest(&certificate).into(),
            initial_owner_epoch: proof.frozen + 1,
        };
        self.db
            .run(|tr, _| {
                let certificate = &certificate;
                async move {
                    requests::retry_result(
                        async {
                            if let Some(raw) = tr.get(&routing::active_key(serial), false).await? {
                                let existing = routing::decode_active(&raw)?;
                                let saved = tr
                                    .get(&routing::certificate_key(serial), false)
                                    .await?
                                    .ok_or(CatalogError::Incomplete)?;
                                active_context(&tr, serial, existing, &saved).await?;
                                if existing.route.namespace != namespace
                                    || existing.frozen_index != proof.frozen
                                {
                                    return Err(CatalogError::Invalid(
                                        "带库已由不同初始化请求激活",
                                    ));
                                }
                                return Ok(existing);
                            }
                            if tr.get(&routing::route_key(serial), false).await?.is_some()
                                || tr
                                    .get(&routing::certificate_key(serial), false)
                                    .await?
                                    .is_some()
                            {
                                return Err(CatalogError::Invalid(
                                    "带库已有准备路由或激活证明，拒绝覆盖",
                                ));
                            }
                            if tr.get(&namespace_key(namespace), false).await?.is_some() {
                                return Err(CatalogError::Invalid("初始化要求未使用的 namespace"));
                            }
                            if let Some(epoch) = tr.get(&owner_key(serial), false).await? {
                                if epoch.len() != 8 {
                                    return Err(CatalogError::Corrupt("已有 owner epoch 编码无效"));
                                }
                                if read_u64(&epoch) >= active.initial_owner_epoch {
                                    return Err(CatalogError::Invalid(
                                        "冻结水位必须高于已有 owner epoch",
                                    ));
                                }
                            }
                            for &kind in cleanup::NAMESPACE_ROWS {
                                let mut prefix = key(kind);
                                prefix.extend_from_slice(namespace.as_bytes());
                                if requests::prefix_has_rows(&tr, &prefix).await? {
                                    return Err(CatalogError::Invalid(
                                        "namespace 存在未清除的目录记录",
                                    ));
                                }
                            }
                            tr.set(&node_key(namespace, Uuid::nil()), &root_value());
                            tr.set(&namespace_key(namespace), &[NamespaceState::Active as u8]);
                            tr.set(&control::managed_key(namespace), b"");
                            control::initialize_managed_layout(&tr, namespace);
                            tr.set(&control::library_key(namespace, serial), b"");
                            control::management::initialize(&tr, namespace);
                            tr.set(&source_revision_key(namespace), &0u64.to_be_bytes());
                            tr.set(
                                &requests::window_key(namespace),
                                &requests::encode_window(RequestWindow {
                                    first_retained: 1,
                                    accepting: 1,
                                }),
                            );
                            maintenance::initialize(&tr, namespace);
                            tr.set(
                                &owner_key(serial),
                                &active.initial_owner_epoch.to_be_bytes(),
                            );
                            tr.set(
                                &routing::route_key(serial),
                                &routing::encode_route(active.route),
                            );
                            tr.set(&routing::certificate_key(serial), certificate);
                            tr.set(
                                &routing::active_key(serial),
                                &routing::encode_active(active),
                            );
                            Ok(active)
                        }
                        .await,
                    )
                }
            })
            .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replies(serial: &str, challenge: Uuid) -> Vec<Value> {
        (1..=3).map(|id| json!({
            "version":1,"library_serial":serial,"challenge":challenge.to_string(),"node_id":id,
            "term":7,"leader":2,"role":if id == 2 {"Leader"} else {"Follower"},
            "members":{"1":"127.0.0.1:8001","2":"127.0.0.1:8002","3":"127.0.0.1:8003"},
            "quiesce_index":89,"frozen_index":90,"applied_index":92,
            "executor_absent":true,"catalog_pending":false,"shutting_down":false,"catalog_empty":true
        })).collect()
    }

    #[test]
    fn barriers_require_three_distinct_frozen_members() {
        let challenge = Uuid::new_v4();
        let good = replies("SIM-NATIVE", challenge);
        assert!(verify_native_barriers("SIM-NATIVE", challenge, &good).is_ok());
        assert!(verify_native_barriers("SIM-NATIVE", Uuid::nil(), &good).is_err());
        assert!(verify_native_barriers("SIM-NATIVE", challenge, &good[..2]).is_err());
        for (field, bad) in [
            ("node_id", json!(2)),
            ("challenge", json!(Uuid::new_v4().to_string())),
            ("version", json!(2)),
            ("frozen_index", json!(91)),
            ("quiesce_index", json!(0)),
            ("applied_index", json!(89)),
            ("term", json!(8)),
            ("leader", json!(3)),
            ("role", json!("Candidate")),
            ("executor_absent", json!(false)),
            ("catalog_pending", json!(true)),
            ("shutting_down", json!(true)),
            ("catalog_empty", json!(false)),
            ("members", json!({"1":"a:1","2":"a:1","3":"a:3"})),
        ] {
            let mut bad_replies = good.clone();
            bad_replies[0][field] = bad;
            assert!(
                verify_native_barriers("SIM-NATIVE", challenge, &bad_replies).is_err(),
                "{field}"
            );
        }
        for frozen in [0, MAX_EPOCH] {
            let mut bad = good.clone();
            for reply in &mut bad {
                reply["frozen_index"] = json!(frozen);
            }
            assert!(verify_native_barriers("SIM-NATIVE", challenge, &bad).is_err());
        }
    }

    #[test]
    fn barriers_cannot_be_reused_across_libraries() {
        let challenge = Uuid::new_v4();
        let good = replies("SIM-A", challenge);
        assert!(verify_native_barriers("SIM-A", challenge, &good).is_ok());
        assert!(verify_native_barriers("SIM-B", challenge, &good).is_err());
        let mut mixed = good.clone();
        mixed[1]["library_serial"] = json!("SIM-B");
        assert!(verify_native_barriers("SIM-A", challenge, &mixed).is_err());
        let mut missing = good;
        missing[0].as_object_mut().unwrap().remove("library_serial");
        assert!(verify_native_barriers("SIM-A", challenge, &missing).is_err());
    }

    #[test]
    fn native_certificate_is_canonical_and_has_explicit_provenance() {
        let challenge = Uuid::new_v4();
        let mut values = replies("SIM-NATIVE", challenge);
        let proof = verify_native_barriers("SIM-NATIVE", challenge, &values).unwrap();
        values.reverse();
        let reversed = verify_native_barriers("SIM-NATIVE", challenge, &values).unwrap();
        let namespace = Uuid::new_v4();
        let encoded = proof.certificate(namespace);
        assert_eq!(reversed.certificate(namespace), encoded);
        let (decoded, saved) = decode(&encoded).unwrap();
        assert_eq!(decoded, namespace);
        assert_eq!(saved.certificate(namespace), encoded);
        assert!(decode(&encoded[1..]).is_err());
        let mut bad = encoded.clone();
        bad.push(b' ');
        assert!(decode(&bad).is_err());
        assert!(decode(&proof.certificate(Uuid::nil())).is_err());
        assert!(decode(&vec![b'N'; LIMIT + 1]).is_err());
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；验证初始化不覆盖已有准备记录，不操作设备"]
    async fn native_init_never_overwrites_prepared_authority() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        for kind in 0..2 {
            let namespace = Uuid::new_v4();
            let serial = format!("SIM-PREPARED-{namespace}");
            let challenge = Uuid::new_v4();
            let proof =
                verify_native_barriers(&serial, challenge, &replies(&serial, challenge)).unwrap();
            let retained = if kind == 0 {
                routing::route_key(&serial)
            } else {
                routing::certificate_key(&serial)
            };
            let value = if kind == 0 {
                routing::encode_route(PreparedLibraryRoute {
                    namespace: Uuid::new_v4(),
                    revision: 1,
                })
                .to_vec()
            } else {
                b"retained-certificate".to_vec()
            };
            let tr = catalog.db.create_trx().unwrap();
            tr.set(&retained, &value);
            tr.commit().await.unwrap();
            let outcome = catalog.initialize_native_library(namespace, &proof).await;
            let tr = catalog.db.create_trx().unwrap();
            assert_eq!(
                tr.get(&retained, false).await.unwrap().unwrap().as_ref(),
                value.as_slice(),
                "初始化不得改写已有准备记录；结果 {outcome:?}"
            );
            assert!(matches!(outcome, Err(CatalogError::Invalid(_))));
            for key in [
                namespace_key(namespace),
                control::managed_key(namespace),
                control::managed_layout_key(namespace),
                node_key(namespace, Uuid::nil()),
                routing::active_key(&serial),
                owner_key(&serial),
            ] {
                assert!(tr.get(&key, false).await.unwrap().is_none());
            }
        }
    }

    #[tokio::test]
    #[ignore = "需要隔离 FDB_TEST_CLUSTER；仅元数据，不访问设备"]
    async fn native_init_is_atomic_idempotent_and_never_reuses_authority() {
        let cluster = std::env::var("FDB_TEST_CLUSTER").unwrap();
        let _network = unsafe { foundationdb::boot() };
        let catalog = ManagedCatalog::open(Some(&cluster)).unwrap();
        let namespace = Uuid::new_v4();
        let serial = format!("SIM-NATIVE-{namespace}");
        let challenge = Uuid::new_v4();
        let proof =
            verify_native_barriers(&serial, challenge, &replies(&serial, challenge)).unwrap();
        assert_eq!(
            catalog
                .native_initialization_status(&serial, namespace)
                .await
                .unwrap(),
            None
        );
        let active = catalog
            .initialize_native_library(namespace, &proof)
            .await
            .unwrap();
        assert_eq!(active.initial_owner_epoch, 91);
        assert_eq!(
            catalog.active_library_route(&serial).await.unwrap(),
            Some(active)
        );
        assert_eq!(
            catalog
                .initialize_native_library(namespace, &proof)
                .await
                .unwrap(),
            active
        );
        assert_eq!(
            catalog
                .native_initialization_status(&serial, namespace)
                .await
                .unwrap(),
            Some(active)
        );
        assert!(
            catalog
                .initialize_native_library(Uuid::new_v4(), &proof)
                .await
                .is_err()
        );
        assert!(
            catalog
                .native_initialization_status(&serial, Uuid::new_v4())
                .await
                .is_err()
        );
        assert_eq!(
            catalog
                .claim_active_owner_epoch(&serial, 100)
                .await
                .unwrap(),
            ActiveOwnerOutcome::Advanced
        );
        assert_eq!(
            catalog
                .initialize_native_library(namespace, &proof)
                .await
                .unwrap(),
            active
        );
        assert_eq!(
            catalog.active_library_owner(&serial).await.unwrap(),
            Some((active, 100))
        );

        // A damaged certificate or root must never be silently regenerated by a retry.
        let cert_key = routing::certificate_key(&serial);
        let cert = proof.certificate(namespace);
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&cert_key, &cert[1..]);
        tr.commit().await.unwrap();
        assert!(
            catalog
                .initialize_native_library(namespace, &proof)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&cert_key, &cert);
        tr.clear(&node_key(namespace, Uuid::nil()));
        tr.commit().await.unwrap();
        assert!(catalog.active_library_route(&serial).await.is_err());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&node_key(namespace, Uuid::nil()), &root_value());
        tr.commit().await.unwrap();
        assert_eq!(
            catalog.active_library_route(&serial).await.unwrap(),
            Some(active)
        );

        // Existing namespaces, orphan rows and high epochs remain untouched on failure.
        let other = Uuid::new_v4();
        let other_serial = format!("SIM-NATIVE-{other}");
        let proof =
            verify_native_barriers(&other_serial, challenge, &replies(&other_serial, challenge))
                .unwrap();
        catalog.begin_import(other).await.unwrap();
        assert!(
            catalog
                .initialize_native_library(other, &proof)
                .await
                .is_err()
        );
        let virgin = Uuid::new_v4();
        let orphan = node_key(virgin, Uuid::new_v4());
        let tr = catalog.db.create_trx().unwrap();
        tr.set(&orphan, b"orphan");
        tr.commit().await.unwrap();
        assert!(
            catalog
                .initialize_native_library(virgin, &proof)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert!(
            tr.get(&routing::active_key(&other_serial), false)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            tr.get(&namespace_key(virgin), false)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            tr.get(&orphan, false).await.unwrap().as_deref(),
            Some(b"orphan".as_slice())
        );
        tr.clear(&orphan);
        tr.set(&owner_key(&other_serial), &100u64.to_be_bytes());
        tr.commit().await.unwrap();
        assert!(
            catalog
                .initialize_native_library(virgin, &proof)
                .await
                .is_err()
        );
        let tr = catalog.db.create_trx().unwrap();
        assert_eq!(
            tr.get(&owner_key(&other_serial), false)
                .await
                .unwrap()
                .as_deref(),
            Some(100u64.to_be_bytes().as_slice())
        );
        assert!(
            tr.get(&namespace_key(virgin), false)
                .await
                .unwrap()
                .is_none()
        );

        // Two libraries cannot acquire the same fresh namespace, even concurrently.
        let shared = Uuid::new_v4();
        let a = format!("SIM-A-{shared}");
        let b = format!("SIM-B-{shared}");
        let pa = verify_native_barriers(&a, challenge, &replies(&a, challenge)).unwrap();
        let pb = verify_native_barriers(&b, challenge, &replies(&b, challenge)).unwrap();
        let (ra, rb) = tokio::join!(
            catalog.initialize_native_library(shared, &pa),
            catalog.initialize_native_library(shared, &pb)
        );
        assert_ne!(ra.is_ok(), rb.is_ok());
        let (winner, loser) = if ra.is_ok() { (&a, &b) } else { (&b, &a) };
        assert_eq!(
            catalog
                .active_library_route(winner)
                .await
                .unwrap()
                .unwrap()
                .route
                .namespace,
            shared
        );
        assert_eq!(catalog.active_library_route(loser).await.unwrap(), None);
    }
}
