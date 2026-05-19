//! 人类可读的通用格式化 helper（被 changer / catalog 多个子模块共用）。

/// 把字节数格式化成 1024 进制单位字符串，例如 `3221225472` → `"3.0G"`。
pub fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T", "P"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{}", bytes)
    } else {
        format!("{:.1}{}", v, UNITS[i])
    }
}
