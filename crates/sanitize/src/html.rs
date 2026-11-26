//! HTML 净化（SEC-002）+ 外部资源默认阻断（SEC-003）。
//!
//! 实现取舍：**不用三方净化库**。原因有二 ——
//! 1. 本机构建环境（沙箱禁 utime）无法解压新的 crate，而安全模块恰恰是最不能
//!    "下载后碰运气"的模块；
//! 2. 邮件 HTML 的子集非常窄，一个 500 行的白名单分词器比一个通用 HTML5
//!    解析器更容易审计，也更容易在平台上保持一致行为（Windows / Android 同源）。
//!
//! 安全模型（每一层都必须独立成立，不依赖上一层）：
//! * **标签白名单**：不在白名单内的标签整个丢弃；`script`/`style`/`svg`/`iframe`
//!   连同内容一起丢（它们属于 raw text / foreign content，留着内容没有意义）。
//! * **属性白名单**：按标签分别授白，`on*` 事件属性无条件拒绝。
//! * **URL 协议白名单**：属性值先做实体解码（`&#106;avascript:` 是经典绕过），
//!   再按协议判定，只放行 `http/https/mailto`（图片额外放行 `cid` 与安全
//!   `data:image/*`），其余一律丢弃。
//! * **CSS 白名单**：`style` 走 [`crate::css::sanitize_style`]，杜绝
//!   `background-image:url()` 这类无 `<img>` 追踪与 `expression()` 脚本通道。
//! * **远程资源阻断**：`img` 的远程地址替换为 1×1 透明占位图，原地址进报告。
//!
//! 幂等性：`sanitize(sanitize(x)) == sanitize(x)`，由单测保证。这是 UI 反复
//! 渲染同一封邮件时不出错的前提。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::css::sanitize_style_report;
use crate::resource::{classify, scheme_allowed, BlockedResource, ResourceKind};

/// 1×1 透明 GIF，用于替换被阻断的远程图片，保持邮件排版不被撑破。
pub const PLACEHOLDER_IMAGE: &str =
    "data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";

/// 允许保留的标签（纯排版，不含表单、框架、脚本、插件、外链样式与 SVG/MathML）。
const ALLOWED_TAGS: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "address",
    "article",
    "aside",
    "b",
    "bdi",
    "bdo",
    "big",
    "blockquote",
    "br",
    "caption",
    "center",
    "cite",
    "code",
    "col",
    "colgroup",
    "dd",
    "del",
    "details",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "figcaption",
    "figure",
    "font",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "main",
    "mark",
    "ol",
    "p",
    "pre",
    "q",
    "rp",
    "rt",
    "ruby",
    "s",
    "samp",
    "section",
    "small",
    "span",
    "strike",
    "strong",
    "sub",
    "summary",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
    "wbr",
];

/// 空元素：不需要闭合标签。
const VOID_TAGS: &[&str] = &["br", "col", "hr", "img", "wbr"];

/// 连内容一起丢弃的标签。这些标签的内容在浏览器里不是普通文本节点
/// （raw text / foreign content / 插件内容），保留内容既没有展示价值又留隐患。
const DROP_WITH_CONTENT: &[&str] = &[
    "script",
    "style",
    "svg",
    "math",
    "iframe",
    "object",
    "applet",
    "embed",
    "template",
    "noembed",
    "noframes",
    "xmp",
    "plaintext",
    "textarea",
    "select",
    "button",
];

/// 会顺带产生网络请求的属性：整条丢弃，并按资源类型记录。
const REMOTE_ATTRS: &[&str] = &[
    "srcset",
    "background",
    "poster",
    "data",
    "dynsrc",
    "lowsrc",
    "action",
    "formaction",
    "manifest",
    "ping",
];

/// 块级标签：生成纯文本回退时补换行，让 `text/plain` 视图不至于糊成一坨。
const BLOCK_TAGS: &[&str] = &[
    "br",
    "p",
    "div",
    "tr",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "blockquote",
    "table",
    "section",
    "article",
    "header",
    "pre",
    "hr",
];

/// 净化策略。默认值即「最保守」：远程资源全阻断、内嵌 data 图片放行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SanitizePolicy {
    /// 放行远程图片（单次放行 / 发件人永久放行由业务层决定）
    pub allow_remote_images: bool,
    /// 放行远程样式与字体（默认永不，即使放行图片也保持关闭）
    pub allow_remote_css: bool,
    /// 放行 `data:image/*` 内嵌图片（SVG 除外）
    pub allow_data_images: bool,
    /// 输出上限，超出截断（防「一封 200MB 的 HTML」把 UI 拖死）
    pub max_bytes: usize,
}

impl Default for SanitizePolicy {
    fn default() -> Self {
        Self {
            allow_remote_images: false,
            allow_remote_css: false,
            allow_data_images: true,
            max_bytes: 2 * 1024 * 1024,
        }
    }
}

impl SanitizePolicy {
    /// 发件人被永久放行时使用的策略：只放行图片，字体/样式仍拦。
    pub fn with_remote_images(mut self) -> Self {
        self.allow_remote_images = true;
        self
    }
}

/// 净化统计，直接作为「为什么显示成这样」的解释交给 UI / 诊断包。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SanitizeReport {
    /// 被移除的标签 → 次数
    pub removed_tags: BTreeMap<String, usize>,
    /// 被移除的属性 → 次数（`on*` 归入 `on*` 一项）
    pub removed_attributes: BTreeMap<String, usize>,
    /// 被阻断的远程资源
    pub blocked: Vec<BlockedResource>,
    /// 输出是否被截断
    pub truncated: bool,
}

impl SanitizeReport {
    /// 是否存在「有任何东西被拦」——UI 用它决定是否显示安全提示条。
    pub fn has_blocked(&self) -> bool {
        !self.blocked.is_empty()
    }

    pub fn record_tag(&mut self, tag: &str) {
        *self.removed_tags.entry(tag.to_string()).or_insert(0) += 1;
    }

    pub fn record_attr(&mut self, attr: &str) {
        *self.removed_attributes.entry(attr.to_string()).or_insert(0) += 1;
    }
}

/// 净化结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SanitizedHtml {
    /// 可安全注入 WebView 的 HTML
    pub html: String,
    /// 由净化后 HTML 派生的纯文本（正文缺失 / 无障碍朗读 / 搜索预览用）
    pub plain: String,
    pub report: SanitizeReport,
}

/// 净化入口。
pub fn sanitize(input: &str, policy: &SanitizePolicy) -> SanitizedHtml {
    let mut s = Sanitizer {
        out: String::with_capacity(input.len().min(policy.max_bytes)),
        plain: String::new(),
        report: SanitizeReport::default(),
        policy,
        stopped: false,
    };
    s.run(input);
    SanitizedHtml {
        html: s.out,
        plain: s.plain.trim().to_string(),
        report: s.report,
    }
}

/// 便捷入口：只拿 HTML。
pub fn sanitize_html(input: &str, policy: &SanitizePolicy) -> String {
    sanitize(input, policy).html
}

struct Sanitizer<'a> {
    out: String,
    plain: String,
    report: SanitizeReport,
    policy: &'a SanitizePolicy,
    stopped: bool,
}

impl Sanitizer<'_> {
    fn run(&mut self, input: &str) {
        let bytes = input.as_bytes();
        let mut i = 0usize;
        while i < bytes.len() {
            if self.stopped {
                break;
            }
            if bytes[i] == b'<' {
                if let Some(next) = self.handle_markup(input, i) {
                    i = next;
                    continue;
                }
                // 不是合法标记起始：当普通文本转义输出，避免半截标签被浏览器猜成标签
                self.push_text("&lt;");
                i += 1;
                continue;
            }
            // 普通文本：按片段推进
            let end = input[i..]
                .find('<')
                .map(|off| i + off)
                .unwrap_or(input.len());
            let chunk = &input[i..end];
            self.push_text(&strip_text_controls(chunk));
            self.push_plain(&strip_text_controls(chunk));
            i = end;
        }
        if self.out.len() >= self.policy.max_bytes {
            self.report.truncated = true;
        }
    }

    /// 处理一个标记；不是标记时返回 None（由调用方当文本处理）。
    fn handle_markup(&mut self, input: &str, i: usize) -> Option<usize> {
        let rest = &input[i..];

        // 注释 / DOCTYPE / 处理指令：整段跳过，不进入输出
        if rest.starts_with("<!--") {
            self.report.record_tag("comment");
            let end = rest.find("-->").map(|o| o + 3).unwrap_or(rest.len());
            return Some(i + end);
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            self.report.record_tag("declaration");
            let end = rest.find('>').map(|o| o + 1).unwrap_or(rest.len());
            return Some(i + end);
        }

        // 结束标签
        if let Some(after) = rest.strip_prefix("</") {
            let name_end = after
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .unwrap_or(after.len());
            if name_end == 0 {
                return None;
            }
            let name = after[..name_end].to_ascii_lowercase();
            let end = rest.find('>').map(|o| o + 1).unwrap_or(rest.len());
            if is_allowed_tag(&name) && !VOID_TAGS.contains(&name.as_str()) {
                self.push_text(&format!("</{name}>"));
            } else if !is_allowed_tag(&name) {
                self.report.record_tag(&name);
            }
            return Some(i + end);
        }

        // 起始标签：必须紧跟 ASCII 字母，`<3` 这种是正文
        let first = rest.as_bytes().get(1).copied()?;
        if !first.is_ascii_alphabetic() {
            return None;
        }
        let tag_end = scan_tag_end(rest)?;
        let (name, attrs) = parse_start_tag(&rest[..tag_end]);

        // 连内容丢弃的标签：跳到对应结束标签之后
        if DROP_WITH_CONTENT.contains(&name.as_str()) {
            self.report.record_tag(&name);
            self.record_blocked_from_attrs(&name, &attrs);
            let close = format!("</{name}");
            let lower = rest.to_ascii_lowercase();
            let end = lower
                .find(&close)
                .map(|o| o + lower[o..].find('>').map(|x| x + 1).unwrap_or(close.len()))
                .unwrap_or(rest.len());
            return Some(i + end);
        }

        if !is_allowed_tag(&name) {
            self.report.record_tag(&name);
            self.record_blocked_from_attrs(&name, &attrs);
            // 标签本身丢弃，内容继续走正常流程（多数未知标签只是排版容器）
            return Some(i + tag_end + 1);
        }

        self.emit_start_tag(&name, &attrs);
        Some(i + tag_end + 1)
    }

    fn emit_start_tag(&mut self, name: &str, attrs: &[(String, String)]) {
        let mut buf = String::with_capacity(64);
        buf.push('<');
        buf.push_str(name);
        let is_image = name == "img";
        let mut has_src = false;

        for (attr, value) in attrs {
            let key = attr.to_ascii_lowercase();
            // 事件处理器：无条件拒绝（含 onerror / onload / onbegin 等所有 on*）
            if key.starts_with("on") && key.len() > 2 {
                self.report.record_attr("on*");
                continue;
            }
            if key == "style" {
                // CSS 里的 `url()` 同样是一次回连；被 CSS 层丢弃的外链也要进报告，
                // 否则「已阻止 N 项远程资源」会漏计（用户看到的安全提示与实际不符）。
                let (clean, dropped) = sanitize_style_report(value);
                for url in dropped {
                    let host = classify(&url).host;
                    self.report.blocked.push(BlockedResource {
                        url,
                        kind: ResourceKind::Stylesheet,
                        host,
                        reason: "CSS 外链被拒绝".into(),
                    });
                }
                match clean {
                    Some(v) => {
                        buf.push_str(" style=\"");
                        buf.push_str(&escape_attr(&v));
                        buf.push('"');
                    }
                    None => self.report.record_attr("style"),
                }
                continue;
            }
            if REMOTE_ATTRS.contains(&key.as_str()) {
                self.record_blocked_url(value, kind_for_attr(name, &key));
                self.report.record_attr(&key);
                continue;
            }
            if key == "src" || key == "href" {
                match self.filter_url(name, value, key == "src") {
                    Some(v) => {
                        buf.push(' ');
                        buf.push_str(&key);
                        buf.push_str("=\"");
                        buf.push_str(&escape_attr(&v));
                        buf.push('"');
                        if key == "src" {
                            has_src = true;
                        }
                    }
                    None => self.report.record_attr(&key),
                }
                continue;
            }
            if !attr_allowed(name, &key) {
                self.report.record_attr(&key);
                continue;
            }
            buf.push(' ');
            buf.push_str(&key);
            buf.push_str("=\"");
            buf.push_str(&escape_attr(value));
            buf.push('"');
        }

        // 远程图片被拦后仍要有一个 src，否则浏览器会显示破图图标
        if is_image && !has_src {
            buf.push_str(" src=\"");
            buf.push_str(PLACEHOLDER_IMAGE);
            buf.push('"');
        }
        // 外链一律新窗口 + 断 opener，防止 `window.opener` 反向控制
        if name == "a" {
            buf.push_str(" rel=\"noopener noreferrer nofollow\" target=\"_blank\"");
        }
        buf.push('>');
        self.push_text(&buf);
        if BLOCK_TAGS.contains(&name) {
            self.push_plain("\n");
        }
    }

    /// URL 属性过滤：协议白名单 + 远程资源策略。
    ///
    /// 入参 `value` 已在 `parse_start_tag` 解过一层实体（与浏览器一致），
    /// **这里不能再解一次**——多解一层会人为造出新的绕过面。
    fn filter_url(&mut self, tag: &str, raw: &str, is_src: bool) -> Option<String> {
        let url = raw.to_string();
        let info = classify(&url);
        if !scheme_allowed(info.scheme.as_deref(), is_src) {
            if matches!(
                info.scheme.as_deref(),
                Some("javascript") | Some("vbscript")
            ) {
                self.report.blocked.push(BlockedResource {
                    url: url.clone(),
                    kind: ResourceKind::Script,
                    host: info.host.clone(),
                    reason: "脚本协议被拒绝".into(),
                });
            }
            return None;
        }
        if info.scheme.as_deref() == Some("data") {
            // `data:` 只允许"栅格图内嵌"这一种形态：
            // `data:image/svg+xml` 里可以带脚本，`data:text/html` 直接是网页，
            // 二者都必须在协议层就被拒，不能指望 `inline_image` 这个闸门
            // （它对非栅格图返回 false，若只用它做"是否阻断"的开关就会漏放）。
            if !info.inline_image {
                self.report.blocked.push(BlockedResource {
                    url: url.clone(),
                    kind: ResourceKind::Script,
                    host: None,
                    reason: "非栅格图的 data: 内嵌资源被拒绝".into(),
                });
                return None;
            }
            if !self.policy.allow_data_images {
                return None;
            }
        }
        if info.remote {
            let kind = kind_for_tag(tag);
            let allowed = match kind {
                ResourceKind::Image => self.policy.allow_remote_images,
                ResourceKind::Stylesheet | ResourceKind::Font => self.policy.allow_remote_css,
                // 脚本/框架/媒体：即使放行远程也不加载
                ResourceKind::Script | ResourceKind::Frame | ResourceKind::Media => false,
                ResourceKind::Other => !is_src,
            };
            if !allowed {
                if is_src {
                    self.report.blocked.push(BlockedResource {
                        url,
                        kind,
                        host: info.host,
                        reason: "默认阻止远程资源".into(),
                    });
                    return Some(PLACEHOLDER_IMAGE.to_string());
                }
                return None;
            }
        }
        Some(url)
    }

    fn record_blocked_from_attrs(&mut self, tag: &str, attrs: &[(String, String)]) {
        for (attr, value) in attrs {
            let key = attr.to_ascii_lowercase();
            if key == "src" || key == "href" || REMOTE_ATTRS.contains(&key.as_str()) {
                self.record_blocked_url(value, kind_for_tag(tag));
            }
        }
    }

    fn record_blocked_url(&mut self, raw: &str, kind: ResourceKind) {
        // raw 已解过实体（见 parse_start_tag），此处不再二次解码
        let url = raw.to_string();
        let info = classify(&url);
        if info.remote {
            self.report.blocked.push(BlockedResource {
                url,
                kind,
                host: info.host,
                reason: "默认阻止远程资源".into(),
            });
        }
    }

    fn push_text(&mut self, s: &str) {
        if self.stopped {
            return;
        }
        self.out.push_str(s);
        if self.out.len() >= self.policy.max_bytes {
            // 必须按字符边界截断，否则会把多字节字符劈开直接 panic
            let mut cut = self.policy.max_bytes.min(self.out.len());
            while cut > 0 && !self.out.is_char_boundary(cut) {
                cut -= 1;
            }
            self.out.truncate(cut);
            self.stopped = true;
        }
    }

    fn push_plain(&mut self, s: &str) {
        if self.plain.len() < self.policy.max_bytes {
            self.plain.push_str(s);
        }
    }
}

fn kind_for_tag(tag: &str) -> ResourceKind {
    match tag {
        "img" => ResourceKind::Image,
        "link" => ResourceKind::Stylesheet,
        "iframe" | "frame" | "frameset" => ResourceKind::Frame,
        "video" | "audio" | "source" | "track" => ResourceKind::Media,
        "script" => ResourceKind::Script,
        _ => ResourceKind::Other,
    }
}

fn kind_for_attr(tag: &str, attr: &str) -> ResourceKind {
    match attr {
        "background" | "poster" | "dynsrc" | "lowsrc" => ResourceKind::Image,
        "data" | "manifest" => ResourceKind::Other,
        _ => kind_for_tag(tag),
    }
}

fn is_allowed_tag(name: &str) -> bool {
    ALLOWED_TAGS.contains(&name)
}

/// 按标签授白的普通属性（URL 属性、`style`、`on*` 已在调用方单独处理）。
fn attr_allowed(tag: &str, attr: &str) -> bool {
    // 全局排版属性
    if matches!(attr, "title" | "dir" | "lang") {
        return true;
    }
    match tag {
        "a" => matches!(attr, "name"),
        "img" => matches!(
            attr,
            "alt" | "width" | "height" | "border" | "hspace" | "vspace" | "align"
        ),
        "table" => matches!(
            attr,
            "width"
                | "height"
                | "border"
                | "cellpadding"
                | "cellspacing"
                | "bgcolor"
                | "align"
                | "summary"
        ),
        "td" | "th" => matches!(
            attr,
            "colspan" | "rowspan" | "width" | "height" | "bgcolor" | "align" | "valign" | "nowrap"
        ),
        "col" | "colgroup" => matches!(attr, "span" | "width"),
        "ol" => matches!(attr, "start" | "type"),
        "ul" => matches!(attr, "type"),
        "li" => matches!(attr, "value" | "type"),
        "font" => matches!(attr, "color" | "size" | "face"),
        "hr" => matches!(attr, "width" | "size" | "color" | "noshade" | "align"),
        "p" | "div" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => matches!(attr, "align"),
        "time" => matches!(attr, "datetime"),
        "blockquote" | "q" | "del" | "ins" => matches!(attr, "cite" | "datetime"),
        _ => matches!(
            attr,
            "align" | "valign" | "width" | "height" | "color" | "size"
        ),
    }
}

/// 返回 `>` 的下标（相对 `s`）；引号内的 `>` 不算结束。没有任何 `>` 时返回 None。
fn scan_tag_end(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    for (i, c) in s.char_indices() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' => quote = Some(c),
                '>' => return Some(i),
                // 标签里不该再出现 `<`：说明是畸形输入，交由调用方当文本处理
                '<' if i > 0 => return None,
                _ => {}
            },
        }
    }
    None
}

/// 解析 `<name attr=val ...>`（不含最后一个 `>`）。
fn parse_start_tag(s: &str) -> (String, Vec<(String, String)>) {
    let body = s.trim_start_matches('<');
    let name_end = body
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(body.len());
    let name = body[..name_end].to_ascii_lowercase();
    let mut rest = &body[name_end..];

    let mut attrs = Vec::new();
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        if rest.is_empty() {
            break;
        }
        let name_len = rest
            .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        if name_len == 0 {
            // 吃掉无法识别的字符，避免死循环
            rest = &rest[1..];
            continue;
        }
        let attr_name = rest[..name_len].to_string();
        rest = &rest[name_len..];
        rest = rest.trim_start_matches(|c: char| c.is_whitespace());
        let mut value = String::new();
        if let Some(after_eq) = rest.strip_prefix('=') {
            let after_eq = after_eq.trim_start_matches(|c: char| c.is_whitespace());
            if let Some(q) = after_eq.chars().next().filter(|c| *c == '"' || *c == '\'') {
                let inner = &after_eq[q.len_utf8()..];
                let end = inner.find(q).unwrap_or(inner.len());
                value = inner[..end].to_string();
                rest = inner[end..].strip_prefix(q).unwrap_or("");
            } else {
                let end = after_eq
                    .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                    .unwrap_or(after_eq.len());
                value = after_eq[..end].to_string();
                rest = &after_eq[end..];
            }
        }
        // 属性值在这里**统一解一次实体**，与浏览器解析行为一致：
        // 1) 幂等：输出时 `escape_attr` 会把 `&` 写成 `&amp;`，若解析时不还原，
        //    每渲染一次就多套一层 `&amp;`，属性值会被越转越乱（title/alt 最明显）；
        // 2) 判定：`href="&#106;avascript:"` 这类必须按解码后的结果判协议。
        let value = decode_entities(&value);
        attrs.push((attr_name, value));
    }
    (name, attrs)
}

/// 最小实体解码：只处理会改变安全判定的部分（数值实体 + 常见命名实体）。
///
/// 只解一层——浏览器也只解一层，多解一层反而会造出新的绕过面。
pub fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let bytes: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i];
        if c != '&' {
            out.push(c);
            i += 1;
            continue;
        }
        // 找分号，最多向后 10 个字符（实体名长度有限）
        let limit = (i + 11).min(bytes.len());
        match bytes[i..limit].iter().position(|c| *c == ';') {
            Some(rel) => {
                let entity: String = bytes[i + 1..i + rel].iter().collect();
                let decoded = decode_one(&entity);
                match decoded {
                    Some(ch) => {
                        out.push(ch);
                        i = i + rel + 1;
                    }
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

fn decode_one(entity: &str) -> Option<char> {
    if let Some(num) = entity.strip_prefix('#') {
        let (radix, digits) = match num.strip_prefix(['x', 'X']) {
            Some(hex) => (16, hex),
            None => (10, num),
        };
        if digits.is_empty() {
            return None;
        }
        let code = u32::from_str_radix(digits, radix).ok()?;
        return char::from_u32(code).or(Some('\u{FFFD}'));
    }
    // 只覆盖会影响判定的几个；其余原样保留（重新转义后不影响安全性）
    Some(match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{00A0}',
        "Tab" => '\t',
        "NewLine" => '\n',
        _ => return None,
    })
}

fn strip_text_controls(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || matches!(c, '\t' | '\n' | '\r'))
        .collect()
}

fn escape_attr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sani(input: &str) -> SanitizedHtml {
        sanitize(input, &SanitizePolicy::default())
    }

    #[test]
    fn strips_script_tags_and_content() {
        let r = sani("<p>hi</p><script>alert('xss')</script><p>bye</p>");
        assert!(!r.html.contains("script"));
        assert!(!r.html.contains("alert"));
        assert_eq!(r.html, "<p>hi</p><p>bye</p>");
        assert_eq!(r.report.removed_tags.get("script"), Some(&1));
    }

    #[test]
    fn strips_event_handlers() {
        let r = sani(r#"<img src="cid:a@b" onerror="alert(1)">"#);
        assert!(!r.html.contains("onerror"));
        assert!(r.html.contains(r#"src="cid:a@b""#));
        assert_eq!(r.report.removed_attributes.get("on*"), Some(&1));
    }

    #[test]
    fn blocks_javascript_url_including_entity_obfuscation() {
        for payload in [
            r#"<a href="javascript:alert(1)">x</a>"#,
            r#"<a href="java&#115;cript:alert(1)">x</a>"#,
            r#"<a href="JaVaScRiPt:alert(1)">x</a>"#,
            r#"<a href="java&#x09;script:alert(1)">x</a>"#,
            r#"<a href=" javascript:alert(1)">x</a>"#,
            "<a href=\"\u{0}javascript:alert(1)\">x</a>",
        ] {
            let r = sani(payload);
            assert!(
                !r.html.contains("javascript"),
                "未拦住: {payload} -> {}",
                r.html
            );
            assert!(
                !r.html.contains("JaVaScRiPt"),
                "未拦住: {payload} -> {}",
                r.html
            );
        }
    }

    #[test]
    fn allows_http_and_mailto_links_with_rel() {
        let r = sani(r#"<a href="https://example.com/a?b=1&amp;c=2">x</a>"#);
        assert!(r
            .html
            .contains(r#"href="https://example.com/a?b=1&amp;c=2""#));
        assert!(r.html.contains("noopener"));
        let r = sani(r#"<a href="mailto:a@b.com">m</a>"#);
        assert!(r.html.contains("mailto:a@b.com"));
    }

    #[test]
    fn blocks_remote_images_by_default() {
        let r = sani(r#"<img src="https://tracker.example.com/p.gif" width="1">"#);
        assert!(r.html.contains(PLACEHOLDER_IMAGE));
        assert!(!r.html.contains("tracker.example.com"));
        assert!(r.html.contains(r#"width="1""#));
        let b = &r.report.blocked;
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].kind, ResourceKind::Image);
        assert_eq!(b[0].host.as_deref(), Some("tracker.example.com"));
    }

    #[test]
    fn allows_remote_images_when_policy_permits() {
        let policy = SanitizePolicy::default().with_remote_images();
        let r = sanitize(r#"<img src="https://cdn.example.com/a.png">"#, &policy);
        assert!(r.html.contains("https://cdn.example.com/a.png"));
        assert!(r.report.blocked.is_empty());
    }

    #[test]
    fn strips_style_attribute_tracking_pixel() {
        let r = sani(
            r#"<div style="background-image:url(https://t.example.com/x.gif);color:red">a</div>"#,
        );
        assert!(!r.html.contains("url("));
        assert!(r.html.contains("color: red"));
    }

    #[test]
    fn drops_style_blocks_and_link_stylesheets() {
        let r = sani(
            r#"<style>body{background:url(//t.example.com/a.gif)}</style><link rel="stylesheet" href="https://t.example.com/a.css">x"#,
        );
        assert!(!r.html.contains("background"));
        assert!(!r.html.contains("stylesheet"));
        assert_eq!(r.html, "x");
        assert_eq!(r.report.blocked.len(), 1);
        assert_eq!(r.report.blocked[0].kind, ResourceKind::Stylesheet);
    }

    #[test]
    fn drops_iframe_svg_and_object_with_content() {
        for (payload, needle) in [
            (
                r#"<iframe src="https://evil.example.com"></iframe>"#,
                "iframe",
            ),
            (r#"<svg><animate onbegin="alert(1)"/></svg>"#, "onbegin"),
            (
                r#"<object data="https://evil.example.com/x.swf"></object>"#,
                "object",
            ),
            (
                r#"<template><img src=x onerror=alert(1)></template>"#,
                "onerror",
            ),
        ] {
            let r = sani(payload);
            assert!(!r.html.contains(needle), "未拦住: {payload} -> {}", r.html);
        }
    }

    #[test]
    fn unclosed_tags_become_text() {
        let r = sani("a < b and 3 < 4");
        assert!(r.html.contains("&lt; b"));
        let r = sani("<div");
        assert!(!r.html.contains("<div"));
    }

    #[test]
    fn keeps_tables_and_layout() {
        let html = r#"<table width="600" cellpadding="0"><tr><td align="center" colspan="2">x</td></tr></table>"#;
        let r = sani(html);
        assert_eq!(r.html, html);
    }

    #[test]
    fn report_counts_and_flag() {
        let r = sani(
            r#"<script>1</script><img src="https://a.example.com/x.gif"><a href="javascript:1">y</a>"#,
        );
        assert!(r.report.has_blocked());
        assert_eq!(r.report.removed_tags.get("script"), Some(&1));
        // 远程图片（Image）+ javascript: 链接（Script）各算一条
        assert_eq!(r.report.blocked.len(), 2);
        assert_eq!(r.report.removed_attributes.get("href"), Some(&1));
    }

    #[test]
    fn is_idempotent() {
        let messy = r#"<div style="color:red;background:url(//t.example.com/a)"><img src="//t.example.com/p.gif" onload="x()"><a href="vbscript:x">l</a><script>y</script>text &amp; more</div>"#;
        let once = sani(messy);
        let twice = sani(&once.html);
        // HTML 幂等是硬要求（UI 每次重渲染都会走一遍净化）；
        // report.blocked 只反映**首次**拦截——第二遍看到的是占位图，本来就没有远程资源了。
        assert_eq!(once.html, twice.html);
        assert_eq!(
            once.report.blocked.len(),
            3,
            "首次应拦住：远程图片 + vbscript 链接 + CSS background url()"
        );
        assert!(twice.report.blocked.is_empty());
    }

    #[test]
    fn rejects_non_raster_data_uri() {
        // `data:image/svg+xml` 可以携带脚本，`data:text/html` 直接是网页：
        // 只有栅格图形态才允许内嵌，且受 allow_data_images 约束。
        let r = sani(r#"<img src="data:image/svg+xml;base64,PHN2Zz48c2NyaXB0Lz48L3N2Zz4=">"#);
        assert!(!r.html.contains("svg"), "{}", r.html);
        assert_eq!(r.report.blocked.len(), 1, "应记为一次拦截");

        let r = sani(r#"<img src="data:text/html,<script>alert(1)</script>">"#);
        assert!(!r.html.contains("text/html"), "{}", r.html);

        // 栅格图仍然放行（邮件里的内嵌 logo 不能丢）
        let ok = sani("data:image/png;base64,iVBORw0KGgo=");
        assert!(ok.html.contains("data:image/png"), "{}", ok.html);
    }

    #[test]
    fn non_url_attribute_values_are_not_double_escaped() {
        // 回归：属性值解析时不还原实体 → 输出时 `&` 被转义成 `&amp;`，
        // 再渲染一次又套一层，title/alt 会被越转越乱（且不幂等）。
        let src = r#"<p title="a &amp; b &lt;c&gt;">x</p>"#;
        let once = sani(src);
        assert!(
            once.html.contains(r#"title="a &amp; b &lt;c&gt;""#),
            "{}",
            once.html
        );
        let twice = sani(&once.html);
        assert_eq!(once.html, twice.html, "属性值转义必须幂等");
    }

    #[test]
    fn css_url_in_denied_property_is_also_counted() {
        // `background-image` 不在 CSS 白名单（allowlist 里是 `background`），
        // 但它同样能打追踪像素：被丢弃时也必须计入拦截报告。
        let r = sani(r#"<div style="background-image:url(//t.example.com/x.gif)">x</div>"#);
        assert_eq!(r.report.blocked.len(), 1, "{:?}", r.report.blocked);
        assert_eq!(r.report.blocked[0].host.as_deref(), Some("t.example.com"));
    }

    #[test]
    fn css_remote_url_is_counted_in_report() {
        // 被 CSS 层丢弃的外链也必须进报告，否则「已阻止 N 项」漏计
        let r = sani(r#"<div style="background:url(//t.example.com/a.gif)">x</div>"#);
        assert_eq!(r.report.blocked.len(), 1);
        assert_eq!(r.report.blocked[0].host.as_deref(), Some("t.example.com"));
        assert_eq!(
            r.report.removed_attributes.get("style"),
            Some(&1),
            "整条 style 被丢弃时应计入 removed_attributes"
        );
    }

    #[test]
    fn plain_text_derivation() {
        let r = sani("<p>line1</p><p>line2</p><br>line3");
        assert_eq!(r.plain.replace("\n\n", "\n"), "line1\nline2\nline3");
    }

    #[test]
    fn truncates_pathological_input() {
        let policy = SanitizePolicy {
            max_bytes: 64,
            ..SanitizePolicy::default()
        };
        let r = sanitize(&"<p>x</p>".repeat(1000), &policy);
        assert!(r.report.truncated);
        assert!(r.html.len() <= 64);
    }

    #[test]
    fn decodes_entities_once() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&#106;"), "j");
        assert_eq!(decode_entities("&#x6A;"), "j");
        // 双层实体只解一层，与浏览器一致
        assert_eq!(decode_entities("&amp;#106;"), "&#106;");
        assert_eq!(decode_entities("a&unknown;b"), "a&unknown;b");
    }

    #[test]
    fn content_editable_and_forms_are_dropped() {
        let r = sani(
            r#"<form action="https://evil.example.com"><input name="p"><button>go</button></form>"#,
        );
        assert!(!r.html.contains("form"));
        assert!(!r.html.contains("input"));
        assert!(!r.html.contains("button"));
    }
}
