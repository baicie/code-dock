//! 时间约定：统一使用 UTC RFC3339（§8.1）。

use chrono::{DateTime, SecondsFormat, Utc};

/// 当前 UTC 时间的 RFC3339 字符串，毫秒精度（如 `2026-09-04T05:31:28.421Z`）。
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 按 RFC3339 毫秒精度格式化时间戳。
pub fn format_rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_uses_utc_z_suffix() {
        let t = DateTime::parse_from_rfc3339("2026-09-04T05:31:28.421Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(format_rfc3339(t), "2026-09-04T05:31:28.421Z");
    }
}
