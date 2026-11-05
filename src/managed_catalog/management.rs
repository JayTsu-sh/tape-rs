//! Persistent management request window initialized with a new managed namespace.
use super::*;

pub(crate) fn prefix(namespace: Uuid, kind: u8) -> Vec<u8> {
    let mut out = key(b'@');
    out.extend_from_slice(namespace.as_bytes());
    out.push(kind);
    out
}

pub(crate) fn initialize(tr: &Transaction, namespace: Uuid) {
    tr.set(
        &prefix(namespace, b'w'),
        &requests::encode_window(RequestWindow {
            first_retained: 1,
            accepting: 1,
        }),
    );
}
