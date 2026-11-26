//! 脱敏（OBS-003）：诊断包要能被用户放心地发给支持人员。
//!
//! 只做**降低信息密度**，不做「看起来安全」的假动作：
//! * 邮箱保留域名（定位认证/同步问题必须知道是哪家服务器），本地部分只留前两位；
//! * 长随机串（token / base64 / 附件 blob id）整段替换；
//! * 敏感键名（password / token / secret …）的值一律替换；
//! * 邮件正文不导出——诊断包只放状态与日志，不放用户内容。

use serde_json::{Map, Value};

/// 敏感键名（小写包含即命中）
const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "credential",
    "authorization",
    "cookie",
    "api_key",
    "apikey",
    "private_key",
    "refresh",
    "access_token",
    "auth",
];

/// 邮箱脱敏：`zhangsan@example.com` → `zh***@example.com`
pub fn redact_email(email: &str) -> String {
    match email.split_once('@') {
        Some((local, domain)) if !domain.is_empty() => {
            let keep: String = local.chars().take(2).collect();
            let masked = if local.chars().count() <= 2 {
                "***".to_string()
            } else {
                format!("{keep}***")
            };
            format!("{masked}@{domain}")
        }
        _ => "***".to_string(),
    }
}

fn looks_like_email_token(t: &str) -> bool {
    match t.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty() && domain.contains('.') && !domain.starts_with('.')
        }
        None => false,
    }
}

fn looks_like_secret(t: &str) -> bool {
    t.len() >= 24
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '-' | '_' | '.'))
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().any(|c| c.is_ascii_alphabetic())
}

fn redact_token(t: &str) -> String {
    if looks_like_email_token(t) {
        return redact_email(t);
    }
    if looks_like_secret(t) {
        return format!("***({}字节已脱敏)", t.len());
    }
    t.to_string()
}

fn is_separator(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            ',' | ';'
                | '"'
                | '\''
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '='
                | ':'
                | '|'
        )
}

/// 文本脱敏：按分隔符切 token，命中邮箱/长密钥就替换，其余原样保留。
pub fn redact_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut token = String::new();
    for c in s.chars() {
        if is_separator(c) {
            if !token.is_empty() {
                out.push_str(&redact_token(&token));
                token.clear();
            }
            out.push(c);
        } else {
            token.push(c);
        }
    }
    if !token.is_empty() {
        out.push_str(&redact_token(&token));
    }
    out
}

fn key_is_sensitive(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_KEYS.iter().any(|k| lower.contains(k))
}

/// JSON 脱敏：递归处理字符串值，敏感键的值整体替换。
pub fn redact_json(value: &Value) -> Value {
    match value {
        Value::String(s) => Value::String(redact_text(s)),
        Value::Array(items) => Value::Array(items.iter().map(redact_json).collect()),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                if key_is_sensitive(k) {
                    out.insert(k.clone(), Value::String("***".into()));
                } else {
                    out.insert(k.clone(), redact_json(v));
                }
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_local_part_but_keeps_domain() {
        assert_eq!(redact_email("zhangsan@example.com"), "zh***@example.com");
        assert_eq!(redact_email("ab@example.com"), "***@example.com");
        assert_eq!(redact_email("not-an-email"), "***");
    }

    #[test]
    fn redacts_emails_inside_sentences() {
        let s = redact_text("同步失败：user=wangwu@corp.cn, mailbox=INBOX");
        assert!(s.contains("wa***@corp.cn"), "{s}");
        assert!(!s.contains("wangwu"));
        assert!(s.contains("INBOX"));
    }

    #[test]
    fn redacts_long_secrets() {
        let s = redact_text("Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9abc123");
        assert!(
            !s.contains("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9abc123"),
            "{s}"
        );
        assert!(s.contains("已脱敏"));
    }

    #[test]
    fn keeps_short_normal_words() {
        assert_eq!(
            redact_text("network timeout after 30s"),
            "network timeout after 30s"
        );
    }

    #[test]
    fn redacts_sensitive_json_keys_recursively() {
        let v = json!({
            "account": {"address": "lisi@example.com", "password": "hunter2"},
            "queue": [{"kind": "flag", "token": "abcdef"}],
            "note": "联系 lisi@example.com",
        });
        let r = redact_json(&v);
        assert_eq!(r["account"]["password"], "***");
        assert_eq!(r["queue"][0]["token"], "***");
        assert_eq!(r["account"]["address"], "li***@example.com");
        assert!(r["note"].as_str().unwrap().contains("li***@example.com"));
    }
}
