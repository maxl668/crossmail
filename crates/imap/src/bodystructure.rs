//! BODYSTRUCTURE 解析（需求 I-005）：按需拉取正文/附件的前提。

use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::Attachment;

use crate::atom::Atom;

/// BODYSTRUCTURE 树节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyPart {
    /// IMAP part-id，如 "1"、"2.1"、"TEXT"
    pub part_id: String,
    /// 小写 `type/subtype`
    pub content_type: String,
    pub charset: Option<String>,
    pub name: Option<String>,
    pub content_id: Option<String>,
    /// base64 / quoted-printable / 7bit
    pub encoding: String,
    pub size: u64,
    pub lines: Option<u64>,
    pub disposition: Option<String>,
    pub children: Vec<BodyPart>,
}

impl BodyPart {
    pub fn parse(atom: &Atom) -> MailResult<BodyPart> {
        Self::parse_inner(atom, "")
    }

    fn parse_inner(atom: &Atom, part_id: &str) -> MailResult<BodyPart> {
        let items = atom
            .as_list()
            .ok_or_else(|| MailError::Parse("BODYSTRUCTURE 不是列表".into()))?;
        if items.is_empty() {
            return Err(MailError::Parse("BODYSTRUCTURE 为空".into()));
        }

        // multipart：子部件全部是 List，第一个非 List 原子是子类型
        if matches!(items[0], Atom::List(_)) {
            let split = items.iter().position(|a| !matches!(a, Atom::List(_)));
            let (bodies, subtype) = match split {
                Some(pos) => (&items[..pos], items[pos].as_str().unwrap_or("mixed")),
                None => (items, "mixed"),
            };
            let mut children = Vec::with_capacity(bodies.len());
            for (idx, body) in bodies.iter().enumerate() {
                let child_id = if part_id.is_empty() {
                    (idx + 1).to_string()
                } else {
                    format!("{part_id}.{}", idx + 1)
                };
                children.push(Self::parse_inner(body, &child_id)?);
            }
            return Ok(BodyPart {
                part_id: if part_id.is_empty() {
                    "TEXT".to_string()
                } else {
                    part_id.to_string()
                },
                content_type: format!("multipart/{}", subtype.to_ascii_lowercase()),
                charset: None,
                name: None,
                content_id: None,
                encoding: "7bit".into(),
                size: children.iter().map(|c| c.size).sum(),
                lines: None,
                disposition: None,
                children,
            });
        }

        let major = items[0].as_str().unwrap_or("application").to_string();
        let minor = items
            .get(1)
            .and_then(|a| a.as_str())
            .unwrap_or("octet-stream")
            .to_string();
        let content_type = format!(
            "{}/{}",
            major.to_ascii_lowercase(),
            minor.to_ascii_lowercase()
        );

        let params = items.get(2).cloned().unwrap_or(Atom::Nil);
        let charset = lookup_param(&params, "charset");
        let mut name = lookup_param(&params, "name");
        let content_id = items.get(3).and_then(|a| a.text());
        let encoding = items
            .get(5)
            .and_then(|a| a.as_str())
            .unwrap_or("7bit")
            .to_ascii_lowercase();
        let size = items.get(6).and_then(|a| a.as_u64()).unwrap_or(0);

        let mut disposition = None;
        let mut lines = None;
        let mut children = Vec::new();

        // TEXT 的 body-fld-lines
        if content_type.starts_with("text/") {
            lines = items.get(7).and_then(|a| a.as_u64());
        } else if content_type == "message/rfc822" {
            lines = items.get(9).and_then(|a| a.as_u64());
            if let Some(embedded) = items.get(8) {
                if embedded.as_list().is_some() {
                    // 顶层 message/rfc822 自身即 part "1"，内嵌部件顺延为 "1.1"
                    let parent_id = if part_id.is_empty() { "1" } else { part_id };
                    children.push(Self::parse_inner(embedded, &format!("{parent_id}.1"))?);
                }
            }
        }

        // 扩展区：MD5、disposition、language、location —— 扫描任意列表定位 disposition
        for item in items.iter().skip(3) {
            if let Some(list) = item.as_list() {
                if let Some(first) = list.first().and_then(|a| a.as_str()) {
                    let low = first.to_ascii_lowercase();
                    if low == "attachment" || low == "inline" {
                        disposition = Some(low);
                        if name.is_none() {
                            if let Some(dparams) = list.get(1) {
                                name = lookup_param(dparams, "filename");
                            }
                        }
                    }
                }
            }
        }

        Ok(BodyPart {
            part_id: if part_id.is_empty() {
                "1".to_string()
            } else {
                part_id.to_string()
            },
            content_type,
            charset,
            name,
            content_id,
            encoding,
            size,
            lines,
            disposition,
            children,
        })
    }

    pub fn is_multipart(&self) -> bool {
        self.content_type.starts_with("multipart/")
    }

    /// 附件判定：显式 disposition=attachment，或带文件名且非正文类型。
    pub fn is_attachment(&self) -> bool {
        if self.is_multipart() {
            return false;
        }
        match self.disposition.as_deref() {
            Some("attachment") => true,
            Some("inline") => false,
            _ => {
                self.name.is_some()
                    && !(self.content_type.starts_with("text/plain")
                        || self.content_type.starts_with("text/html"))
            }
        }
    }

    /// 深度优先遍历所有叶子节点。
    pub fn leaves(&self) -> Vec<&BodyPart> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves<'a>(&'a self, out: &mut Vec<&'a BodyPart>) {
        if self.children.is_empty() {
            out.push(self);
        } else {
            for c in &self.children {
                c.collect_leaves(out);
            }
        }
    }

    /// 选择正文部件：优先 HTML，其次纯文本。
    pub fn find_body(&self, prefer_html: bool) -> Option<&BodyPart> {
        let leaves: Vec<&BodyPart> = self
            .leaves()
            .into_iter()
            .filter(|p| !p.is_attachment())
            .collect();
        let want = if prefer_html {
            "text/html"
        } else {
            "text/plain"
        };
        leaves
            .iter()
            .find(|p| p.content_type == want)
            .or_else(|| leaves.iter().find(|p| p.content_type.starts_with("text/")))
            .copied()
    }

    /// 转为统一附件列表（I-005 / M-007）。
    pub fn to_attachments(&self) -> Vec<Attachment> {
        self.leaves()
            .into_iter()
            .filter(|p| p.is_attachment())
            .enumerate()
            .map(|(idx, p)| {
                let mut a = Attachment::new(format!("att{idx}"), p.content_type.clone());
                a.name = p.name.clone();
                a.size = p.size;
                a.part_id = Some(p.part_id.clone());
                a.inline = p.disposition.as_deref() == Some("inline");
                a.cid = p.content_id.clone();
                a
            })
            .collect()
    }
}

fn lookup_param(params: &Atom, key: &str) -> Option<String> {
    let items = params.as_list()?;
    let mut i = 0;
    while i + 1 < items.len() {
        let k = items[i].as_str().unwrap_or("").to_ascii_lowercase();
        if k == key {
            return items[i + 1].text();
        }
        i += 2;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atom::parse_atoms;

    fn body(src: &str) -> BodyPart {
        let atoms = parse_atoms(src).unwrap();
        BodyPart::parse(&atoms[0]).unwrap()
    }

    #[test]
    fn single_text_part() {
        let p = body(r#"("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" 320 12 NIL NIL NIL)"#);
        assert_eq!(p.content_type, "text/plain");
        assert_eq!(p.charset.as_deref(), Some("UTF-8"));
        assert_eq!(p.size, 320);
        assert_eq!(p.lines, Some(12));
        assert!(!p.is_attachment());
    }

    #[test]
    fn multipart_alternative_with_attachment() {
        let p = body(
            r#"(("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" 100 3 NIL NIL NIL)(("TEXT" "HTML" ("CHARSET" "UTF-8") NIL NIL "7BIT" 200 5 NIL NIL NIL)("APPLICATION" "PDF" ("NAME" "a.pdf") NIL NIL "BASE64" 900 NIL ("ATTACHMENT" ("FILENAME" "a.pdf")) NIL) "ALTERNATIVE") "MIXED" ("BOUNDARY" "b1") NIL NIL NIL)"#,
        );
        assert_eq!(p.content_type, "multipart/mixed");
        assert_eq!(p.children.len(), 2);
        assert_eq!(p.children[1].content_type, "multipart/alternative");

        let html = p.find_body(true).unwrap();
        assert_eq!(html.content_type, "text/html");
        assert_eq!(html.part_id, "2.1");

        let atts = p.to_attachments();
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].name.as_deref(), Some("a.pdf"));
        assert_eq!(atts[0].part_id.as_deref(), Some("2.2"));
    }

    #[test]
    fn inline_image_not_attachment() {
        let p = body(
            r#"(("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" 10 1 NIL NIL NIL)("IMAGE" "PNG" ("NAME" "logo.png") "<cid1>" NIL "BASE64" 50 NIL ("INLINE" ("FILENAME" "logo.png")) NIL) "RELATED" NIL NIL NIL NIL)"#,
        );
        let atts = p.to_attachments();
        assert!(atts.is_empty(), "inline 图片不应当作附件");
        let leaf = p
            .leaves()
            .into_iter()
            .find(|x| x.content_type == "image/png")
            .unwrap();
        assert_eq!(leaf.content_id.as_deref(), Some("<cid1>"));
    }

    #[test]
    fn message_rfc822_embedded() {
        let p = body(
            r#"("MESSAGE" "RFC822" NIL NIL NIL "7BIT" 500 NIL ("TEXT" "PLAIN" ("CHARSET" "US-ASCII") NIL NIL "7BIT" 20 1 NIL NIL NIL) 15)"#,
        );
        assert_eq!(p.content_type, "message/rfc822");
        assert_eq!(p.children.len(), 1);
        assert_eq!(p.children[0].part_id, "1.1");
    }

    #[test]
    fn malformed_input_errors() {
        assert!(BodyPart::parse(&Atom::Word("X".into())).is_err());
    }
}
