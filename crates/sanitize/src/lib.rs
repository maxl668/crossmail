//! CrossMail 安全渲染层：HTML 净化（SEC-002）、外部资源控制（SEC-003）、
//! 发件人钓鱼判定（SEC-004）。
//!
//! 本 crate **无网络、无文件 IO、无第三方依赖**，是一组纯函数：
//! 输入原始邮件 HTML / 发件人，输出可以安全交给 WebView 的内容与一份
//! 可解释的报告。这样 UI 层（Windows / Android）能拿到完全一致的安全行为，
//! 也方便单独做 fuzz 与回归。

pub mod css;
pub mod html;
pub mod phishing;
pub mod resource;

pub use css::{is_allowed_property, sanitize_style, sanitize_style_report};
pub use html::{
    decode_entities, sanitize, sanitize_html, SanitizePolicy, SanitizeReport, SanitizedHtml,
    PLACEHOLDER_IMAGE,
};
pub use phishing::{analyze_sender, registrable_domain, SenderTrust, TrustLevel, TrustWarning};
pub use resource::{classify, scheme_allowed, BlockedResource, ResourceKind, UrlInfo};
