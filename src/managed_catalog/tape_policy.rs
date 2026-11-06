//! Logical tape attributes are distinct from drive-observed restrictions.
use super::*;
use serde_json::{Value, json};

fn key_for(a: &TapeAssignment) -> Vec<u8> {
    let mut key = management::prefix(a.namespace, b'Y');
    append_component(&mut key, a.library_serial.as_bytes());
    append_component(&mut key, a.barcode.as_bytes());
    key.extend_from_slice(a.volume_id.unwrap_or(Uuid::nil()).as_bytes());
    key
}

pub(super) fn valid_message(value: &Value) -> bool {
    value.as_str().is_some_and(|s| {
        !s.is_empty()
            && s.len() <= 63
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_- ".contains(&b))
    })
}

pub(super) async fn read_at(tr: &Transaction, a: &TapeAssignment) -> Result<Value> {
    let Some(raw) = tr.get(&key_for(a), false).await? else {
        return Ok(json!({"offline_message":null}));
    };
    let value: Value = serde_json::from_slice(&raw)
        .map_err(|_| CatalogError::Corrupt("磁带逻辑属性 JSON 损坏"))?;
    if !value
        .as_object()
        .is_some_and(|v| v.len() == 1 && v.contains_key("offline_message"))
        || !(value["offline_message"].is_null() || valid_message(&value["offline_message"]))
    {
        return Err(CatalogError::Corrupt("磁带逻辑属性字段损坏"));
    }
    Ok(value)
}

pub(super) async fn view_at(tr: &Transaction, a: &TapeAssignment) -> Result<Value> {
    let mut value = a.to_json();
    for (k, v) in read_at(tr, a)
        .await?
        .as_object()
        .expect("validated attributes")
    {
        value[k] = v.clone();
    }
    Ok(value)
}

pub(super) fn write_at(tr: &Transaction, a: &TapeAssignment, value: &Value) {
    tr.set(&key_for(a), value.to_string().as_bytes());
}

pub(super) fn clear_at(tr: &Transaction, a: &TapeAssignment) {
    tr.clear(&key_for(a));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn offline_message_matches_ee_character_and_length_limits() {
        for text in ["20261104", "maintenance_1 - drive", &"a".repeat(63)] {
            assert!(valid_message(&json!(text)));
        }
        for value in [
            json!(""),
            json!("a".repeat(64)),
            json!("换带"),
            json!("x\n"),
            json!(1),
        ] {
            assert!(!valid_message(&value));
        }
    }
}
