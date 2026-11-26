//! 资源分类：判断一个 URL 是「本地可渲染」还是「远程需放行」（SEC-003）。
//!
//! 这里刻意不做完整 URL 解析——邮件里的 URL 十有八九是畸形或半畸形的，
//! 用严格解析器会拒掉大量正常邮件。只做安全判定需要的三件事：
//! 1. 协议（scheme）是什么；
//! 2. 会不会触发网络请求（远程）；
//! 3. 属于哪类资源（决定默认放行策略）。

use serde::{Deserialize, Serialize};

/// 资源类型，决定默认阻断与否。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Image,
    Stylesheet,
    Font,
    Media,
    Frame,
    Script,
    Other,
}

impl ResourceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ResourceKind::Image => "image",
            ResourceKind::Stylesheet => "stylesheet",
            ResourceKind::Font => "font",
            ResourceKind::Media => "media",
            ResourceKind::Frame => "frame",
            ResourceKind::Script => "script",
            ResourceKind::Other => "other",
        }
    }

    /// 该类型资源在「默认阻止远程资源」策略下是否必须拦截。
    /// 图片/样式/字体/媒体/框架全部拦；脚本永远拦（即使放行远程也不执行）。
    pub fn is_remote_by_default_blocked(self) -> bool {
        !matches!(self, ResourceKind::Other)
    }
}

/// 被拦截的远程资源，交给 UI 展示「已阻止 N 项远程内容」并提供一键放行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedResource {
    /// 原始（已解码实体、已去控制字符）URL
    pub url: String,
    pub kind: ResourceKind,
    /// 宿主域名；IP 字面量或解析不出时为 None
    pub host: Option<String>,
    /// 拦截原因，直接给日志与诊断包看
    pub reason: String,
}

/// URL 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlInfo {
    /// 小写协议名；无协议（相对路径）时为 None
    pub scheme: Option<String>,
    pub host: Option<String>,
    /// 是否会产生网络请求
    pub remote: bool,
    /// data: URI 的内嵌负载是否为允许的图片子类型
    pub inline_image: bool,
}

/// 剥掉 URL 中的控制字符与空白：`java\nscript:` 这类绕过全靠它们。
pub fn strip_controls(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control() && !c.is_whitespace())
        .collect()
}

/// 解析协议名（小写）。`//host/x` 视为协议相对，返回 None 且按远程处理。
fn split_scheme(s: &str) -> Option<(&str, &str)> {
    let idx = s.find(':')?;
    let (head, rest) = s.split_at(idx);
    // 协议名只能是 ALPHA *( ALPHA / DIGIT / "+" / "-" / "." )
    if head.is_empty() || !head.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    if !head
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return None;
    }
    Some((head, &rest[1..]))
}

/// 从 `//host/path` 或 `scheme://host/path` 中取 host（小写、去端口、去 userinfo）。
pub fn extract_host(rest: &str) -> Option<String> {
    let after_slashes = rest.strip_prefix("//")?;
    let authority = after_slashes
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(stripped) = authority.strip_prefix('[') {
        // IPv6 字面量：[::1]:8080
        stripped.split(']').next().unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or_default()
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// data: URI 是否为安全的内嵌图片（拒绝 image/svg+xml，SVG 可带脚本）。
pub fn is_inline_image_data_uri(payload: &str) -> bool {
    let lower = payload.to_ascii_lowercase();
    let meta = lower.split(',').next().unwrap_or_default();
    if !meta.starts_with("data:image/") {
        return false;
    }
    ["png", "jpeg", "jpg", "gif", "webp", "bmp"]
        .iter()
        .any(|t| meta.starts_with(&format!("data:image/{t}")))
}

/// 归一化并判定 URL。调用方必须先做实体解码再调用。
pub fn classify(raw: &str) -> UrlInfo {
    let cleaned = strip_controls(raw);
    if cleaned.is_empty() {
        return UrlInfo {
            scheme: None,
            host: None,
            remote: false,
            inline_image: false,
        };
    }
    // 协议相对：//cdn.example.com/a.png —— 浏览器会补上当前页协议，必属远程
    if let Some(rest) = cleaned.strip_prefix("//") {
        return UrlInfo {
            scheme: None,
            host: extract_host(&format!("//{rest}")),
            remote: true,
            inline_image: false,
        };
    }
    match split_scheme(&cleaned) {
        Some((scheme, rest)) => {
            let scheme = scheme.to_ascii_lowercase();
            let host = if rest.starts_with("//") {
                extract_host(rest)
            } else {
                None
            };
            let inline_image = scheme == "data" && is_inline_image_data_uri(&cleaned);
            let remote = matches!(
                scheme.as_str(),
                "http" | "https" | "ftp" | "ftps" | "ws" | "wss"
            );
            UrlInfo {
                scheme: Some(scheme),
                host,
                remote,
                inline_image,
            }
        }
        // 无协议：相对路径 / 本地锚点，不产生外网请求
        None => UrlInfo {
            scheme: None,
            host: None,
            remote: false,
            inline_image: false,
        },
    }
}

/// 该协议是否允许出现在 `href` / `src` 上（白名单，不是黑名单）。
pub fn scheme_allowed(scheme: Option<&str>, for_image: bool) -> bool {
    match scheme {
        // 相对路径 / 锚点
        None => true,
        Some("http") | Some("https") => true,
        Some("mailto") => !for_image,
        // cid 是 MIME 内嵌引用，data 是内嵌负载（子类型另校验），二者只对图片有意义
        Some("cid") | Some("data") => for_image,
        // javascript / vbscript / file / jar / blob / about ... 一律拒绝
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_remote_and_inline() {
        let r = classify("https://img.example.com/a.png");
        assert!(r.remote);
        assert_eq!(r.host.as_deref(), Some("img.example.com"));

        let r = classify("//tracker.example.com/p.gif");
        assert!(r.remote, "协议相对 URL 必须按远程处理");
        assert_eq!(r.host.as_deref(), Some("tracker.example.com"));

        let r = classify("cid:logo@example.com");
        assert!(!r.remote);
        assert_eq!(r.scheme.as_deref(), Some("cid"));

        let r = classify("data:image/png;base64,iVBORw0KGgo=");
        assert!(r.inline_image);
        assert!(!r.remote);
    }

    #[test]
    fn rejects_svg_data_uri() {
        assert!(!is_inline_image_data_uri(
            "data:image/svg+xml,%3Csvg%20onload=alert(1)%3E"
        ));
        assert!(is_inline_image_data_uri("data:image/gif;base64,R0lGOD"));
    }

    #[test]
    fn strips_control_chars_used_for_bypass() {
        assert_eq!(
            strip_controls("java\n\tscript:alert(1)"),
            "javascript:alert(1)"
        );
        let r = classify(" javas\u{0}cript:alert(1)");
        assert_eq!(r.scheme.as_deref(), Some("javascript"));
        assert!(!scheme_allowed(r.scheme.as_deref(), false));
    }

    #[test]
    fn host_parsing_handles_port_userinfo_and_ipv6() {
        assert_eq!(
            extract_host("//user:pw@Example.COM:8443/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(extract_host("//[::1]:993/x").as_deref(), Some("::1"));
        assert_eq!(
            extract_host("//example.com./x").as_deref(),
            Some("example.com")
        );
    }
}
