//! MAM evidence is useful for accounting only when the particular sample is
//! bound to the exact verified media head. A later index_state is insufficient.
use super::*;
use serde_json::{Value, json};

fn stamp_key(ns: Uuid, volume: Uuid) -> Vec<u8> {
    let mut key = management::prefix(ns, b'g');
    key.push(b'm');
    key.extend_from_slice(volume.as_bytes());
    key
}

/// Prepare before the monitor transaction's first mutation. Index-only
/// observations never refresh a previous sample's identity or timestamp.
pub(super) async fn sample_mutation(
    tr: &Transaction,
    record: &tape_monitor::TapeMonitorRecord,
    sampled: bool,
) -> Result<Option<(Vec<u8>, Option<Vec<u8>>)>> {
    if !sampled {
        return Ok(None);
    }
    let key = stamp_key(record.namespace, record.volume);
    let Some(sample) = record
        .sample
        .as_ref()
        .filter(|s| s.data.is_some() && s.errors.is_empty())
    else {
        return Ok(Some((key, None)));
    };
    if record.import_required || record.requires_validation || record.verified_generation.is_none()
    {
        return Ok(Some((key, None)));
    }
    let (_, head) = active_volume::observed_at(tr, &record.library, record.volume).await?;
    if record.verified_generation != Some(head.generation) {
        return Ok(Some((key, None)));
    }
    let mut index = Vec::new();
    active_volume::encode_index(&mut index, head);
    let value = json!({"version":1,"namespace":record.namespace.to_string(),"library":record.library,"barcode":record.barcode,"volume":record.volume.to_string(),"observed_at":sample.observed_at,"index":index});
    Ok(Some((key, Some(value.to_string().into_bytes()))))
}

pub(super) async fn at(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
    barcode: &str,
    volume: Uuid,
) -> Result<Option<Value>> {
    let Some(record) = tape_monitor::read_at(tr, ns, serial, barcode).await? else {
        return Ok(None);
    };
    if record.volume != volume || record.import_required || record.requires_validation {
        return Ok(None);
    }
    let Some(sample) = record
        .sample
        .filter(|s| s.data.is_some() && s.errors.is_empty())
    else {
        return Ok(None);
    };
    let Some(raw) = tr.get(&stamp_key(ns, volume), false).await? else {
        return Ok(None);
    };
    let value: Value =
        serde_json::from_slice(&raw).map_err(|_| CatalogError::Corrupt("MAM采样卷头证明JSON"))?;
    let fields = [
        "version",
        "namespace",
        "library",
        "barcode",
        "volume",
        "observed_at",
        "index",
    ];
    if raw.len() > 4096
        || value.to_string().as_bytes() != raw.as_ref()
        || value["version"] != 1
        || !value.as_object().is_some_and(|m| {
            m.len() == fields.len() && m.keys().all(|k| fields.contains(&k.as_str()))
        })
        || value["namespace"] != ns.to_string()
        || value["library"] != serial
        || value["barcode"] != barcode
        || value["volume"] != volume.to_string()
    {
        return Err(CatalogError::Corrupt("MAM采样卷头证明身份"));
    }
    let bytes: Vec<u8> = serde_json::from_value(value["index"].clone())
        .map_err(|_| CatalogError::Corrupt("MAM采样索引编码"))?;
    let stamped = active_volume::decode_index(volume, &bytes)?;
    let (_, head) = active_volume::observed_at(tr, serial, volume).await?;
    if stamped != head
        || record.verified_generation != Some(head.generation)
        || value["observed_at"] != sample.observed_at
    {
        return Ok(None);
    }
    let capacity = sample.data.expect("filtered capacity");
    Ok(Some(
        json!({"volume":volume.to_string(),"generation":head.generation,"observed_at":sample.observed_at,"maximum":capacity.maximum,"remaining":capacity.remaining,"resolution_bytes":1048576}),
    ))
}

impl ManagedCatalog {
    /// No mounting or probing: return only a previously captured, still-current
    /// native MAM observation. Missing or stale evidence remains unknown.
    pub async fn active_tape_capacity_evidence(
        &self,
        serial: &str,
        route: ActiveLibraryRoute,
        barcode: &str,
    ) -> Result<Option<Value>> {
        let tr = self.db.create_trx()?;
        let ns = routing::require_expected_active_namespace(&tr, serial, Some(route)).await?;
        let Some(assignment) = Self::verified_assignment(&tr, serial, barcode)
            .await?
            .filter(|a| a.namespace == ns)
        else {
            return Ok(None);
        };
        let Some(volume) = assignment.volume_id else {
            return Ok(None);
        };
        at(&tr, ns, serial, barcode, volume).await
    }
}
