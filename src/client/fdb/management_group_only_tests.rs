use super::*;
fn request() -> Value {
    json!({"version":1,"library":"MODE-SIM","namespace":Uuid::new_v4().to_string(),"epoch":1,"operation":Uuid::new_v4().to_string(),"change":{"kind":"media-reclaim","pool":Uuid::new_v4().to_string(),"barcode":"TEST00L8","volume":Uuid::new_v4().to_string(),"revision":1,"confirm_format":true,"unassign":false}})
}
#[test]
fn group_only_saved_management_request_round_trips_without_default_mode() {
    let old = request();
    let bytes = old.to_string();
    let old = PreparedManagement::parse(old).unwrap();
    assert_eq!(old.value().to_string(), bytes);
    let mut explicit = old.value().clone();
    explicit["change"]["mode"] = json!("group-only");
    let original = PreparedManagement::parse(explicit.clone()).unwrap();
    let root = std::env::temp_dir().join(format!("group-only-mode-{}", Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("request.json");
    original.save_new(&path).unwrap();
    assert_eq!(PreparedManagement::load(&path).unwrap().value(), &explicit);
    assert!(old.save_new(&path).is_err());
    std::fs::remove_dir_all(root).unwrap();
}
#[test]
fn group_only_client_mode_is_strict() {
    for mode in [
        Value::Null,
        json!(false),
        json!(true),
        json!(1),
        json!("standard"),
        json!("group_only"),
    ] {
        let mut value = request();
        value["change"]["mode"] = mode;
        assert!(PreparedManagement::parse(value).is_err());
    }
}
