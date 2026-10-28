//! 库级选带的纯策略。REPORT DENSITY SUPPORT 布局依据 IBM GA32-0928-07 §5.2.26。
use std::collections::BTreeMap;

/// 密度代码存在表示可读；WRTOK 单独决定可写。截断响应不能变成兼容性证据。
pub(super) fn densities(raw: &[u8]) -> Option<BTreeMap<u8, bool>> {
    if raw.len() < 4 {
        return None;
    }
    let end = usize::from(u16::from_be_bytes([raw[0], raw[1]])).checked_add(2)?;
    if end < 4 || end > raw.len() {
        return None;
    }
    let mut out = BTreeMap::new();
    let mut offset = 4;
    while offset < end {
        let d = raw.get(offset..offset + 52)?;
        let size = if d[2] & 1 != 0 {
            5 + usize::from(u16::from_be_bytes([d[3], d[4]]))
        } else {
            52
        };
        if size < 52 || offset.checked_add(size)? > end {
            return None;
        }
        let writable = d[2] & 0x80 != 0;
        for code in [d[0], d[1]] {
            out.entry(code)
                .and_modify(|w| *w |= writable)
                .or_insert(writable);
        }
        offset += size;
    }
    Some(out)
}

pub(super) fn barcode_density(barcode: &str) -> Option<u8> {
    match barcode.get(barcode.len().checked_sub(2)?..)? {
        "L5" => Some(0x58),
        "L6" => Some(0x5a),
        "L7" => Some(0x5c),
        "M8" => Some(0x5d),
        "L8" => Some(0x5e),
        "L9" => Some(0x60),
        "LA" => Some(0x62),
        _ => None,
    }
}

pub(super) fn compatible(densities: &BTreeMap<u8, bool>, barcode: &str, write: bool) -> bool {
    barcode_density(barcode)
        .is_some_and(|density| densities.get(&density).is_some_and(|w| !write || *w))
}

/// 已用物理容量比例降序；活文件长度不能代表重写和旧索引实际占用的带空间。
pub(super) fn fill_order(x: Option<(u64, u64)>, y: Option<(u64, u64)>) -> std::cmp::Ordering {
    match (x, y) {
        (Some((xu, xt)), Some((yu, yt))) => {
            (u128::from(yu) * u128::from(xt)).cmp(&(u128::from(xu) * u128::from(yt)))
        }
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// 仅真实、完整的 MAM 采样能形成预算；未知绝不能伪造为零使用量或超大容量。
pub(super) fn write_budget(capacity: &serde_json::Value, index_entries: u64) -> Option<(u64, u64)> {
    let total = capacity["maximum"].as_u64()?;
    let remaining = capacity["remaining"].as_u64()?;
    if total == 0 || remaining > total {
        return None;
    }
    let ip = &capacity["index_capacity"];
    let index_total = ip["maximum"].as_u64()?;
    let index_free = ip["remaining"].as_u64()?;
    if index_total == 0 || index_free > index_total {
        return None;
    }
    let reserve = (total / 50)
        .max(index_entries.saturating_mul(4400))
        .saturating_add((total / 20).min(1 << 30));
    Some((
        total.saturating_sub(reserve),
        if index_free <= index_entries.saturating_mul(2200) {
            0
        } else {
            remaining.saturating_sub(reserve)
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fill_ranking_uses_physical_fraction_and_handles_large_values() {
        assert!(fill_order(Some((9, 10)), Some((80, 100))).is_lt());
        assert!(fill_order(Some((1, 10)), Some((80, 100))).is_gt());
        assert!(fill_order(Some((u64::MAX - 1, u64::MAX)), Some((1, 2))).is_lt());
        assert!(fill_order(None, Some((0, 100))).is_gt());
    }

    #[test]
    fn density_response_validates_lengths_and_write_permission() {
        let mut raw = vec![0; 108];
        raw[..2].copy_from_slice(&106u16.to_be_bytes());
        raw[4] = 0x5e;
        raw[5] = 0x5e;
        raw[6] = 0x80;
        raw[56] = 0x5c;
        raw[57] = 0x5c;
        let d = densities(&raw).unwrap();
        assert!(compatible(&d, "ABC001L8", true));
        assert!(compatible(&d, "ABC001L7", false));
        assert!(!compatible(&d, "ABC001L7", true));
        assert!(!compatible(&d, "ABC001L9", false));
        for n in 0..raw.len() {
            assert!(densities(&raw[..n]).is_none());
        }
        raw[6] |= 1;
        raw[7..9].copy_from_slice(&48u16.to_be_bytes());
        assert!(densities(&raw).is_none());
    }
    #[test]
    fn unknown_capacity_has_no_budget() {
        for v in [
            serde_json::Value::Null,
            serde_json::json!({"maximum":100}),
            serde_json::json!({"maximum":0,"remaining":0}),
            serde_json::json!({"maximum":2,"remaining":3}),
        ] {
            assert_eq!(write_budget(&v, 1), None);
        }
        assert_eq!(
            write_budget(
                &serde_json::json!({"maximum":1000000,"remaining":1000000,"index_capacity":{"maximum":100000,"remaining":100000}}),
                1
            ),
            Some((930000, 930000))
        );
        assert_eq!(
            write_budget(
                &serde_json::json!({"maximum":1000000,"remaining":1,"index_capacity":{"maximum":100000,"remaining":100000}}),
                1
            ),
            Some((930000, 0))
        );
    }
}
