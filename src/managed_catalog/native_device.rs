//! Read shared maintenance exclusion before creating new pool metadata.
use super::*;

fn domain(ns: Uuid, kind: u8) -> Vec<u8> {
    let mut out = management::prefix(ns, b'K');
    out.push(kind);
    out
}

pub(super) fn library_gate(ns: Uuid, serial: &str) -> Vec<u8> {
    let mut out = domain(ns, b'l');
    append_component(&mut out, serial.as_bytes());
    out
}

pub(super) async fn require_library_available(
    tr: &Transaction,
    ns: Uuid,
    serial: &str,
) -> Result<()> {
    if tr.get(&library_gate(ns, serial), false).await?.is_some() {
        return Err(CatalogError::PoolBusy);
    }
    Ok(())
}
