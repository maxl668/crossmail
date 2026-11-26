//! MIME 传输编码解码（Content-Transfer-Encoding）。
//!
//! 为什么必须有这一层：`UID FETCH BODY[1]` 返回的是**传输编码后的原始字节**，
//! 不是可读文本。正文用 `base64` 或 `quoted-printable` 编码时（中文邮件、富文本邮件
//! 的绝大多数），直接把字节按 UTF-8 解释出来就是一屏乱码。
//!
//! 本模块只做编码解码，不做字符集转换：`charset` 非 UTF-8 的正文在
//! `decode_to_string` 里按 UTF-8 尽力解码并显式标注 —— 宁可在元数据里承认真实情况，
//! 也不要假装 GBK 正文是 UTF-8。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

/// 传输编码（大小写不敏感，未知值按 7bit 处理）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferEncoding {
    SevenBit,
    EightBit,
    Binary,
    Base64,
    QuotedPrintable,
}

impl TransferEncoding {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "base64" => TransferEncoding::Base64,
            "quoted-printable" => TransferEncoding::QuotedPrintable,
            "8bit" => TransferEncoding::EightBit,
            "binary" => TransferEncoding::Binary,
            // 7bit / 空 / 任何没见过的值：一律按原样传递
            _ => TransferEncoding::SevenBit,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            TransferEncoding::SevenBit => "7bit",
            TransferEncoding::EightBit => "8bit",
            TransferEncoding::Binary => "binary",
            TransferEncoding::Base64 => "base64",
            TransferEncoding::QuotedPrintable => "quoted-printable",
        }
    }
}

/// 按传输编码解码。解码失败**不报错**，原样返回 + `Ok(false)`：
/// 正文是给用户看的内容，宁可显示原文让用户自己判断，也不能因为一段编码错
/// 就让整封邮件打不开。
pub fn decode(bytes: &[u8], encoding: TransferEncoding) -> (Vec<u8>, bool) {
    match encoding {
        TransferEncoding::SevenBit | TransferEncoding::EightBit | TransferEncoding::Binary => {
            (bytes.to_vec(), true)
        }
        TransferEncoding::Base64 => match decode_base64(bytes) {
            Some(v) => (v, true),
            None => (bytes.to_vec(), false),
        },
        TransferEncoding::QuotedPrintable => (decode_quoted_printable(bytes), true),
    }
}

/// 解码并转字符串。
///
/// 返回 `(文本, 编码是否成功, 字符集是否被如实支持)`：
/// 字符集不是 UTF-8/US-ASCII 时，第三项为 `false`，调用方应把这个事实透出到界面，
/// 而不是让用户对着一屏问号猜。
pub fn decode_to_string(
    bytes: &[u8],
    encoding: TransferEncoding,
    charset: Option<&str>,
) -> (String, bool, bool) {
    let (raw, ok) = decode(bytes, encoding);
    let cs = charset.unwrap_or("").trim().to_ascii_lowercase();
    let utf8_family = matches!(
        cs.as_str(),
        "" | "utf-8" | "utf8" | "us-ascii" | "ascii" | "ansi_x3.4-1968"
    );
    if utf8_family {
        (String::from_utf8_lossy(&raw).into_owned(), ok, true)
    } else {
        // 没有码表就不假装能转：按 UTF-8 尽力解码，并把「字符集未支持」如实上报
        (String::from_utf8_lossy(&raw).into_owned(), ok, false)
    }
}

fn decode_base64(bytes: &[u8]) -> Option<Vec<u8>> {
    // 传输层按 76 列折行，必须先剔除所有空白
    let cleaned: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|b| !matches!(b, b'\r' | b'\n' | b' ' | b'\t'))
        .collect();
    if cleaned.is_empty() {
        return Some(Vec::new());
    }
    // 缺 padding 的服务器不少，补齐再解；仍失败就返回 None 交调用方兜底
    let mut padded = cleaned.clone();
    let rem = padded.len() % 4;
    if rem != 0 {
        padded.extend(std::iter::repeat(b'=').take(4 - rem));
    }
    STANDARD
        .decode(&padded)
        .or_else(|_| STANDARD.decode(&cleaned))
        .ok()
}

fn decode_quoted_printable(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b != b'=' {
            out.push(b);
            i += 1;
            continue;
        }
        // 软换行：`=` 后紧跟 CRLF（或裸 LF）表示续行，直接吞掉
        if matches!(bytes.get(i + 1), Some(b'\r')) && matches!(bytes.get(i + 2), Some(b'\n')) {
            i += 3;
            continue;
        }
        if matches!(bytes.get(i + 1), Some(b'\n')) {
            i += 2;
            continue;
        }
        // `=XX` 十六进制
        let hi = bytes.get(i + 1).copied().and_then(hex_val);
        let lo = bytes.get(i + 2).copied().and_then(hex_val);
        match (hi, lo) {
            (Some(h), Some(l)) => {
                out.push(h * 16 + l);
                i += 3;
            }
            // 孤立的 `=`（后面不是合法十六进制）：按字面保留，别吞数据
            _ => {
                out.push(b'=');
                i += 1;
            }
        }
    }
    out
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_parse_is_case_insensitive() {
        assert_eq!(TransferEncoding::parse("BASE64"), TransferEncoding::Base64);
        assert_eq!(
            TransferEncoding::parse(" Quoted-Printable "),
            TransferEncoding::QuotedPrintable
        );
        // 未知值不能当成空，按 7bit 原样透传才是安全默认
        assert_eq!(
            TransferEncoding::parse("x-uuencode"),
            TransferEncoding::SevenBit
        );
        assert_eq!(TransferEncoding::parse(""), TransferEncoding::SevenBit);
    }

    #[test]
    fn base64_decodes_wrapped_lines() {
        // 真实邮件按 76 列折行，还必须容忍末尾少 padding
        let raw = "bGl2ZSBpbnRlZ3JhdGlvbiBib2R5\r\n";
        let (out, ok) = decode(raw.as_bytes(), TransferEncoding::Base64);
        assert!(ok);
        assert_eq!(String::from_utf8_lossy(&out), "live integration body");

        let (out, ok) = decode(b"YWJj", TransferEncoding::Base64);
        assert!(ok);
        assert_eq!(out, b"abc");
    }

    #[test]
    fn base64_garbage_falls_back_to_raw() {
        // 解码失败要原样返回而不是空：宁可显示原文也不能把正文弄没
        let (out, ok) = decode(b"!!!not-base64!!!", TransferEncoding::Base64);
        assert!(!ok);
        assert_eq!(out, b"!!!not-base64!!!");
    }

    #[test]
    fn quoted_printable_handles_soft_break_and_hex() {
        // `=` + CRLF 是软换行（续行），吞掉；`=E4=B8=AD` 是「中」
        let raw = b"hello=\r\n world =E4=B8=AD=E6=96=87";
        let (out, ok) = decode(raw, TransferEncoding::QuotedPrintable);
        assert!(ok);
        assert_eq!(
            String::from_utf8_lossy(&out),
            "hello world 中文".to_string()
        );
    }

    #[test]
    fn quoted_printable_keeps_lone_equals() {
        // 孤立 `=` 后不是合法十六进制时按字面保留，不能吞字符
        let (out, _) = decode(b"a=b", TransferEncoding::QuotedPrintable);
        assert_eq!(out, b"a=b");
    }

    #[test]
    fn seven_bit_passes_through_untouched() {
        let raw = "纯文本 body".as_bytes();
        let (out, ok) = decode(raw, TransferEncoding::SevenBit);
        assert!(ok);
        assert_eq!(out, raw);
    }

    #[test]
    fn non_utf8_charset_is_declared_not_faked() {
        // 「中文」的 GBK 字节，先按 base64 编码（真实邮件的形态）
        let gbk = [0xD6u8, 0xD0, 0xCE, 0xC4];
        let b64 = STANDARD.encode(gbk);
        let (text, enc_ok, charset_ok) =
            decode_to_string(b64.as_bytes(), TransferEncoding::Base64, Some("gb2312"));
        assert!(enc_ok, "base64 解码本身应成功");
        // 字节确实解出来了（编码层没错），只是缺 GBK 码表所以字符是坏的：
        // 必须如实报告「字符集未支持」，不能声称解码正确
        assert!(!charset_ok, "gb2312 应被标注为未支持");
        assert!(!text.is_empty());

        let (text, _, charset_ok) =
            decode_to_string(b"aGVsbG8=", TransferEncoding::Base64, Some("utf-8"));
        assert_eq!(text, "hello");
        assert!(charset_ok);
    }
}
