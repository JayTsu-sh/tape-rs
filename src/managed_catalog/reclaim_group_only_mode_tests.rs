#[test]
fn explicit_group_only_mode_preserves_old_canonical_bytes_and_changes_identity() {
    let old = request();
    let bytes = old.to_string();
    let standard = ReclaimSpec::parse(&old).unwrap();
    assert!(!standard.group_only());
    assert_eq!(standard.to_json().to_string(), bytes);
    let mut explicit = old.clone();
    explicit["mode"] = json!("group-only");
    let group = ReclaimSpec::parse(&explicit).unwrap();
    assert!(group.group_only());
    assert_eq!(group.to_json(), explicit);
    let base = json!({"version":1,"library":"MODE-SIM","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":old});
    let ordinary = ManagementRequest::parse(base.clone()).unwrap();
    let mut changed = base;
    changed["change"] = explicit;
    let changed = ManagementRequest::parse(changed).unwrap();
    assert_ne!(ordinary.fingerprint(), changed.fingerprint());
    assert_eq!(ordinary.operation(), changed.operation());
}
#[test]
fn explicit_group_only_mode_rejects_unknown_null_and_default_aliases() {
    for mode in [
        Value::Null,
        json!(false),
        json!(true),
        json!(1),
        json!("standard"),
        json!("group_only"),
        json!(""),
        json!({}),
    ] {
        let mut value = request();
        value["mode"] = mode;
        assert!(ReclaimSpec::parse(&value).is_err());
    }
    let mut value = request();
    value["mode"] = json!("group-only");
    value["extra"] = json!(true);
    assert!(ReclaimSpec::parse(&value).is_err());
}
