//! IMAP 响应词法单元（近似 S-表达式），供 FETCH / BODYSTRUCTURE / LIST 复用。

use crossmail_core::error::{MailError, MailResult};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Atom {
    /// 裸原子（关键字、数字、flag）
    Word(String),
    /// 带引号字符串或 literal 内容
    Str(String),
    /// 括号列表
    List(Vec<Atom>),
    /// NIL
    Nil,
}

impl Atom {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Atom::Word(s) | Atom::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Atom]> {
        match self {
            Atom::List(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        self.as_str()?.trim().parse().ok()
    }

    pub fn as_u32(&self) -> Option<u32> {
        self.as_str()?.trim().parse().ok()
    }

    pub fn is_nil(&self) -> bool {
        matches!(self, Atom::Nil)
    }

    /// NIL 与空字符串统一视为 None。
    pub fn text(&self) -> Option<String> {
        match self {
            Atom::Word(s) | Atom::Str(s) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    /// 遍历当前列表内的键值对（形如 `UID 123 FLAGS (\Seen)`）。
    pub fn pairs(&self) -> Vec<(String, Atom)> {
        let mut out = Vec::new();
        if let Some(items) = self.as_list() {
            let mut i = 0;
            while i < items.len() {
                let key = items[i].as_str().map(|s| s.to_ascii_uppercase());
                match (key, items.get(i + 1)) {
                    (Some(k), Some(v)) => {
                        out.push((k, v.clone()));
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
        }
        out
    }
}

/// 括号嵌套层数上限。
///
/// 真实 IMAP 的 BODYSTRUCTURE 最深不过 5~6 层（multipart → part → 参数表），
/// 但**畸形/恶意响应可以随便塞几千个左括号**。本解析器是递归的，不设上限时
/// 2000 层就会把栈打爆——而栈溢出是 SIGABRT，`catch_unwind` 兜不住，
/// 整个 App 直接死。这是不可接受的崩溃面，所以必须在词法层就截断。
const MAX_DEPTH: usize = 32;

/// 解析一段 IMAP 文本为顶层单元序列。
pub fn parse_atoms(input: &str) -> MailResult<Vec<Atom>> {
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0usize;
    let mut out = Vec::new();
    parse_seq(&chars, &mut i, &mut out, false, 0)?;
    Ok(out)
}

fn parse_seq(
    chars: &[char],
    i: &mut usize,
    out: &mut Vec<Atom>,
    nested: bool,
    depth: usize,
) -> MailResult<()> {
    if depth > MAX_DEPTH {
        return Err(MailError::Parse(format!(
            "IMAP 响应嵌套超过 {MAX_DEPTH} 层，已拒绝解析"
        )));
    }
    loop {
        while *i < chars.len() && chars[*i].is_whitespace() {
            *i += 1;
        }
        if *i >= chars.len() {
            return if nested {
                Err(MailError::Parse("IMAP 响应括号未闭合".into()))
            } else {
                Ok(())
            };
        }
        match chars[*i] {
            ')' => {
                if nested {
                    *i += 1;
                    return Ok(());
                }
                return Err(MailError::Parse("多余的右括号".into()));
            }
            '(' => {
                *i += 1;
                let mut sub = Vec::new();
                parse_seq(chars, i, &mut sub, true, depth + 1)?;
                out.push(Atom::List(sub));
            }
            '"' => {
                *i += 1;
                let mut buf = String::new();
                while *i < chars.len() {
                    let c = chars[*i];
                    *i += 1;
                    if c == '\\' && *i < chars.len() {
                        buf.push(chars[*i]);
                        *i += 1;
                    } else if c == '"' {
                        break;
                    } else {
                        buf.push(c);
                    }
                }
                out.push(Atom::Str(buf));
            }
            _ => {
                let start = *i;
                while *i < chars.len()
                    && !chars[*i].is_whitespace()
                    && chars[*i] != '('
                    && chars[*i] != ')'
                {
                    *i += 1;
                }
                let word: String = chars[start..*i].iter().collect();
                if word.eq_ignore_ascii_case("NIL") {
                    out.push(Atom::Nil);
                } else {
                    out.push(Atom::Word(word));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_pathological_nesting_instead_of_overflowing() {
        // 回归：递归解析器在 ~2000 层括号下会栈溢出（SIGABRT，无法 unwind）。
        // 服务器发一条畸形 BODYSTRUCTURE 就能让整个 App 死，必须在词法层拒绝。
        let deep = format!("{}{}", "(".repeat(2_000), ")".repeat(2_000));
        let err = parse_atoms(&deep).unwrap_err();
        assert!(format!("{err}").contains("嵌套"), "{err}");

        // 真实邮件的嵌套深度必须照常可用
        let ok = format!(
            "{}({}){}",
            "(".repeat(20),
            r#""TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" 1 1"#,
            ")".repeat(20)
        );
        assert!(parse_atoms(&ok).is_ok());
    }

    #[test]
    fn nested_list_and_quoted() {
        let atoms = parse_atoms(r#"(\Seen \Flagged) "/" "Sent Items""#).unwrap();
        assert_eq!(atoms.len(), 3);
        assert_eq!(atoms[0].as_list().unwrap().len(), 2);
        assert_eq!(atoms[2].as_str(), Some("Sent Items"));
    }

    #[test]
    fn escapes_inside_quotes() {
        let atoms = parse_atoms(r#""a\"b""#).unwrap();
        assert_eq!(atoms[0].as_str(), Some("a\"b"));
    }

    #[test]
    fn nil_becomes_none() {
        let atoms = parse_atoms("NIL").unwrap();
        assert!(atoms[0].is_nil());
        assert!(atoms[0].text().is_none());
    }

    #[test]
    fn pairs_extraction() {
        let atoms = parse_atoms("(UID 12 FLAGS (\\Seen) SIZE 340)").unwrap();
        let pairs = atoms[0].pairs();
        assert_eq!(pairs[0].0, "UID");
        assert_eq!(pairs[0].1.as_u32(), Some(12));
        assert_eq!(pairs[1].0, "FLAGS");
    }

    #[test]
    fn unbalanced_paren_is_error() {
        assert!(parse_atoms("(UID 1").is_err());
    }
}
