//! Secret 检测与脱敏（§18.5：脱敏是统一的数据管线，而不是单独保护几个文件名）。
//!
//! 任何进入模型上下文的内联文本先经 [`SecretScanner`]：
//! - 命中已知密钥形态 → 以 `[REDACTED:<类型>]` 替换，条目追加
//!   [`TransformationKind::Redact`] 变换记录（§8.4.8）；
//! - 密文出现在 Shell 输出、Git Diff、工具结果、用户消息都一样处理。
//!
//! 这是内容级防线，与 `Classification::Secret` 的条目级拦截（§8.4.6，
//! 由 SnapshotBuilder 执行）互补。检测是启发式的，只覆盖常见形态；
//! 新形态在后续版本补充，误杀（redact 正常文本）比漏报安全（§19 安全默认值）。

/// 单条脱敏记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    /// 命中的密钥形态名，如 `openai_key`。
    pub kind: &'static str,
    /// 替换后的占位符。
    pub placeholder: String,
}

/// 扫描结果：脱敏后的文本与记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    pub text: String,
    pub redactions: Vec<Redaction>,
    /// 是否发生了替换。
    pub redacted: bool,
}

#[derive(Debug, Clone, Copy)]
struct Pattern {
    kind: &'static str,
    /// 正则替代：受限的手写匹配器（避免引入 regex 依赖）。
    matcher: fn(&str) -> Option<(usize, usize)>,
}

/// 已知密钥形态（§18.5：Shell 输出 / Diff / 日志 / 工具结果都可能出现）。
static PATTERNS: &[Pattern] = &[
    Pattern {
        kind: "openai_key",
        matcher: find_openai_key,
    },
    Pattern {
        kind: "github_token",
        matcher: find_github_token,
    },
    Pattern {
        kind: "aws_access_key",
        matcher: find_aws_key,
    },
    Pattern {
        kind: "private_key_block",
        matcher: find_private_key,
    },
    Pattern {
        kind: "bearer_token",
        matcher: find_bearer,
    },
];

impl SecretScanner {
    pub fn scan(&self, text: &str) -> ScanOutcome {
        let mut out = text.to_string();
        let mut redactions = Vec::new();
        // 逐个 pattern 全量替换；_private_key 块优先级由遍历顺序决定，
        // 多 pattern 命中同一片段时后替换者找不到原文自然跳过。
        for pattern in PATTERNS {
            let mut replaced_any = false;
            while let Some((start, end)) = (pattern.matcher)(&out) {
                let placeholder = format!("[REDACTED:{}]", pattern.kind);
                out.replace_range(start..end, &placeholder);
                replaced_any = true;
            }
            if replaced_any {
                redactions.push(Redaction {
                    kind: pattern.kind,
                    placeholder: format!("[REDACTED:{}]", pattern.kind),
                });
            }
        }
        ScanOutcome {
            redacted: !redactions.is_empty(),
            redactions,
            text: out,
        }
    }
}

/// Secret 扫描器（无状态，可静态复用）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SecretScanner;

fn is_secret_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// `sk-` 前缀 + ≥16 个密钥字符。
fn find_openai_key(text: &str) -> Option<(usize, usize)> {
    let mut search_from = 0;
    while let Some(pos) = text[search_from..].find("sk-") {
        let start = search_from + pos;
        let rest = &text[start + 3..];
        let len = rest.chars().take_while(|c| is_secret_char(*c)).count();
        if len >= 16 {
            return Some((start, start + 3 + len));
        }
        search_from = start + 3;
    }
    None
}

/// `gh[pousr]_` 前缀 + ≥20 个密钥字符。
fn find_github_token(text: &str) -> Option<(usize, usize)> {
    for prefix in ["ghp_", "gho_", "ghu_", "ghs_", "ghr_"] {
        if let Some(pos) = text.find(prefix) {
            let rest = &text[pos + prefix.len()..];
            let len = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .count();
            if len >= 20 {
                return Some((pos, pos + prefix.len() + len));
            }
        }
    }
    None
}

/// `AKIA` + 16 个大写字母数字。
fn find_aws_key(text: &str) -> Option<(usize, usize)> {
    if let Some(pos) = text.find("AKIA") {
        let rest = &text[pos + 4..];
        let len = rest
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            .count();
        if len >= 16 {
            return Some((pos, pos + 4 + len));
        }
    }
    None
}

/// PEM 私钥块：从 BEGIN 行到 END 行整体删除。
fn find_private_key(text: &str) -> Option<(usize, usize)> {
    let start = text.find("-----BEGIN")?;
    let kind_end = text[start..].find("PRIVATE KEY-----")?;
    let body_start = start + kind_end + "PRIVATE KEY-----".len();
    let end_marker = "-----END";
    let end = text[body_start..].find(end_marker)?;
    let line_end = text[body_start + end..]
        .find('\n')
        .map(|i| body_start + end + i + 1);
    Some((start, line_end.unwrap_or(text.len())))
}

/// `Bearer <token>`（≥16 字符）。
fn find_bearer(text: &str) -> Option<(usize, usize)> {
    let mut search_from = 0;
    while let Some(pos) = text[search_from..].find("Bearer ") {
        let start = search_from + pos;
        let rest = &text[start + 7..];
        let len = rest
            .chars()
            .take_while(|c| is_secret_char(*c) || *c == '.')
            .count();
        if len >= 16 {
            return Some((start, start + 7 + len));
        }
        search_from = start + 7;
    }
    None
}

/// 扫描并（如有命中）脱敏；返回 `(处理后文本, 是否发生脱敏)`。
pub fn redact_text(text: &str) -> (String, bool) {
    let outcome = SecretScanner.scan(text);
    (outcome.text, outcome.redacted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_secret_shapes() {
        let (text, redacted) =
            redact_text("config: api_key=sk-abcdefgh123456789012345 end\nnormal line");
        assert!(redacted);
        assert!(text.contains("[REDACTED:openai_key]"));
        assert!(!text.contains("sk-abcdefgh"));

        let (text, _) = redact_text("token: ghp_abcdefghijklmnopqrstuvwxyz123456");
        assert!(text.contains("[REDACTED:github_token]"));

        let (text, _) = redact_text("AKIAIOSFODNN7EXAMPLE is aws");
        assert!(text.contains("[REDACTED:aws_access_key]"));

        let (text, _) = redact_text("Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9");
        assert!(text.contains("[REDACTED:bearer_token]"));

        let (text, _) = redact_text(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIEow...\n-----END RSA PRIVATE KEY-----\nkeep me",
        );
        assert!(!text.contains("MIIEow"));
        assert!(text.contains("keep me"), "块外内容保留");
    }

    #[test]
    fn short_lookalikes_are_ignored() {
        // 普通代码里的 "task-"、"sk" 缩写不应误杀
        let (text, redacted) = redact_text("let sk-total = compute(); // task-1");
        assert!(!redacted);
        assert_eq!(text, "let sk-total = compute(); // task-1");
    }

    #[test]
    fn scan_reports_redaction_kinds() {
        let outcome = SecretScanner.scan("key = sk-abcdefghijklmnop12345678");
        assert!(outcome.redacted);
        assert_eq!(outcome.redactions.len(), 1);
        assert_eq!(outcome.redactions[0].kind, "openai_key");
    }
}
