//! 统一领域模型（需求 A-002）：屏蔽 JMAP / IMAP 协议差异。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub type Id = String;

/// 协议类型，账号级别确定，上层业务不感知（A-001）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolKind {
    Jmap,
    Imap,
}

/// 传输安全模式：不提供明文选项（SEC-001 / I-001）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityMode {
    /// 993 / 465 隐式 TLS
    ImplicitTls,
    /// 143 / 587 STARTTLS
    StartTls,
}

/// 凭证引用：只保存平台安全存储的别名，凭证永不明文落盘（ACC-005 / SEC-005）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Credential {
    Keyring { alias: String },
    OAuth2 { alias: String, refresh_url: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JmapConfig {
    pub endpoint: String,
    pub credential: Credential,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub security: SecurityMode,
    pub credential: Credential,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    pub security: SecurityMode,
    pub credential: Credential,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: Id,
    pub email_address: String,
    pub display_name: Option<String>,
    pub protocol: ProtocolKind,
    pub jmap: Option<JmapConfig>,
    pub imap: Option<ImapConfig>,
    pub smtp: Option<SmtpConfig>,
    /// 本地缓存天数，默认 30（M-008）
    pub sync_days: u32,
    pub poll_interval_secs: u32,
    pub created_at: DateTime<Utc>,
}

impl Account {
    pub fn new(
        id: impl Into<Id>,
        email_address: impl Into<String>,
        protocol: ProtocolKind,
    ) -> Self {
        Self {
            id: id.into(),
            email_address: email_address.into(),
            display_name: None,
            protocol,
            jmap: None,
            imap: None,
            smtp: None,
            sync_days: 30,
            poll_interval_secs: 300,
            created_at: Utc::now(),
        }
    }
}

/// 收/发件人地址。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

impl Address {
    pub fn new(email: impl Into<String>) -> Self {
        Self {
            name: None,
            email: email.into(),
        }
    }

    pub fn with_name(name: impl Into<String>, email: impl Into<String>) -> Self {
        Self {
            name: Some(name.into()),
            email: email.into(),
        }
    }

    /// 发件人真实域名（SEC-004 钓鱼防护用）。
    pub fn domain(&self) -> &str {
        self.email.rsplit_once('@').map(|(_, d)| d).unwrap_or("")
    }

    pub fn is_valid(&self) -> bool {
        let e = self.email.trim();
        match e.split_once('@') {
            Some((local, domain)) => {
                !local.is_empty() && domain.contains('.') && !domain.contains(' ')
            }
            None => false,
        }
    }
}

/// 统一标记：JMAP keyword 与 IMAP flag 双向映射（J-007 / I-006）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Flag {
    Seen,
    Flagged,
    Answered,
    Draft,
    Deleted,
    /// 自定义关键词 / Gmail 标签
    Custom(String),
}

impl Flag {
    pub fn as_imap(&self) -> String {
        match self {
            Flag::Seen => "\\Seen".into(),
            Flag::Flagged => "\\Flagged".into(),
            Flag::Answered => "\\Answered".into(),
            Flag::Draft => "\\Draft".into(),
            Flag::Deleted => "\\Deleted".into(),
            Flag::Custom(s) => s.clone(),
        }
    }

    pub fn from_imap(s: &str) -> Option<Flag> {
        match s.to_ascii_lowercase().as_str() {
            "\\seen" => Some(Flag::Seen),
            "\\flagged" => Some(Flag::Flagged),
            "\\answered" => Some(Flag::Answered),
            "\\draft" => Some(Flag::Draft),
            "\\deleted" => Some(Flag::Deleted),
            _ if s.is_empty() => None,
            _ => Some(Flag::Custom(s.to_string())),
        }
    }

    pub fn as_jmap_keyword(&self) -> String {
        match self {
            Flag::Seen => "$seen".into(),
            Flag::Flagged => "$flagged".into(),
            Flag::Answered => "$answered".into(),
            Flag::Draft => "$draft".into(),
            Flag::Deleted => "$deleted".into(),
            Flag::Custom(s) => s.clone(),
        }
    }

    pub fn from_jmap_keyword(k: &str) -> Option<Flag> {
        match k.to_ascii_lowercase().as_str() {
            "$seen" => Some(Flag::Seen),
            "$flagged" => Some(Flag::Flagged),
            "$answered" => Some(Flag::Answered),
            "$draft" => Some(Flag::Draft),
            "$deleted" => Some(Flag::Deleted),
            _ if k.is_empty() || k.starts_with('$') => None,
            _ => Some(Flag::Custom(k.to_string())),
        }
    }
}

/// 特殊用途文件夹（J-003 / I-003）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MailboxRole {
    Inbox,
    Sent,
    Trash,
    Archive,
    Drafts,
    Junk,
    Other,
}

impl MailboxRole {
    /// IMAP SPECIAL-USE / LIST 扩展属性 -> 统一角色
    pub fn from_special_use(flag: &str) -> Self {
        match flag.trim().trim_matches('\\').to_ascii_lowercase().as_str() {
            "inbox" => MailboxRole::Inbox,
            "sent" => MailboxRole::Sent,
            "trash" => MailboxRole::Trash,
            "archive" | "all" | "allmail" => MailboxRole::Archive,
            "drafts" => MailboxRole::Drafts,
            "junk" | "spam" => MailboxRole::Junk,
            _ => MailboxRole::Other,
        }
    }

    /// JMAP mailbox.role -> 统一角色
    pub fn from_jmap_role(role: &str) -> Self {
        match role.to_ascii_lowercase().as_str() {
            "inbox" => MailboxRole::Inbox,
            "sent" => MailboxRole::Sent,
            "trash" => MailboxRole::Trash,
            "archive" => MailboxRole::Archive,
            "drafts" => MailboxRole::Drafts,
            "junk" => MailboxRole::Junk,
            _ => MailboxRole::Other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mailbox {
    pub id: Id,
    pub account_id: Id,
    pub name: String,
    pub parent_id: Option<Id>,
    pub role: MailboxRole,
    pub total: u64,
    pub unread: u64,
    /// IMAP UIDVALIDITY：变化时必须全量校准
    pub uid_validity: Option<u32>,
    /// CONDSTORE / JMAP state：增量同步游标
    pub highest_modseq: Option<u64>,
    pub state: Option<String>,
}

impl Mailbox {
    pub fn new(id: impl Into<Id>, account_id: impl Into<Id>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            account_id: account_id.into(),
            name: name.into(),
            parent_id: None,
            role: MailboxRole::Other,
            total: 0,
            unread: 0,
            uid_validity: None,
            highest_modseq: None,
            state: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: Id,
    pub name: Option<String>,
    pub content_type: String,
    pub size: u64,
    /// JMAP blobId / IMAP part-id，按需拉取（J-009 / I-005）
    pub blob_id: Option<String>,
    pub part_id: Option<String>,
    pub inline: bool,
    pub cid: Option<String>,
}

impl Attachment {
    pub fn new(id: impl Into<Id>, content_type: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            content_type: content_type.into(),
            size: 0,
            blob_id: None,
            part_id: None,
            inline: false,
            cid: None,
        }
    }
}

/// 统一邮件实体：JMAP Email 与 IMAP ENVELOPE/BODYSTRUCTURE 归一。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Email {
    pub id: Id,
    pub account_id: Id,
    pub mailbox_ids: Vec<Id>,
    pub thread_id: Option<Id>,
    pub subject: String,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub reply_to: Vec<Address>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub sent_at: Option<DateTime<Utc>>,
    pub received_at: DateTime<Utc>,
    pub preview: String,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub attachments: Vec<Attachment>,
    pub flags: Vec<Flag>,
    pub size: u64,
    /// 协议侧状态游标，用于 sinceState / MODSEQ 增量（J-004 / I-004）
    pub state: Option<String>,
    pub modseq: Option<u64>,
    pub uid: Option<u32>,
    pub blob_id: Option<String>,
    pub raw_headers: BTreeMap<String, String>,
}

impl Email {
    pub fn new(id: impl Into<Id>, account_id: impl Into<Id>, mailbox_id: impl Into<Id>) -> Self {
        Self {
            id: id.into(),
            account_id: account_id.into(),
            mailbox_ids: vec![mailbox_id.into()],
            thread_id: None,
            subject: String::new(),
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            sent_at: None,
            received_at: Utc::now(),
            preview: String::new(),
            text_body: None,
            html_body: None,
            attachments: Vec::new(),
            flags: Vec::new(),
            size: 0,
            state: None,
            modseq: None,
            uid: None,
            blob_id: None,
            raw_headers: BTreeMap::new(),
        }
    }

    pub fn is_read(&self) -> bool {
        self.flags.contains(&Flag::Seen)
    }

    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
    }

    /// 幂等标记写入：重复同步不产生重复标记（A-005）
    pub fn set_flag(&mut self, flag: Flag, enabled: bool) {
        let set: BTreeSet<Flag> = self.flags.iter().cloned().collect();
        let has = set.contains(&flag);
        if enabled && !has {
            self.flags.push(flag);
        } else if !enabled && has {
            self.flags.retain(|f| f != &flag);
        }
    }

    pub fn refresh_preview(&mut self) {
        let src = self.text_body.as_deref().unwrap_or("");
        let clean: String = src.chars().take(200).collect();
        self.preview = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    }
}

/// 待发送邮件（M-003 / J-006 / I-009）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub account_id: Id,
    pub from: Vec<Address>,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub attachments: Vec<OutgoingAttachment>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// 保存草稿后发送时使用
    pub draft_id: Option<Id>,
}

impl OutgoingMessage {
    pub fn new(
        account_id: impl Into<Id>,
        from: Address,
        to: Vec<Address>,
        subject: impl Into<String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            from: vec![from],
            to,
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.into(),
            text_body: None,
            html_body: None,
            attachments: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            draft_id: None,
        }
    }

    pub fn recipients(&self) -> Vec<&Address> {
        self.to
            .iter()
            .chain(self.cc.iter())
            .chain(self.bcc.iter())
            .collect()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.from.len() != 1 || !self.from[0].is_valid() {
            return Err("发件人必须且只能有一个有效地址".into());
        }
        if self.recipients().is_empty() {
            return Err("至少需要一个收件人".into());
        }
        if let Some(bad) = self.recipients().iter().find(|a| !a.is_valid()) {
            return Err(format!("收件人地址无效: {}", bad.email));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingAttachment {
    pub name: String,
    pub content_type: String,
    pub data: Vec<u8>,
}
