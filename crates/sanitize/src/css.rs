//! CSS 白名单过滤（SEC-002 / SEC-003）。
//!
//! 邮件里的 `style` 属性和 `<style>` 块是最容易被忽略的 XSS 与追踪入口：
//! `background-image:url()` 可以在不显示任何 `<img>` 的情况下回连追踪服务器，
//! `expression()` / `-moz-binding` 是老 IE/Gecko 的脚本执行通道。策略是
//! **按属性白名单 + 值内危险子串拒绝**，而不是黑名单打补丁。

/// 允许的 CSS 属性（都是纯排版，不产生网络请求、不影响层叠定位）。
const ALLOWED_PROPERTIES: &[&str] = &[
    "color",
    "background-color",
    "background",
    "font",
    "font-family",
    "font-size",
    "font-style",
    "font-weight",
    "font-variant",
    "letter-spacing",
    "line-height",
    "text-align",
    "text-decoration",
    "text-decoration-color",
    "text-indent",
    "text-transform",
    "vertical-align",
    "white-space",
    "word-break",
    "word-wrap",
    "overflow-wrap",
    "table-layout",
    "border-collapse",
    "border-spacing",
    "width",
    "height",
    "max-width",
    "min-width",
    "margin",
    "margin-top",
    "margin-right",
    "margin-bottom",
    "margin-left",
    "padding",
    "padding-top",
    "padding-right",
    "padding-bottom",
    "padding-left",
    "border",
    "border-top",
    "border-right",
    "border-bottom",
    "border-left",
    "border-color",
    "border-style",
    "border-width",
    "border-radius",
    "display",
    "list-style-type",
    "direction",
    "opacity",
];

/// 值里出现即整条声明丢弃的危险子串（小写比较）。
const DENIED_FRAGMENTS: &[&str] = &[
    "url(",
    "expression",
    "javascript:",
    "vbscript:",
    "behavior",
    "-moz-binding",
    "\\", // CSS 转义是绕过分词器做关键字混淆的常用手段，邮件里没有正当用途
    "@import",
    "<",
    ">",
    "&",  // 防止 `&amp;#1` 一类实体二次解码
    "/*", // 注释拼接
    "</",
];

/// 过滤单条 `style` 属性值；全部声明被丢弃时返回 None。
///
/// 返回的是重建后的声明串（`a: b; c: d`），不带原值的引号，避免属性逃逸。
pub fn sanitize_style(value: &str) -> Option<String> {
    sanitize_style_report(value).0
}

/// 同上，但**附带被丢弃的外链地址**。
///
/// 为什么需要：`background:url(//tracker/x.gif)` 是被 CSS 丢弃的，而不是被
/// URL 属性过滤丢弃的。若只返回 `Option<String>`，业务层的「已阻止 N 项」会漏计，
/// 用户看到的拦截数量与实际发生的回连尝试不一致 —— 安全提示的可信度就没了。
pub fn sanitize_style_report(value: &str) -> (Option<String>, Vec<String>) {
    let mut kept: Vec<String> = Vec::new();
    let mut dropped_urls: Vec<String> = Vec::new();
    for decl in split_declarations(value) {
        let Some((prop, val)) = decl.split_once(':') else {
            continue;
        };
        let prop = prop.trim().to_ascii_lowercase();
        let val = val.trim();
        let keep = !prop.is_empty()
            && ALLOWED_PROPERTIES.contains(&prop.as_str())
            && !val.is_empty()
            && value_is_safe(val);
        if keep {
            kept.push(format!("{prop}: {val}"));
        } else {
            // 无论是因为「属性不在白名单」还是「值里有危险子串」被丢弃，
            // 只要这条声明里带着外链，就是一次被拦下的回连，必须照实计数
            // （`background-image` 不在白名单，但它同样能打追踪像素）。
            dropped_urls.extend(extract_urls(val));
        }
    }
    let out = if kept.is_empty() {
        None
    } else {
        Some(kept.join("; "))
    };
    (out, dropped_urls)
}

/// 从值里抠出所有 `url(...)` 的地址（去引号）。只用于「告诉用户拦了什么」，
/// 抠漏不影响安全（安全由 `value_is_safe` 判定，与本函数无关）。
fn extract_urls(val: &str) -> Vec<String> {
    let lower = val.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while let Some(rel) = lower[i..].find("url(") {
        let start = i + rel + 4;
        let Some(close) = lower[start..].find(')') else {
            break;
        };
        let raw = val[start..start + close].trim();
        let cleaned = raw
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| raw.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(raw)
            .trim();
        if !cleaned.is_empty() {
            out.push(cleaned.to_string());
        }
        i = start + close;
        if i >= bytes.len() {
            break;
        }
    }
    out
}

/// 该属性名是否在白名单内（供外部做统计/日志）。
pub fn is_allowed_property(prop: &str) -> bool {
    ALLOWED_PROPERTIES.contains(&prop.trim().to_ascii_lowercase().as_str())
}

fn value_is_safe(val: &str) -> bool {
    let lower = val.to_ascii_lowercase();
    if lower.len() > 512 {
        return false;
    }
    if lower.chars().any(|c| c.is_control() && c != '\t') {
        return false;
    }
    !DENIED_FRAGMENTS.iter().any(|f| lower.contains(f))
}

/// 按分号切分声明，但跳过括号与引号内部的分号（`font-family: "a;b"`）。
fn split_declarations(value: &str) -> Vec<&str> {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    let mut start = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'"' | b'\'' => quote = Some(b),
                b'(' => depth += 1,
                b')' => depth -= 1,
                b';' if depth <= 0 => {
                    out.push(&value[start..i]);
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    if start < value.len() {
        out.push(&value[start..]);
    }
    out
}

/// 供 `<style>` 块降级使用：整块样式只保留能安全内联的声明（当前一律丢弃，返回 None）。
///
/// 保留这个入口是为了让调用方有明确语义——`<style>` 块我们不尝试解析，
/// 直接整块丢弃，避免为「邮件里的 CSS 选择器引擎」买单。
pub fn sanitize_stylesheet(_css: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_layout_properties() {
        let s = sanitize_style("color: #333; font-size: 14px; text-align:center").unwrap();
        assert_eq!(s, "color: #333; font-size: 14px; text-align: center");
    }

    #[test]
    fn drops_tracking_background_url() {
        assert!(sanitize_style("background-image: url(https://t.example.com/p.gif)").is_none());
        assert!(sanitize_style("background: url('//t.example.com/x') no-repeat").is_none());
    }

    #[test]
    fn drops_script_channels() {
        assert!(sanitize_style("width: expression(alert(1))").is_none());
        assert!(sanitize_style("-moz-binding: url(#x)").is_none());
        assert!(sanitize_style("behavior: url(x.htc)").is_none());
        assert!(sanitize_style("color: javascript:alert(1)").is_none());
    }

    #[test]
    fn drops_css_escape_obfuscation() {
        // `\75 rl(` 会被浏览器还原成 url( —— 反斜杠一律拒
        assert!(sanitize_style("background: \\75 rl(https://t.example.com)").is_none());
    }

    #[test]
    fn drops_positioning_that_enables_overlay_attacks() {
        assert!(sanitize_style("position: fixed; top: 0; left: 0").is_none());
    }

    #[test]
    fn keeps_valid_declaration_next_to_bad_one() {
        let s = sanitize_style("color: red; background-image: url(x)").unwrap();
        assert_eq!(s, "color: red");
    }

    #[test]
    fn handles_semicolon_inside_quotes() {
        let s = sanitize_style("font-family: \"a;b\"; color: blue").unwrap();
        assert_eq!(s, "font-family: \"a;b\"; color: blue");
    }

    #[test]
    fn rejects_entity_smuggling() {
        assert!(sanitize_style("color: &amp;#1;").is_none());
    }
}
