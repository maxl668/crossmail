//! IMAP 文件夹名 Modified UTF-7 编解码（RFC 3501 §5.1.3 / 需求 I-003）。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

/// 把本地 UTF-8 名称编码为 IMAP Modified UTF-7。
pub fn encode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut buf: Vec<u8> = Vec::new();

    fn flush(buf: &mut Vec<u8>, out: &mut String) {
        if buf.is_empty() {
            return;
        }
        let mut b64 = STANDARD.encode(&buf[..]);
        while b64.ends_with('=') {
            b64.pop();
        }
        out.push('&');
        out.push_str(&b64.replace('/', ","));
        out.push('-');
        buf.clear();
    }

    for ch in name.chars() {
        if ch == '&' {
            flush(&mut buf, &mut out);
            out.push_str("&-");
        } else if ('\u{20}'..='\u{7e}').contains(&ch) {
            flush(&mut buf, &mut out);
            out.push(ch);
        } else {
            let mut units = [0u16; 2];
            for u in ch.encode_utf16(&mut units).iter() {
                buf.extend_from_slice(&u.to_be_bytes());
            }
        }
    }
    flush(&mut buf, &mut out);
    out
}

/// 将 IMAP 原始名称解码为 UTF-8。非法序列按字面保留，避免整个文件夹列表失败。
pub fn decode(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let chars: Vec<char> = name.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '&' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        match chars[i + 1..].iter().position(|c| *c == '-') {
            Some(rel) => {
                let end = i + 1 + rel;
                let chunk: String = chars[i + 1..end].iter().collect();
                if chunk.is_empty() {
                    out.push('&');
                } else {
                    match decode_chunk(&chunk) {
                        Some(s) => out.push_str(&s),
                        None => {
                            out.push('&');
                            out.push_str(&chunk);
                            out.push('-');
                        }
                    }
                }
                i = end + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

fn decode_chunk(chunk: &str) -> Option<String> {
    let standard = chunk.replace(',', "/");
    let mut padded = standard;
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = STANDARD.decode(padded.as_bytes()).ok()?;
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passthrough() {
        assert_eq!(encode("Sent"), "Sent");
        assert_eq!(decode("Sent"), "Sent");
    }

    #[test]
    fn ampersand_escaped() {
        assert_eq!(encode("A&B"), "A&-B");
        assert_eq!(decode("A&-B"), "A&B");
    }

    #[test]
    fn chinese_roundtrip() {
        let name = "已发送邮件";
        let encoded = encode(name);
        assert!(encoded.starts_with('&') && encoded.ends_with('-'));
        assert_eq!(decode(&encoded), name);
    }

    #[test]
    fn mailbox_path_roundtrip() {
        let name = "工作/2026 项目";
        assert_eq!(decode(&encode(name)), name);
    }

    #[test]
    fn invalid_sequence_kept_literal() {
        assert_eq!(decode("&bad"), "&bad");
    }
}
