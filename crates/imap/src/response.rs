//! IMAP 响应解析：tagged 完成响应、untagged 数据、response code（RFC 3501 / 4315）。

use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Address, Flag, MailboxRole};
use std::collections::BTreeMap;

use crate::atom::{parse_atoms, Atom};
use crate::bodystructure::BodyPart;
use crate::capability::Capabilities;
use crate::uid::UidRange;
use crate::utf7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    No,
    Bad,
    Continue,
    PreAuth,
    Bye,
}

impl Status {
    pub fn parse(s: &str) -> Option<Status> {
        match s.to_ascii_uppercase().as_str() {
            "OK" => Some(Status::Ok),
            "NO" => Some(Status::No),
            "BAD" => Some(Status::Bad),
            "PREAUTH" => Some(Status::PreAuth),
            "BYE" => Some(Status::Bye),
            _ => None,
        }
    }

    pub fn is_ok(self) -> bool {
        matches!(self, Status::Ok | Status::PreAuth)
    }
}

/// `[UIDVALIDITY 1]` 一类的响应码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseCode {
    UidValidity(u32),
    UidNext(u32),
    HighestModSeq(u64),
    AppendUid {
        uid_validity: u32,
        uid: u32,
    },
    CopyUid {
        uid_validity: u32,
        src: Vec<UidRange>,
        dst: Vec<UidRange>,
    },
    ReadOnly,
    ReadWrite,
    Capability(Capabilities),
    Alert(String),
    TryCreate,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaggedResponse {
    pub tag: String,
    pub status: Status,
    pub code: Option<ResponseCode>,
    pub text: String,
}

impl TaggedResponse {
    pub fn is_ok(&self) -> bool {
        self.status.is_ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEntry {
    pub flags: Vec<String>,
    pub delimiter: Option<char>,
    /// 已解码为 UTF-8 的名称
    pub name: String,
    pub role: MailboxRole,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Envelope {
    pub date: Option<String>,
    pub subject: Option<String>,
    pub from: Vec<Address>,
    pub sender: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub in_reply_to: Option<String>,
    pub message_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchItem {
    pub seq: u32,
    pub uid: Option<u32>,
    pub flags: Vec<Flag>,
    pub size: Option<u64>,
    pub internal_date: Option<String>,
    pub modseq: Option<u64>,
    pub envelope: Option<Envelope>,
    pub body: Option<BodyPart>,
}

// FetchItem 携带完整 ENVELOPE/BODYSTRUCTURE，体积天然大于其他变体；
// 一次响应只会构造少量实例，用枚举直接持有比 Box 更省心。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Untagged {
    Capability(Capabilities),
    List(ListEntry),
    Fetch(FetchItem),
    Exists(u32),
    Recent(u32),
    Expunge(u32),
    Status {
        mailbox: String,
        items: BTreeMap<String, u64>,
    },
    Ok(Option<ResponseCode>, String),
    No(String),
    Bye(String),
    Other(String),
}

/// 解析 `A1 OK [APPENDUID 1 10] done` 形式的完成响应。
pub fn parse_tagged(line: &str) -> MailResult<Option<TaggedResponse>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('*') || line.starts_with('+') {
        return Ok(None);
    }
    let mut parts = line.splitn(3, ' ');
    let tag = match parts.next() {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => return Ok(None),
    };
    let status_word = match parts.next() {
        Some(s) => s,
        None => return Ok(None),
    };
    let status = match Status::parse(status_word) {
        Some(s) => s,
        None => return Ok(None),
    };
    let text = parts.next().unwrap_or("").trim().to_string();
    let code = parse_response_code(&text);
    Ok(Some(TaggedResponse {
        tag,
        status,
        code,
        text,
    }))
}

/// 从响应文本中提取 `[...]` 响应码。
pub fn parse_response_code(text: &str) -> Option<ResponseCode> {
    let start = text.find('[')?;
    let end = text[start..].find(']')? + start;
    let inner = &text[start + 1..end];
    let mut it = inner.splitn(2, ' ');
    let key = it.next()?.to_ascii_uppercase();
    let rest = it.next().unwrap_or("").trim();
    Some(match key.as_str() {
        "UIDVALIDITY" => ResponseCode::UidValidity(rest.parse().ok()?),
        "UIDNEXT" => ResponseCode::UidNext(rest.parse().ok()?),
        "HIGHESTMODSEQ" => ResponseCode::HighestModSeq(rest.parse().ok()?),
        "APPENDUID" => {
            let mut p = rest.split_whitespace();
            ResponseCode::AppendUid {
                uid_validity: p.next()?.parse().ok()?,
                uid: p.next()?.parse().ok()?,
            }
        }
        "COPYUID" => {
            let mut p = rest.split_whitespace();
            ResponseCode::CopyUid {
                uid_validity: p.next()?.parse().ok()?,
                src: UidRange::parse_set(p.next()?).ok()?,
                dst: UidRange::parse_set(p.next()?).ok()?,
            }
        }
        "READ-ONLY" => ResponseCode::ReadOnly,
        "READ-WRITE" => ResponseCode::ReadWrite,
        "CAPABILITY" => ResponseCode::Capability(Capabilities::parse(inner)),
        "ALERT" => ResponseCode::Alert(rest.to_string()),
        "TRYCREATE" => ResponseCode::TryCreate,
        _ => ResponseCode::Other(inner.to_string()),
    })
}

/// 解析一行 untagged 响应（含前导 `* `）。
pub fn parse_untagged(line: &str) -> MailResult<Untagged> {
    let body = line
        .trim_start()
        .strip_prefix('*')
        .map(|s| s.trim_start())
        .unwrap_or(line.trim());

    let mut it = body.splitn(2, ' ');
    let head = it.next().unwrap_or("");
    let rest = it.next().unwrap_or("").trim();

    // `* 12 FETCH (...)` / `* 12 EXISTS`
    if head.chars().all(|c| c.is_ascii_digit()) && !head.is_empty() {
        let seq: u32 = head
            .parse()
            .map_err(|_| MailError::Parse(format!("非法序号: {head}")))?;
        let mut r = rest.splitn(2, ' ');
        let kw = r.next().unwrap_or("").to_ascii_uppercase();
        let payload = r.next().unwrap_or("").trim();
        return Ok(match kw.as_str() {
            "EXISTS" => Untagged::Exists(seq),
            "RECENT" => Untagged::Recent(seq),
            "EXPUNGE" => Untagged::Expunge(seq),
            "FETCH" => {
                let atoms = parse_atoms(payload)?;
                let list = atoms
                    .first()
                    .ok_or_else(|| MailError::Parse("FETCH 无属性列表".into()))?;
                let mut item = parse_fetch_attrs(list)?;
                item.seq = seq;
                Untagged::Fetch(item)
            }
            _ => Untagged::Other(body.to_string()),
        });
    }

    let kw = head.to_ascii_uppercase();
    Ok(match kw.as_str() {
        "CAPABILITY" => Untagged::Capability(Capabilities::parse(body)),
        "LIST" | "LSUB" => {
            let atoms = parse_atoms(rest)?;
            Untagged::List(parse_list_entry(&atoms)?)
        }
        "STATUS" => {
            let atoms = parse_atoms(rest)?;
            let mailbox = atoms
                .first()
                .and_then(|a| a.text())
                .map(|n| utf7::decode(&n))
                .ok_or_else(|| MailError::Parse("STATUS 缺少邮箱名".into()))?;
            let mut items = BTreeMap::new();
            if let Some(list) = atoms.get(1).and_then(|a| a.as_list()) {
                let mut i = 0;
                while i + 1 < list.len() {
                    if let (Some(k), Some(v)) = (list[i].as_str(), list[i + 1].as_u64()) {
                        items.insert(k.to_ascii_uppercase(), v);
                    }
                    i += 2;
                }
            }
            Untagged::Status { mailbox, items }
        }
        "OK" => Untagged::Ok(parse_response_code(rest), rest.to_string()),
        "NO" => Untagged::No(rest.to_string()),
        "BYE" => Untagged::Bye(rest.to_string()),
        _ => Untagged::Other(body.to_string()),
    })
}

fn parse_list_entry(atoms: &[Atom]) -> MailResult<ListEntry> {
    let flag_atoms = atoms
        .first()
        .ok_or_else(|| MailError::Parse("LIST 缺少属性".into()))?;
    let flags: Vec<String> = flag_atoms
        .as_list()
        .ok_or_else(|| MailError::Parse("LIST 属性不是列表".into()))?
        .iter()
        .filter_map(|a| a.as_str().map(|s| s.to_string()))
        .collect();

    let delimiter = atoms
        .get(1)
        .and_then(|a| a.as_str())
        .and_then(|s| s.chars().next());

    let raw_name = atoms
        .get(2)
        .and_then(|a| a.text())
        .ok_or_else(|| MailError::Parse("LIST 缺少邮箱名".into()))?;

    // 优先取能映射到具体角色的标记（\HasNoChildren 之类的属性必须先跳过）
    let role = flags
        .iter()
        .filter(|f| f.starts_with('\\'))
        .map(|f| MailboxRole::from_special_use(f))
        .find(|r| *r != MailboxRole::Other)
        .unwrap_or(MailboxRole::Other);

    Ok(ListEntry {
        flags,
        delimiter,
        name: utf7::decode(&raw_name),
        role,
    })
}

fn parse_fetch_attrs(attrs: &Atom) -> MailResult<FetchItem> {
    let mut item = FetchItem::default();
    for (key, value) in attrs.pairs() {
        match key.as_str() {
            "UID" => item.uid = value.as_u32(),
            "FLAGS" => {
                item.flags = value
                    .as_list()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|a| a.as_str().and_then(Flag::from_imap))
                    .collect();
            }
            "RFC822.SIZE" => item.size = value.as_u64(),
            "INTERNALDATE" => item.internal_date = value.text(),
            "MODSEQ" => {
                item.modseq = value
                    .as_list()
                    .and_then(|l| l.first())
                    .and_then(|a| a.as_u64());
            }
            "ENVELOPE" => item.envelope = Some(parse_envelope(&value)?),
            "BODYSTRUCTURE" | "BODY" if value.as_list().is_some() => {
                item.body = Some(BodyPart::parse(&value)?);
            }
            _ => {}
        }
    }
    Ok(item)
}

fn parse_envelope(atom: &Atom) -> MailResult<Envelope> {
    let items = atom
        .as_list()
        .ok_or_else(|| MailError::Parse("ENVELOPE 不是列表".into()))?;
    let get = |i: usize| items.get(i).and_then(|a| a.text());
    let addr = |i: usize| items.get(i).map(parse_address_list).unwrap_or_default();
    Ok(Envelope {
        date: get(0),
        // ENVELOPE 返回的是**线上原样**：非 ASCII 主题一定带 RFC 2047 编码词。
        // 不在解析层解掉，中文主题到上层就是 `=?utf-8?B?...?=` 一串乱码
        // （JMAP 侧返回的是解码后的值，所以这个坑只在 IMAP 上暴露）。
        subject: get(1).map(|s| crate::rfc2047::decode_encoded_words(&s)),
        from: addr(2),
        sender: addr(3),
        reply_to: addr(4),
        to: addr(5),
        cc: addr(6),
        bcc: addr(7),
        in_reply_to: get(8),
        message_id: get(9),
    })
}

fn parse_address_list(atom: &Atom) -> Vec<Address> {
    let Some(list) = atom.as_list() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for a in list {
        let Some(parts) = a.as_list() else { continue };
        let name = parts.first().and_then(|x| x.text());
        let mailbox = parts.get(2).and_then(|x| x.text()).unwrap_or_default();
        let host = parts.get(3).and_then(|x| x.text());
        let email = match host {
            Some(h) if !h.is_empty() => format!("{mailbox}@{h}"),
            _ => mailbox.clone(),
        };
        if email.is_empty() {
            continue;
        }
        out.push(match name {
            // 显示名与主题同源，同样可能是 RFC 2047 编码词
            Some(n) if !n.is_empty() => {
                Address::with_name(crate::rfc2047::decode_encoded_words(&n), email)
            }
            _ => Address::new(email),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_ok_with_appenduid() {
        let r = parse_tagged("A3 OK [APPENDUID 42 100] append completed")
            .unwrap()
            .unwrap();
        assert_eq!(r.tag, "A3");
        assert!(r.is_ok());
        assert_eq!(
            r.code,
            Some(ResponseCode::AppendUid {
                uid_validity: 42,
                uid: 100
            })
        );
    }

    #[test]
    fn tagged_no_is_error_signal() {
        let r = parse_tagged("A4 NO [TRYCREATE] mailbox not found")
            .unwrap()
            .unwrap();
        assert_eq!(r.status, Status::No);
        assert_eq!(r.code, Some(ResponseCode::TryCreate));
    }

    #[test]
    fn untagged_list_special_use() {
        let u = parse_untagged(r#"* LIST (\HasNoChildren \Sent) "/" "Sent Items""#).unwrap();
        match u {
            Untagged::List(e) => {
                assert_eq!(e.name, "Sent Items");
                assert_eq!(e.role, MailboxRole::Sent);
                assert_eq!(e.delimiter, Some('/'));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn untagged_list_utf7_name_decoded() {
        let u = parse_untagged("* LIST () \"/\" \"&XfJT0ZAB-\"").unwrap();
        match u {
            Untagged::List(e) => assert_eq!(e.name, "已发送"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn untagged_fetch_with_envelope() {
        let line = r#"* 5 FETCH (UID 300 FLAGS (\Seen \Flagged) RFC822.SIZE 2048 MODSEQ (77) INTERNALDATE "13-Nov-2026 10:00:00 +0800" ENVELOPE ("Fri, 13 Nov 2026 10:00:00 +0800" "Hello" (("Alice" NIL "alice" "example.com")) NIL NIL (("Bob" NIL "bob" "example.com")) NIL NIL NIL "<msg1@example.com>"))"#;
        match parse_untagged(line).unwrap() {
            Untagged::Fetch(f) => {
                assert_eq!(f.seq, 5);
                assert_eq!(f.uid, Some(300));
                assert_eq!(f.size, Some(2048));
                assert_eq!(f.modseq, Some(77));
                assert_eq!(f.flags.len(), 2);
                let env = f.envelope.unwrap();
                assert_eq!(env.subject.as_deref(), Some("Hello"));
                assert_eq!(env.from[0].email, "alice@example.com");
                assert_eq!(env.from[0].name.as_deref(), Some("Alice"));
                assert_eq!(env.to[0].email, "bob@example.com");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn untagged_status_items() {
        let u = parse_untagged(r#"* STATUS "INBOX" (MESSAGES 12 UNSEEN 3 UIDNEXT 40)"#).unwrap();
        match u {
            Untagged::Status { mailbox, items } => {
                assert_eq!(mailbox, "INBOX");
                assert_eq!(items["MESSAGES"], 12);
                assert_eq!(items["UIDNEXT"], 40);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn untagged_capability_line() {
        match parse_untagged("* CAPABILITY IMAP4rev1 UIDPLUS MOVE").unwrap() {
            Untagged::Capability(c) => {
                assert!(c.supports_uidplus());
                assert!(c.supports_move());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn uidvalidity_and_highestmodseq_codes() {
        match parse_response_code("OK [UIDVALIDITY 9] done") {
            Some(ResponseCode::UidValidity(9)) => {}
            other => panic!("unexpected {other:?}"),
        }
        match parse_response_code("OK [HIGHESTMODSEQ 1024] done") {
            Some(ResponseCode::HighestModSeq(1024)) => {}
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn copyuid_pairs() {
        match parse_response_code("OK [COPYUID 5 1:2 10:11] copied") {
            Some(ResponseCode::CopyUid { src, dst, .. }) => {
                assert_eq!(UidRange::pair(&src, &dst), vec![(1, 10), (2, 11)]);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn missing_mailbox_in_status_errors() {
        assert!(parse_untagged("* STATUS").is_err());
    }
}
