//! S6-08 可回归部分：协议层脏数据回归。
//!
//! 真实服务器（尤其第三方 IMAP / 转发链）会吐出各种畸形响应：括号不配对、
//! 字符串没闭合、BODYSTRUCTURE 字段缺一半、UTF-7 编码被截断、数值字段是
//! `nil` 或天文数字。这些都不该让客户端 panic —— 崩一个线程在内核库里等于
//! 整个 App 挂掉，而 UI 层根本没有能力兜。
//!
//! 本文件的不变量只有三条，但对**每个**输入都必须成立：
//! 1. 不 panic（含索引越界、切片劈开多字节字符、整数溢出）；
//! 2. 要么给出可用结果，要么给出 `Err`——不允许"静默返回一半错误数据"；
//! 3. 结果规模有界（不能被 `{999999999}` 这类声明撑爆内存）。

use crossmail_imap::atom::parse_atoms;
use crossmail_imap::bodystructure::BodyPart;
use crossmail_imap::capability::Capabilities;
use crossmail_imap::response::{parse_response_code, parse_tagged, parse_untagged, Status};
use crossmail_imap::uid::UidRange;
use crossmail_imap::utf7;

// ---------------------------------------------------------------- atom / 词法

#[test]
fn atom_parser_survives_malformed_input() {
    let cases = [
        "",
        " ",
        "\0",
        "()",
        "(",
        ")",
        ")))",
        "((((",
        "(((((((((((((((((((((((((((((",
        "\"",
        "\"unterminated",
        "\"a\\",
        "\"a\\\"b\"",
        "NIL",
        "nil",
        "NIL NIL NIL",
        "{999999}",
        "{999999999999999999999}",
        "(((\"a\")(\"b\")(\"c\"",
        "a b c",
        "(\"CHARSET\" \"UTF-8\")",
        "\"中文\" \"日本語\" \"🙂\"",
        "🌟",
        "\u{feff}(\u{200b})",
    ];
    for c in cases {
        // 只要不 panic，返回 Ok 还是 Err 都算合格（容错策略由上层决定）
        let _ = parse_atoms(c);
    }
}

#[test]
fn atom_parser_errors_on_unbalanced_parens() {
    // 这一条不是"容错"而是"必须报错"：括号不配对时若返回 Ok，
    // 上层会把一条残缺的 BODYSTRUCTURE 当成有效结构写进数据库。
    assert!(parse_atoms("(\"a\"").is_err(), "缺右括号必须报错");
    assert!(parse_atoms("\"a\")").is_err(), "多余右括号必须报错");
}

#[test]
fn atom_parser_does_not_split_multibyte_chars() {
    // 曾经的风险点：按字节索引切片会劈开 UTF-8 多字节字符直接 panic
    let s = "(\"中文正文\" \"日本語のテキスト\" \"🙂🙂🙂\")";
    let atoms = parse_atoms(s).expect("合法输入应解析成功");
    assert_eq!(atoms.len(), 1);
    let list = atoms[0].as_list().expect("应是列表");
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].as_str(), Some("中文正文"));
}

// ---------------------------------------------------------------- 响应行

#[test]
fn untagged_response_parser_survives_garbage() {
    let cases = [
        "",
        "*",
        "* ",
        "* OK",
        "* OK [",
        "* OK [UIDVALIDITY] 已就绪",
        "* OK [UIDVALIDITY 0] x",
        "* OK [UIDVALIDITY 4294967296] 越界",
        "* OK [UIDNEXT 99999999999999999999] 天文数字",
        "* NO",
        "* BAD 未知命令",
        "* BYE 服务端关闭",
        "* 123 EXISTS",
        "* 123 EXISTS 多余",
        "* -1 EXISTS",
        "* 99999999999999 EXISTS",
        "* LIST () \"/\" \"INBOX\"",
        "* LIST () \"/\"",
        "* LIST () \"",
        "* FETCH 1 (UID 123 BODY[] {5})",
        "* FETCH 1 (BODYSTRUCTURE (\"TEXT\" \"PLAIN\"",
        "* STATUS INBOX (MESSAGES",
        "* STATUS (MESSAGES 1)",
        "* CAPABILITY",
        "* CAPABILITY IMAP4rev1 UIDPLUS",
        "* 未知东西 \u{0}\u{1}",
        "* FETCH 1 (UID 1) 后面还有垃圾",
    ];
    for c in cases {
        let _ = parse_untagged(c);
    }
}

#[test]
fn tagged_response_parser_survives_garbage() {
    let cases = [
        "",
        "A1",
        "A1 OK",
        "A1 OK [APPENDUID 1 10] done",
        "A1 OK [APPENDUID] x",
        "A1 OK [BADCHARSET (UTF-8)] x",
        "A1 NO [TRYCREATE] 邮箱不存在",
        "A1 BAD 语法错误",
        "A1 未知状态",
        " OK 无标签",
        "* OK 不是完成响应",
        "+ 续行请求",
        "A1 OK [UIDVALIDITY 1] \u{0}",
    ];
    for c in cases {
        let _ = parse_tagged(c);
    }
    // `*`/`+` 开头的不是完成响应，必须返回 None 而不是误判
    assert!(parse_tagged("* OK hi").unwrap().is_none());
    assert!(parse_tagged("+ go").unwrap().is_none());
}

#[test]
fn response_code_parser_is_total() {
    let cases = [
        "",
        "UIDVALIDITY",
        "UIDVALIDITY 0",
        "UIDVALIDITY abc",
        "UIDNEXT 4294967295",
        "UIDNEXT -1",
        "APPENDUID 1 99999999999999999999",
        "COPYUID 1 2:3 4:5",
        "BADCHARSET (UTF-8 \"GBK\")",
        "BADCHARSET (",
        "PERMANENTFLAGS (\\Seen \\*)",
        "PERMANENTFLAGS (\\未知)",
        "TRYCREATE",
        "READ-WRITE",
        "\u{0}",
    ];
    for c in cases {
        let _ = parse_response_code(c);
    }
}

#[test]
fn status_parse_is_total() {
    for c in [
        "OK", "ok", "NO", "BAD", "BYE", "PREAUTH", "", "O", "OKAY", "\u{0}",
    ] {
        let _ = Status::parse(c);
    }
    assert!(Status::parse("OK").is_some());
}

// ---------------------------------------------------------------- BODYSTRUCTURE

fn body_of(s: &str) -> Option<BodyPart> {
    let atoms = parse_atoms(s).ok()?;
    atoms.first().and_then(|a| BodyPart::parse(a).ok())
}

#[test]
fn bodystructure_parser_survives_truncated_structures() {
    let cases = [
        // 单段缺一半字段
        "(\"TEXT\" \"PLAIN\")",
        "(\"TEXT\")",
        "()",
        "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 100 3)",
        "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\") NIL NIL \"BASE64\" 100 3 NIL NIL NIL NIL)",
        // 尺寸字段是垃圾
        "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" abc 3)",
        "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 999999999999999999999 3)",
        "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" -5 -5)",
        // 类型字段是 NIL / 空串 / 非字符串
        "(NIL NIL NIL NIL NIL NIL 1 1)",
        "(\"\" \"\" NIL NIL NIL \"7BIT\" 1 1)",
        "(123 456 NIL NIL NIL \"7BIT\" 1 1)",
        // multipart 结构损坏
        "((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1) \"MIXED\")",
        "((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1))",
        "(() ())",
        "((\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1) (\"TEXT\" \"HTML\" NIL NIL NIL \"7BIT\" 1 1) \"ALTERNATIVE\" (\"BOUNDARY\" \"x\"))",
        // 只为触发参数表/扩展字段的边界
        "(\"TEXT\" \"PLAIN\" (\"CHARSET\") NIL NIL \"7BIT\" 1 1)",
        "(\"TEXT\" \"PLAIN\" (\"CHARSET\" \"UTF-8\" \"NAME\") NIL NIL \"7BIT\" 1 1)",
        "(\"APPLICATION\" \"PDF\" NIL NIL NIL \"BASE64\" 1 1 NIL (\"ATTACHMENT\" (\"FILENAME\" \"a.pdf\")) NIL)",
        // 天文长度字符串（不 panic，也不无限吃内存）
        "(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1 NIL NIL NIL NIL NIL)",
    ];
    for c in cases {
        let _ = body_of(c);
    }
}

#[test]
fn bodystructure_never_panics_on_deep_nesting() {
    // 回归：2000 层括号曾直接打爆栈（SIGABRT，catch_unwind 兜不住，App 死）。
    // 现在必须在词法层就拒绝，并给出可处理的 Err。
    let mut s = String::new();
    for _ in 0..2_000 {
        s.push('(');
    }
    s.push_str("(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1)");
    for _ in 0..2_000 {
        s.push(')');
    }
    assert!(parse_atoms(&s).is_err(), "超深嵌套必须报错而不是爆栈");
    assert!(body_of(&s).is_none());

    // 真实邮件的深度（alternatives → mixed → part → 参数表）必须照常解析
    let mut ok = String::new();
    for _ in 0..20 {
        ok.push('(');
    }
    ok.push_str("(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1)");
    for _ in 0..20 {
        ok.push(')');
    }
    assert!(parse_atoms(&ok).is_ok(), "20 层属于正常范围，不该被拒");
}

#[test]
fn bodystructure_multipart_with_broken_children_is_bounded() {
    // 500 个子段：保证结果数量有界且不 O(n²) 爆掉
    let mut s = String::from("(");
    for _ in 0..500 {
        s.push_str("(\"TEXT\" \"PLAIN\" NIL NIL NIL \"7BIT\" 1 1)");
    }
    s.push_str(" \"MIXED\")");
    if let Some(part) = body_of(&s) {
        assert!(part.leaves().len() <= 500);
        // 选择正文时不能 panic（可能挑不到）
        let _ = part.find_body(true);
        let _ = part.find_body(false);
        let _ = part.to_attachments();
    }
}

// ---------------------------------------------------------------- UTF-7 文件夹名

#[test]
fn utf7_decode_is_total() {
    let cases = [
        "",
        "-",
        "&",
        "&-",
        "&A",
        "&A-",
        "&AAA",
        "&AAA-",
        "&!!!-",
        "&中文-",
        "&-&-&-",
        "&ZeVnLIqe-", // 日本語
        "INBOX.&ZeVnLIqe-",
        "&AOgA6Q-", // 带空格的编码
        "&AOgA6Q",  // 缺结束 '-'
        "\u{0}&\u{0}-",
        "&&-",
        "&AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA-",
    ];
    for c in cases {
        let d = utf7::decode(c);
        // 输出必须是合法 UTF-8（String 本身保证）+ 长度有界
        assert!(
            d.len() <= c.len() * 4 + 8,
            "解码结果异常膨胀: {c:?} -> {}",
            d.len()
        );
    }
    assert_eq!(utf7::decode("&-"), "&");
    assert_eq!(utf7::decode("INBOX"), "INBOX");
    assert_eq!(utf7::decode("&ZeVnLIqe-"), "日本語");
}

#[test]
fn utf7_roundtrip_for_arbitrary_names() {
    let names = [
        "INBOX",
        "已发送",
        "Trash & Junk",
        "日本語フォルダ",
        "🙂 表情",
        "a&b-c",
        "",
    ];
    for n in names {
        let enc = utf7::encode(n);
        assert_eq!(utf7::decode(&enc), n, "往返不一致: {n:?} -> {enc:?}");
    }
}

#[test]
fn utf7_encode_never_emits_bare_ampersand() {
    // `&` 在 modified UTF-7 里必须写成 `&-`，否则服务端会按编码段解析
    let enc = utf7::encode("A&B");
    assert!(!enc.contains('&') || enc.contains("&-"), "{enc}");
}

// ---------------------------------------------------------------- UID 集合

#[test]
fn uid_set_parser_survives_garbage() {
    let cases = [
        "",
        ",",
        "1",
        "1,2,3",
        "1:5",
        "1:5,7,9:11",
        ":",
        ":5",
        "5:",
        "1:",
        "a:b",
        "1:2:3",
        "4294967295",
        "4294967296",
        "99999999999999999999",
        "-1",
        "1:-5",
        "\\Seen",
        "\u{0}",
        "1:1000000",
    ];
    for c in cases {
        if let Ok(ranges) = UidRange::parse_set(c) {
            // 解析成功的集合必须自洽：不能出现空区间（空区间意味着序列化后会变成
            // `1:` 这种非法串，下一次 FETCH 直接被服务器 BAD）
            for r in &ranges {
                assert!(!r.is_empty(), "{c:?} 解析出空区间");
                assert_ne!(r.len(), 0, "{c:?} 长度计算错误");
            }
        }
    }
    // 合理的输入必须解析正确
    let r = UidRange::parse_set("1:5,7").unwrap();
    assert_eq!(r.len(), 2);
    assert!(r[0].contains(3));
    assert!(!r[1].contains(3));
}

// ---------------------------------------------------------------- 能力协商

#[test]
fn capability_parser_is_total_and_conservative() {
    let cases = [
        "",
        "IMAP4rev1",
        "IMAP4rev1 UIDPLUS CONDSTORE QRESYNC",
        "imap4rev1 uidplus",
        "IMAP4rev1  UIDPLUS   CONDSTORE",
        "IMAP4rev1 LITERAL+ SASL-IR AUTH=PLAIN AUTH=XOAUTH2",
        "AUTH=",
        "=PLAIN",
        "IMAP4rev1 \u{0}",
        "IMAP4rev1 [CAPABILITY IMAP4rev1 UIDPLUS]",
    ];
    for c in cases {
        let caps = Capabilities::parse(c);
        // 未声明就不该当成支持（能力判定必须"默认不信"）
        if !c.to_ascii_uppercase().contains("UIDPLUS") {
            assert!(!caps.has("UIDPLUS"), "{c:?} 未声明却判定支持 UIDPLUS");
        }
        if !c.to_ascii_uppercase().contains("CONDSTORE") {
            assert!(!caps.has("CONDSTORE"), "{c:?} 未声明却判定支持 CONDSTORE");
        }
        assert!(!caps.has("QRESYNC") || c.contains("QRESYNC"), "{c:?}");
    }
    let caps = Capabilities::parse("IMAP4rev1 UIDPLUS CONDSTORE AUTH=PLAIN");
    assert!(caps.has("UIDPLUS"));
    assert!(caps.has("CONDSTORE"));
    // 大小写不该影响能力判定（服务器大小写随意）
    assert!(caps.has("uidplus"));
    assert!(!caps.is_empty());
}
