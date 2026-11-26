//! RFC 2047 编码词解码（`=?charset?encoding?encoded-text?=`）。
//!
//! 为什么非做不可：IMAP 的 `ENVELOPE` 返回的是**线上原样**，非 ASCII 主题一定是编码词。
//! 不解码的话，中文主题在客户端里就是 `=?utf-8?B?5L2g5aW9?=` 这种东西——
//! 真机实测主题 `CrossMail-idle-1112329 IDLE 驱动同步` 返回的正是
//! `=?utf-8?B?Q3Jvc3NNYWlsLWlkbGUtMTExMjMyOSBJRExFIOmpseWKqOWQjOWQjOatpQ==?=`。
//! JMAP 侧返回的是解码后的字符串，所以这个缺陷只在 IMAP 上暴露。
//!
//! 只处理 `B`（base64）与 `Q`（quoted-printable 变体，`_` 代表空格）两种编码，
//! 这是 RFC 2047 定义的全部。
//!
//! **字符集边界（照实说明）**：只转 `utf-8` / `us-ascii` / `iso-8859-1`（含 `latin1`）。
//! 其它字符集（`gbk`、`big5`、`iso-2022-jp`……）**原样保留编码词**——本 crate 不带字符集
//! 转码库，猜错会产出比原文更难排查的乱码，宁可留着让上层看得见「这里没解」。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

/// 一个解析出来的编码词。
struct EncodedWord<'a> {
    charset: &'a str,
    encoding: char,
    text: &'a str,
}

/// 行内片段：普通文本或编码词。
enum Segment<'a> {
    Text(&'a str),
    Word(EncodedWord<'a>),
}

/// 解码字符串里所有的 RFC 2047 编码词。
pub fn decode_encoded_words(input: &str) -> String {
    let segments = split_segments(input);
    if !segments.iter().any(|s| matches!(s, Segment::Word(_))) {
        // 没有编码词就原样返回，绝不做任何「顺手规范化」——标题里的空白有语义
        return input.to_string();
    }

    let mut out = String::with_capacity(input.len());
    for (idx, seg) in segments.iter().enumerate() {
        match seg {
            Segment::Word(w) => out.push_str(&decode_word(w)),
            Segment::Text(t) => {
                // RFC 2047 §6.2：相邻两个编码词之间的分隔空白要丢弃。
                // 长文本会被切成多个 `=?..?= =?..?=`，不丢空白就会凭空多出空格。
                let between_words = idx > 0
                    && idx + 1 < segments.len()
                    && matches!(segments[idx - 1], Segment::Word(_))
                    && matches!(segments[idx + 1], Segment::Word(_))
                    && t.chars()
                        .all(|c| c == ' ' || c == '\t' || c == '\r' || c == '\n');
                if !between_words {
                    out.push_str(t);
                }
            }
        }
    }
    out
}

fn split_segments(input: &str) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut cursor = 0usize;
    let mut text_start = 0usize;

    while cursor + 1 < bytes.len() {
        if bytes[cursor] == b'=' && bytes[cursor + 1] == b'?' {
            if let Some((end, word)) = parse_word(&input[cursor..]) {
                if cursor > text_start {
                    out.push(Segment::Text(&input[text_start..cursor]));
                }
                out.push(Segment::Word(word));
                cursor += end;
                text_start = cursor;
                continue;
            }
        }
        cursor += 1;
    }
    if text_start < input.len() {
        out.push(Segment::Text(&input[text_start..]));
    }
    out
}

/// 解析以 `=?` 开头的一个编码词，返回（消耗字节数, 解析结果）。
fn parse_word(s: &str) -> Option<(usize, EncodedWord<'_>)> {
    let rest = s.strip_prefix("=?")?;
    let (charset, rest) = rest.split_once('?')?;
    if charset.is_empty() || charset.contains(' ') {
        return None;
    }
    let (enc, rest) = rest.split_once('?')?;
    let encoding = match enc {
        "B" | "b" => 'B',
        "Q" | "q" => 'Q',
        _ => return None,
    };
    let (text, _) = rest.split_once("?=")?;
    // RFC 2047：encoded-text 里不允许出现空格与 `?`
    if text.contains(' ') || text.contains('?') {
        return None;
    }
    let consumed = 2 + charset.len() + 1 + 1 + 1 + text.len() + 2;
    Some((
        consumed,
        EncodedWord {
            charset,
            encoding,
            text,
        },
    ))
}

fn decode_word(word: &EncodedWord<'_>) -> String {
    let raw = match word.encoding {
        'B' => match STANDARD.decode(word.text.as_bytes()) {
            Ok(bytes) => bytes,
            // 解不开就保留原文：把一段坏掉的 base64 变成空串是最糟的选择
            Err(_) => return reencode(word),
        },
        'Q' => match decode_q(word.text) {
            Some(bytes) => bytes,
            None => return reencode(word),
        },
        _ => return reencode(word),
    };

    match word.charset.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" | "us-ascii" | "ascii" => String::from_utf8_lossy(&raw).into_owned(),
        // latin1 是「字节即码点」，无需查表，转出来一定无损
        "iso-8859-1" | "latin1" => raw.iter().map(|b| *b as char).collect(),
        _ => reencode(word),
    }
}

/// 把解码失败/字符集不支持的词还原成原始编码词，保持可诊断。
fn reencode(word: &EncodedWord<'_>) -> String {
    format!("=?{}?{}?{}?=", word.charset, word.encoding, word.text)
}

/// RFC 2047 的 `Q` 编码：`=XX` 十六进制，`_` 表示空格。
fn decode_q(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'_' => {
                out.push(b' ');
                i += 1;
            }
            b'=' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机返回的原文（原样摘自 live 失败输出），必须解成可读主题。
    #[test]
    fn decodes_real_world_subject() {
        let raw = "=?utf-8?B?Q3Jvc3NNYWlsLWlkbGUtMTExMjMyOSBJRExFIOmpseWKqOWQjOatpQ==?=";
        assert_eq!(
            decode_encoded_words(raw),
            "CrossMail-idle-1112329 IDLE 驱动同步"
        );
    }

    #[test]
    fn decodes_q_encoding_with_underscore_as_space() {
        assert_eq!(decode_encoded_words("=?utf-8?Q?a_b=20c?="), "a b c");
    }

    #[test]
    fn plain_text_passes_through_untouched() {
        assert_eq!(decode_encoded_words("Hello World"), "Hello World");
        // 没有编码词时连空白都不能动
        assert_eq!(decode_encoded_words("  spaced  out  "), "  spaced  out  ");
    }

    /// 长主题会被切成多个相邻编码词，中间的分隔空白必须丢掉，否则凭空多空格。
    #[test]
    fn adjacent_words_drop_separating_whitespace() {
        let a = "=?utf-8?B?5L2g?="; // 你
        let b = "=?utf-8?B?5aW9?="; // 好
        assert_eq!(decode_encoded_words(&format!("{a} {b}")), "你好");
        assert_eq!(decode_encoded_words(&format!("{a}\r\n {b}")), "你好");
    }

    /// 文本与编码词混排：只解编码词，其余原样。
    #[test]
    fn mixed_text_and_words() {
        assert_eq!(
            decode_encoded_words("Re: =?utf-8?B?5L2g5aW9?= (fwd)"),
            "Re: 你好 (fwd)"
        );
    }

    #[test]
    fn latin1_decoded_byte_wise() {
        assert_eq!(decode_encoded_words("=?iso-8859-1?Q?caf=E9?="), "café");
    }

    /// 不支持的字符集要**保留原编码词**：解错比不解更难排查。
    #[test]
    fn unsupported_charset_is_left_visible() {
        let raw = "=?gbk?B?xOO6ww==?=";
        assert_eq!(decode_encoded_words(raw), raw);
    }

    /// 坏掉的 base64 不能变成空串（那会让整封邮件的主题凭空消失）。
    #[test]
    fn broken_base64_keeps_original() {
        let raw = "=?utf-8?B?!!!not-base64!!!?=";
        assert_eq!(decode_encoded_words(raw), raw);
    }

    /// 格式不完整时不吞字符，按普通文本处理。
    #[test]
    fn malformed_word_is_not_eaten() {
        for raw in ["=?utf-8?B", "=?utf-8?", "=? ?B?x?=", "=?utf-8?X?x?="] {
            assert_eq!(decode_encoded_words(raw), raw, "原文应完整保留: {raw}");
        }
    }
}
