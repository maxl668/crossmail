//! JMAP JSON -> 统一领域模型（J-005 / J-007）。
//!
//! 所有解析函数都是纯函数：不触发网络、不依赖全局状态，便于覆盖率测试。

use chrono::{DateTime, Utc};
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{
    Address, Attachment, Email, Flag, Mailbox, MailboxRole, OutgoingMessage,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

/// 发送身份（RFC 8621 §6 `Identity`）。
///
/// `id` 是服务端生成的**身份标识**，与邮箱地址毫无关系：真机 Stalwart 上
/// `admin@example.com` 对应的身份 id 就是 `"b"`。`EmailSubmission/set` 的
/// `identityId` 只认这个 id，填邮箱地址会得到
/// `notCreated[sub]={type:"invalidProperties",properties:["identityId"]}`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub id: String,
    pub name: String,
    pub email: String,
    pub reply_to: Option<Address>,
    pub may_delete: bool,
}

pub fn parse_identity(v: &Value) -> Option<Identity> {
    let id = str_field(v, "id")?.to_string();
    Some(Identity {
        id,
        name: str_field(v, "name").unwrap_or("").to_string(),
        email: str_field(v, "email").unwrap_or("").trim().to_string(),
        reply_to: parse_addresses(v.get("replyTo")).into_iter().next(),
        may_delete: v.get("mayDelete").and_then(Value::as_bool).unwrap_or(false),
    })
}

pub fn parse_identities(list: &[Value]) -> Vec<Identity> {
    list.iter().filter_map(parse_identity).collect()
}

/// 已上传的附件引用，用于 Email/set create。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmailBlob {
    pub blob_id: String,
    pub name: String,
    pub content_type: String,
    pub size: u64,
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn require_str(v: &Value, key: &str) -> MailResult<String> {
    str_field(v, key)
        .map(str::to_string)
        .ok_or_else(|| MailError::Parse(format!("JMAP 对象缺少字符串字段 {key}")))
}

pub fn parse_datetime(s: Option<&str>) -> Option<DateTime<Utc>> {
    s.and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|d| d.with_timezone(&Utc))
}

pub fn parse_addresses(v: Option<&Value>) -> Vec<Address> {
    v.and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| {
                    str_field(item, "email").map(|email| Address {
                        name: str_field(item, "name").map(str::to_string),
                        email: email.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 形如 `{"$seen": true, "custom": true}` 的 keywords -> 统一标记。
pub fn parse_keywords(v: &Value) -> Vec<Flag> {
    let mut flags: BTreeSet<Flag> = BTreeSet::new();
    if let Some(map) = v.get("keywords").and_then(Value::as_object) {
        for (key, enabled) in map {
            if enabled.as_bool().unwrap_or(false) {
                if let Some(flag) = Flag::from_jmap_keyword(key) {
                    flags.insert(flag);
                }
            }
        }
    }
    flags.into_iter().collect()
}

/// 形如 `{"M1": true, "M2": false}` 的 mailboxIds -> 有效文件夹 id 列表。
pub fn parse_mailbox_ids(v: &Value) -> Vec<String> {
    v.get("mailboxIds")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter(|(_, val)| val.as_bool().unwrap_or(false))
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

pub fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn first_string(v: Option<&Value>) -> Option<String> {
    string_list(v).into_iter().next()
}

/// 从 bodyValues 按 textBody/htmlBody 的 partId 顺序拼正文（J-005）。
/// 未请求 bodyValues 时返回 None，表示「仅元数据，正文按需拉取」。
pub fn body_value(v: &Value, parts_key: &str) -> Option<String> {
    let parts = v.get(parts_key).and_then(Value::as_array)?;
    let values = v.get("bodyValues").and_then(Value::as_object)?;
    let mut out = String::new();
    for part in parts {
        let Some(part_id) = str_field(part, "partId") else {
            continue;
        };
        if let Some(text) = values
            .get(part_id)
            .and_then(|b| b.get("value"))
            .and_then(Value::as_str)
        {
            out.push_str(text);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

pub fn parse_attachments(v: &Value) -> Vec<Attachment> {
    v.get("attachments")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .enumerate()
                .map(|(idx, a)| {
                    let blob_id = str_field(a, "blobId").map(str::to_string);
                    let part_id = str_field(a, "partId").map(str::to_string);
                    let id = blob_id
                        .clone()
                        .or_else(|| part_id.clone())
                        .unwrap_or_else(|| format!("att-{idx}"));
                    let inline = str_field(a, "disposition") == Some("inline")
                        || a.get("isInline").and_then(Value::as_bool).unwrap_or(false);
                    Attachment {
                        id,
                        name: str_field(a, "name").map(str::to_string),
                        content_type: str_field(a, "type")
                            .unwrap_or("application/octet-stream")
                            .to_string(),
                        size: a.get("size").and_then(Value::as_u64).unwrap_or(0),
                        blob_id,
                        part_id,
                        inline,
                        cid: str_field(a, "cid").map(str::to_string),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_headers(v: &Value) -> BTreeMap<String, String> {
    v.get("headers")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|h| {
                    let name = str_field(h, "name")?;
                    let value = str_field(h, "value")?;
                    Some((name.to_ascii_lowercase(), value.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn parse_email(v: &Value, account_id: &str) -> MailResult<Email> {
    let id = require_str(v, "id")?;
    let mailbox_ids = parse_mailbox_ids(v);
    let first = mailbox_ids.first().cloned().unwrap_or_default();
    let mut email = Email::new(id, account_id, first);
    email.mailbox_ids = mailbox_ids;
    email.thread_id = str_field(v, "threadId").map(str::to_string);
    email.subject = str_field(v, "subject").unwrap_or("").to_string();
    email.from = parse_addresses(v.get("from"));
    email.to = parse_addresses(v.get("to"));
    email.cc = parse_addresses(v.get("cc"));
    email.bcc = parse_addresses(v.get("bcc"));
    email.reply_to = parse_addresses(v.get("replyTo"));
    email.message_id = first_string(v.get("messageId"));
    email.in_reply_to = first_string(v.get("inReplyTo"));
    email.references = string_list(v.get("references"));
    email.sent_at = parse_datetime(str_field(v, "sentAt"));
    email.received_at = parse_datetime(str_field(v, "receivedAt")).unwrap_or_else(Utc::now);
    email.preview = str_field(v, "preview").unwrap_or("").to_string();
    email.text_body = body_value(v, "textBody");
    email.html_body = body_value(v, "htmlBody");
    email.attachments = parse_attachments(v);
    email.flags = parse_keywords(v);
    email.size = v.get("size").and_then(Value::as_u64).unwrap_or(0);
    email.blob_id = str_field(v, "blobId").map(str::to_string);
    email.raw_headers = parse_headers(v);
    if email.preview.is_empty() {
        email.refresh_preview();
    }
    Ok(email)
}

pub fn parse_mailbox(v: &Value, account_id: &str) -> MailResult<Mailbox> {
    let id = require_str(v, "id")?;
    let mut mailbox = Mailbox::new(id, account_id, str_field(v, "name").unwrap_or(""));
    mailbox.parent_id = str_field(v, "parentId").map(str::to_string);
    mailbox.role = str_field(v, "role")
        .map(MailboxRole::from_jmap_role)
        .unwrap_or(MailboxRole::Other);
    mailbox.total = v.get("totalEmails").and_then(Value::as_u64).unwrap_or(0);
    mailbox.unread = v.get("unreadEmails").and_then(Value::as_u64).unwrap_or(0);
    Ok(mailbox)
}

pub fn addresses_to_json(list: &[Address]) -> Value {
    Value::Array(
        list.iter()
            .map(|a| {
                let mut obj = json!({ "email": a.email });
                if let Some(name) = &a.name {
                    obj["name"] = json!(name);
                }
                obj
            })
            .collect(),
    )
}

/// OutgoingMessage -> JMAP Email/create 对象（J-006 草稿落盘用）。
/// `message_id` 由上层生成后传入，保证纯函数可测。
pub fn build_email_create(
    msg: &OutgoingMessage,
    draft_mailbox_id: &str,
    message_id: &str,
    blobs: &[EmailBlob],
) -> Value {
    let mut body_values = serde_json::Map::new();
    let mut text_parts = Vec::new();
    let mut html_parts = Vec::new();

    if let Some(text) = &msg.text_body {
        body_values.insert(
            "body".into(),
            json!({ "value": text, "charset": "utf-8", "type": "text/plain" }),
        );
        text_parts.push(json!({ "partId": "body", "type": "text/plain" }));
    }
    if let Some(html) = &msg.html_body {
        body_values.insert(
            "html".into(),
            json!({ "value": html, "charset": "utf-8", "type": "text/html" }),
        );
        html_parts.push(json!({ "partId": "html", "type": "text/html" }));
    }

    let attachments: Vec<Value> = blobs
        .iter()
        .map(|b| {
            json!({
                "blobId": b.blob_id,
                "type": b.content_type,
                "name": b.name,
                "size": b.size,
                "disposition": "attachment"
            })
        })
        .collect();

    let in_reply_to: Vec<&String> = msg.in_reply_to.iter().collect();
    json!({
        "mailboxIds": { (draft_mailbox_id): true },
        "keywords": { "$draft": true },
        "from": addresses_to_json(&msg.from),
        "to": addresses_to_json(&msg.to),
        "cc": addresses_to_json(&msg.cc),
        "bcc": addresses_to_json(&msg.bcc),
        "subject": msg.subject,
        "messageId": [message_id],
        "inReplyTo": in_reply_to,
        "references": msg.references,
        "bodyValues": body_values,
        "textBody": text_parts,
        "htmlBody": html_parts,
        "attachments": attachments
    })
}
