//! FFI 数据结构与核心领域模型的双向转换（S5-01）。
//!
//! 为什么要有这层镜像：uniffi 只认它自己那套类型（UDL 里声明过、且 Rust 侧
//! 有同名同形定义），而 `crossmail-core` 的模型不该被绑定层绑死。转换集中放在
//! 这里，UI 侧拿到的是稳定的 DTO，换协议、改内部结构都不影响已发布的接口。
//!
//! 注意：这里的每个类型都**必须**与 `crossmail_ffi.udl` 的字段名、顺序、类型
//! 完全一致。对不上时 scaffolding 会在编译期直接报错——这正是我们想要的护栏。

use crossmail_core::model as core;
use crossmail_core::MailError;
use crossmail_service::{AccountPatch, Endpoint, NewAccount, ProtocolSetup};
use crossmail_storage::EmailQuery;
use std::time::SystemTime;

// ---------------------------------------------------------------- 枚举

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolKindDto {
    Jmap,
    Imap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityModeDto {
    ImplicitTls,
    StartTls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxRoleDto {
    Inbox,
    Sent,
    Trash,
    Archive,
    Drafts,
    Junk,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScopeDto {
    All,
    Subject,
    From,
    To,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagDto {
    Seen,
    Flagged,
    Answered,
    Draft,
    Deleted,
    Custom { value: String },
}

// ---------------------------------------------------------------- 错误

/// 统一错误：每个变体自带人类可读文本（用户提示 + 技术细节）。
#[derive(Debug, Clone, thiserror::Error)]
pub enum CrossMailError {
    #[error("{message}")]
    InvalidInput { message: String },
    #[error("{message}")]
    Authentication { message: String },
    #[error("{message}")]
    Network { message: String },
    #[error("{message}")]
    Protocol { message: String },
    #[error("{message}")]
    Quota { message: String },
    #[error("{message}")]
    Conflict { message: String },
    #[error("{message}")]
    NotFound { message: String },
    #[error("{message}")]
    Storage { message: String },
    #[error("{message}")]
    Cancelled { message: String },
    #[error("{message}")]
    Unsupported { message: String },
    #[error("{message}")]
    RateLimited { message: String },
    #[error("{message}")]
    Tls { message: String },
    #[error("{message}")]
    Parse { message: String },
    #[error("{message}")]
    Server { message: String },
}

impl From<MailError> for CrossMailError {
    /// 把内部错误收敛成 UI 可直接展示的形态：`用户提示：技术细节`。
    /// 分类保持一一对应，UI 想精确分派时仍能拿到具体变体。
    fn from(e: MailError) -> Self {
        let hint = e.user_hint();
        let detail = e.to_string();
        let message = if detail.is_empty() || detail == hint {
            hint.to_string()
        } else {
            format!("{hint}：{detail}")
        };
        match e.kind() {
            crossmail_core::ErrorKind::InvalidInput => Self::InvalidInput { message },
            crossmail_core::ErrorKind::Authentication => Self::Authentication { message },
            crossmail_core::ErrorKind::Network => Self::Network { message },
            crossmail_core::ErrorKind::Protocol => Self::Protocol { message },
            crossmail_core::ErrorKind::Quota => Self::Quota { message },
            crossmail_core::ErrorKind::Conflict => Self::Conflict { message },
            crossmail_core::ErrorKind::NotFound => Self::NotFound { message },
            crossmail_core::ErrorKind::Storage => Self::Storage { message },
            crossmail_core::ErrorKind::Cancelled => Self::Cancelled { message },
            crossmail_core::ErrorKind::Unsupported => Self::Unsupported { message },
            crossmail_core::ErrorKind::RateLimited => Self::RateLimited { message },
            crossmail_core::ErrorKind::Tls => Self::Tls { message },
            crossmail_core::ErrorKind::Parse => Self::Parse { message },
            crossmail_core::ErrorKind::Server => Self::Server { message },
        }
    }
}

pub type FfiResult<T> = Result<T, CrossMailError>;

// ---------------------------------------------------------------- 配置

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub db_path: String,
    pub cache_dir: String,
    pub cache_quota_bytes: u64,
    pub master_key_b64: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CoreStatus {
    pub version: String,
    pub supported_protocols: Vec<String>,
}

// ---------------------------------------------------------------- 数据结构

#[derive(Debug, Clone)]
pub struct AddressDto {
    pub name: Option<String>,
    pub email: String,
}

#[derive(Debug, Clone)]
pub struct AttachmentDto {
    pub id: String,
    pub file_name: Option<String>,
    pub content_type: String,
    pub size: u64,
    pub is_inline: bool,
    pub content_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EmailDto {
    pub id: String,
    pub account_id: String,
    pub mailbox_ids: Vec<String>,
    pub thread_id: Option<String>,
    pub subject: String,
    pub from: Vec<AddressDto>,
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub bcc: Vec<AddressDto>,
    pub reply_to: Vec<AddressDto>,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub sent_at: Option<SystemTime>,
    pub received_at: SystemTime,
    pub preview: String,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub attachments: Vec<AttachmentDto>,
    pub flags: Vec<FlagDto>,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct EmailQueryDto {
    pub mailbox_id: Option<String>,
    pub unread_only: bool,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Debug, Clone)]
pub struct EmailPageDto {
    pub items: Vec<EmailDto>,
    pub total: u32,
    pub next_offset: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct MailboxDto {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub role: MailboxRoleDto,
    pub total: u64,
    pub unread: u64,
}

#[derive(Debug, Clone)]
pub struct MailboxCountDto {
    pub mailbox_id: String,
    pub unread: u64,
}

#[derive(Debug, Clone)]
pub struct AccountDto {
    pub id: String,
    pub email_address: String,
    pub display_name: Option<String>,
    pub protocol: ProtocolKindDto,
    pub jmap_endpoint: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<SecurityModeDto>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SecurityModeDto>,
    pub sync_days: u32,
    pub poll_interval_secs: u32,
}

#[derive(Debug, Clone)]
pub struct NewAccountDto {
    pub email_address: String,
    pub display_name: Option<String>,
    pub protocol: ProtocolKindDto,
    pub secret: String,
    pub jmap_endpoint: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<SecurityModeDto>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SecurityModeDto>,
    pub sync_days: Option<u32>,
    pub poll_interval_secs: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct AccountPatchDto {
    pub display_name: Option<String>,
    pub clear_display_name: bool,
    pub sync_days: Option<u32>,
    pub poll_interval_secs: Option<u32>,
    pub jmap_endpoint: Option<String>,
    pub imap_host: Option<String>,
    pub imap_port: Option<u16>,
    pub imap_security: Option<SecurityModeDto>,
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<SecurityModeDto>,
    pub clear_smtp: bool,
    pub secret: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DraftDto {
    pub to: Vec<AddressDto>,
    pub cc: Vec<AddressDto>,
    pub bcc: Vec<AddressDto>,
    pub subject: String,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SendReceiptDto {
    pub email_id: String,
    pub message_id: String,
    pub sent_at: SystemTime,
    pub sent_mailbox_id: String,
}

#[derive(Debug, Clone)]
pub struct DeleteOutcomeDto {
    pub email_id: String,
    pub trashed: bool,
}

#[derive(Debug, Clone)]
pub struct CacheStatusDto {
    pub usage_bytes: u64,
    pub quota_bytes: u64,
    pub over_quota: bool,
}

#[derive(Debug, Clone)]
pub struct EvictionReportDto {
    pub removed_count: u32,
    pub freed_bytes: u64,
    pub usage_after: u64,
    pub foreign_entries: u32,
}

// ---------------------------------------------------------------- 转换：core -> DTO

pub(crate) fn to_system_time(t: chrono::DateTime<chrono::Utc>) -> SystemTime {
    SystemTime::from(t)
}

pub(crate) fn address_to_dto(a: &core::Address) -> AddressDto {
    AddressDto {
        name: a.name.clone(),
        email: a.email.clone(),
    }
}

pub(crate) fn address_from_dto(a: &AddressDto) -> core::Address {
    core::Address {
        name: a.name.clone(),
        email: a.email.clone(),
    }
}

fn addresses_to_dto(v: &[core::Address]) -> Vec<AddressDto> {
    v.iter().map(address_to_dto).collect()
}

fn addresses_from_dto(v: &[AddressDto]) -> Vec<core::Address> {
    v.iter().map(address_from_dto).collect()
}

pub(crate) fn flag_to_dto(f: &core::Flag) -> FlagDto {
    match f {
        core::Flag::Seen => FlagDto::Seen,
        core::Flag::Flagged => FlagDto::Flagged,
        core::Flag::Answered => FlagDto::Answered,
        core::Flag::Draft => FlagDto::Draft,
        core::Flag::Deleted => FlagDto::Deleted,
        core::Flag::Custom(v) => FlagDto::Custom { value: v.clone() },
    }
}

pub(crate) fn role_to_dto(r: core::MailboxRole) -> MailboxRoleDto {
    match r {
        core::MailboxRole::Inbox => MailboxRoleDto::Inbox,
        core::MailboxRole::Sent => MailboxRoleDto::Sent,
        core::MailboxRole::Trash => MailboxRoleDto::Trash,
        core::MailboxRole::Archive => MailboxRoleDto::Archive,
        core::MailboxRole::Drafts => MailboxRoleDto::Drafts,
        core::MailboxRole::Junk => MailboxRoleDto::Junk,
        core::MailboxRole::Other => MailboxRoleDto::Other,
    }
}

pub(crate) fn protocol_to_dto(p: core::ProtocolKind) -> ProtocolKindDto {
    match p {
        core::ProtocolKind::Jmap => ProtocolKindDto::Jmap,
        core::ProtocolKind::Imap => ProtocolKindDto::Imap,
    }
}

fn security_to_dto(s: core::SecurityMode) -> SecurityModeDto {
    match s {
        core::SecurityMode::ImplicitTls => SecurityModeDto::ImplicitTls,
        core::SecurityMode::StartTls => SecurityModeDto::StartTls,
    }
}

fn security_from_dto(s: SecurityModeDto) -> core::SecurityMode {
    match s {
        SecurityModeDto::ImplicitTls => core::SecurityMode::ImplicitTls,
        SecurityModeDto::StartTls => core::SecurityMode::StartTls,
    }
}

pub(crate) fn attachment_to_dto(a: &core::Attachment) -> AttachmentDto {
    AttachmentDto {
        id: a.id.clone(),
        file_name: a.name.clone(),
        content_type: a.content_type.clone(),
        size: a.size,
        is_inline: a.inline,
        content_id: a.cid.clone(),
    }
}

pub(crate) fn email_to_dto(e: &core::Email) -> EmailDto {
    EmailDto {
        id: e.id.clone(),
        account_id: e.account_id.clone(),
        mailbox_ids: e.mailbox_ids.clone(),
        thread_id: e.thread_id.clone(),
        subject: e.subject.clone(),
        from: addresses_to_dto(&e.from),
        to: addresses_to_dto(&e.to),
        cc: addresses_to_dto(&e.cc),
        bcc: addresses_to_dto(&e.bcc),
        reply_to: addresses_to_dto(&e.reply_to),
        message_id: e.message_id.clone(),
        in_reply_to: e.in_reply_to.clone(),
        references: e.references.clone(),
        sent_at: e.sent_at.map(to_system_time),
        received_at: to_system_time(e.received_at),
        preview: e.preview.clone(),
        text_body: e.text_body.clone(),
        html_body: e.html_body.clone(),
        attachments: e.attachments.iter().map(attachment_to_dto).collect(),
        flags: e.flags.iter().map(flag_to_dto).collect(),
        size: e.size,
    }
}

pub(crate) fn mailbox_to_dto(m: &core::Mailbox) -> MailboxDto {
    MailboxDto {
        id: m.id.clone(),
        account_id: m.account_id.clone(),
        name: m.name.clone(),
        parent_id: m.parent_id.clone(),
        role: role_to_dto(m.role),
        total: m.total,
        unread: m.unread,
    }
}

pub(crate) fn account_to_dto(a: &core::Account) -> AccountDto {
    AccountDto {
        id: a.id.clone(),
        email_address: a.email_address.clone(),
        display_name: a.display_name.clone(),
        protocol: protocol_to_dto(a.protocol),
        jmap_endpoint: a.jmap.as_ref().map(|c| c.endpoint.clone()),
        imap_host: a.imap.as_ref().map(|c| c.host.clone()),
        imap_port: a.imap.as_ref().map(|c| c.port),
        imap_security: a.imap.as_ref().map(|c| security_to_dto(c.security)),
        smtp_host: a.smtp.as_ref().map(|c| c.host.clone()),
        smtp_port: a.smtp.as_ref().map(|c| c.port),
        smtp_security: a.smtp.as_ref().map(|c| security_to_dto(c.security)),
        sync_days: a.sync_days,
        poll_interval_secs: a.poll_interval_secs,
    }
}

// ---------------------------------------------------------------- 转换：DTO -> core

/// 端点三件套必须齐全；缺一个就让 UI 拿到明确的输入错误，而不是建出一个连不上的账号。
fn endpoint_from_dto(
    host: &Option<String>,
    port: Option<u16>,
    security: Option<SecurityModeDto>,
    label: &str,
) -> FfiResult<Endpoint> {
    let host = host
        .as_ref()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .ok_or_else(|| CrossMailError::InvalidInput {
            message: format!("{label} 主机不能为空"),
        })?;
    let port = port.ok_or_else(|| CrossMailError::InvalidInput {
        message: format!("{label} 端口不能为空"),
    })?;
    let security = security.ok_or_else(|| CrossMailError::InvalidInput {
        message: format!("{label} 加密方式不能为空"),
    })?;
    Ok(Endpoint::new(host, port, security_from_dto(security)))
}

/// 可选端点：三个字段都没填才算「未配置」，填了就必须填全。
fn optional_endpoint_from_dto(
    host: &Option<String>,
    port: Option<u16>,
    security: Option<SecurityModeDto>,
    label: &str,
) -> FfiResult<Option<Endpoint>> {
    let blank = host.as_ref().map(|h| h.trim().is_empty()).unwrap_or(true);
    if blank && port.is_none() && security.is_none() {
        return Ok(None);
    }
    Ok(Some(endpoint_from_dto(host, port, security, label)?))
}

pub(crate) fn new_account_from_dto(input: &NewAccountDto) -> FfiResult<NewAccount> {
    if input.secret.trim().is_empty() {
        return Err(CrossMailError::InvalidInput {
            message: "凭证不能为空".into(),
        });
    }
    let setup = match input.protocol {
        ProtocolKindDto::Jmap => {
            let endpoint = input
                .jmap_endpoint
                .as_ref()
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty())
                .ok_or_else(|| CrossMailError::InvalidInput {
                    message: "JMAP 端点不能为空".into(),
                })?;
            ProtocolSetup::Jmap { endpoint }
        }
        ProtocolKindDto::Imap => {
            let imap = endpoint_from_dto(
                &input.imap_host,
                input.imap_port,
                input.imap_security,
                "IMAP",
            )?;
            let smtp = optional_endpoint_from_dto(
                &input.smtp_host,
                input.smtp_port,
                input.smtp_security,
                "SMTP",
            )?;
            ProtocolSetup::Imap { imap, smtp }
        }
    };
    let mut account = NewAccount::new(
        input.email_address.trim(),
        setup,
        input.secret.trim().to_string(),
    );
    account.display_name = input.display_name.clone();
    account.sync_days = input.sync_days;
    account.poll_interval_secs = input.poll_interval_secs;
    Ok(account)
}

pub(crate) fn account_patch_from_dto(patch: &AccountPatchDto) -> FfiResult<AccountPatch> {
    // UDL 表达不了 Option<Option<T>>：用 clear_display_name / clear_smtp 表示「显式置空」
    let display_name = if patch.clear_display_name {
        Some(None)
    } else {
        patch.display_name.clone().map(Some)
    };

    let imap = match (&patch.imap_host, patch.imap_port, patch.imap_security) {
        (None, None, None) => None,
        _ => Some(endpoint_from_dto(
            &patch.imap_host,
            patch.imap_port,
            patch.imap_security,
            "IMAP",
        )?),
    };

    let smtp = if patch.clear_smtp {
        Some(None)
    } else {
        optional_endpoint_from_dto(
            &patch.smtp_host,
            patch.smtp_port,
            patch.smtp_security,
            "SMTP",
        )?
        .map(Some)
    };

    Ok(AccountPatch {
        display_name,
        sync_days: patch.sync_days,
        poll_interval_secs: patch.poll_interval_secs,
        jmap_endpoint: patch.jmap_endpoint.clone(),
        imap,
        smtp,
        secret: patch.secret.clone(),
    })
}

pub(crate) fn draft_from_dto(d: &DraftDto) -> crossmail_service::Draft {
    let mut draft = crossmail_service::Draft::new(d.subject.clone());
    draft.to = addresses_from_dto(&d.to);
    draft.cc = addresses_from_dto(&d.cc);
    draft.bcc = addresses_from_dto(&d.bcc);
    draft.text_body = d.text_body.clone();
    draft.html_body = d.html_body.clone();
    draft.in_reply_to = d.in_reply_to.clone();
    draft.references = d.references.clone();
    draft
}

/// 草稿 → 待发消息。发件人一律取账号自身地址（不允许草稿伪造 From）；
/// 正文两路都为空时给空串，保证 SMTP 报文里有合法的 text/plain 分隔段。
pub(crate) fn outgoing_from_draft(account: &core::Account, d: &DraftDto) -> core::OutgoingMessage {
    let mut msg = core::OutgoingMessage::new(
        account.id.clone(),
        core::Address {
            name: account.display_name.clone(),
            email: account.email_address.clone(),
        },
        addresses_from_dto(&d.to),
        d.subject.clone(),
    );
    msg.cc = addresses_from_dto(&d.cc);
    msg.bcc = addresses_from_dto(&d.bcc);
    msg.text_body = Some(d.text_body.clone().unwrap_or_default());
    msg.html_body = d.html_body.clone();
    msg.in_reply_to = d.in_reply_to.clone();
    msg.references = d.references.clone();
    msg
}

pub(crate) fn email_query_from_dto(q: &EmailQueryDto) -> EmailQuery {
    // limit 为 0 时给个合理默认，避免 UI 忘了传就查出 0 条
    let limit = if q.limit == 0 { 50 } else { q.limit };
    let mut query = EmailQuery::new().with_limit(limit).with_offset(q.offset);
    if let Some(mid) = &q.mailbox_id {
        if !mid.trim().is_empty() {
            query = query.in_mailbox(mid);
        }
    }
    if q.unread_only {
        query = query.unread_only();
    }
    query
}

pub(crate) fn search_scope_to_service(scope: SearchScopeDto) -> crossmail_service::SearchScope {
    match scope {
        SearchScopeDto::All => crossmail_service::SearchScope::All,
        SearchScopeDto::Subject => crossmail_service::SearchScope::Subject,
        SearchScopeDto::From => crossmail_service::SearchScope::From,
        SearchScopeDto::To => crossmail_service::SearchScope::To,
    }
}

// ------------------------------------------------ 安全渲染与可观测（S6）

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedBodyDto {
    pub email_id: String,
    pub html: Option<String>,
    pub text: Option<String>,
    pub remote_images_allowed: bool,
    pub blocked_count: u32,
    pub blocked_urls: Vec<String>,
    pub removed_tag_count: u32,
    pub truncated: bool,
    pub trust_level: String,
    pub trust_message: String,
    pub sender_domain: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderTrustDto {
    pub address: String,
    pub domain: String,
    pub level: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowanceDto {
    pub sender: String,
    pub scope: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogStatusDto {
    pub level: String,
    pub protocol_payload: bool,
    pub files: Vec<String>,
    pub dir: Option<String>,
}

pub(crate) fn rendered_to_dto(r: &crossmail_service::RenderedBody) -> RenderedBodyDto {
    let removed_tag_count = r
        .sanitize
        .as_ref()
        .map(|s| s.removed_tags.values().sum::<usize>() as u32)
        .unwrap_or(0);
    RenderedBodyDto {
        email_id: r.email_id.clone(),
        html: r.html.clone(),
        text: r.text.clone(),
        remote_images_allowed: r.remote_images_allowed,
        blocked_count: r.blocked_count() as u32,
        blocked_urls: r.blocked_urls.clone(),
        removed_tag_count,
        truncated: r.sanitize.as_ref().map(|s| s.truncated).unwrap_or(false),
        trust_level: r.trust.level.as_str().to_string(),
        trust_message: r.trust.summary(),
        sender_domain: r.trust.registrable_domain.clone(),
    }
}

pub(crate) fn sender_trust_to_dto(t: &crossmail_sanitize::SenderTrust) -> SenderTrustDto {
    SenderTrustDto {
        address: t.address.clone(),
        domain: t.registrable_domain.clone(),
        level: t.level.as_str().to_string(),
        message: t.summary(),
    }
}

pub(crate) fn allowance_to_dto(a: &crossmail_storage::AllowanceRow) -> AllowanceDto {
    AllowanceDto {
        sender: a.sender.clone(),
        scope: a.scope.clone(),
        expires_at: a.expires_at.map(|t| t.to_rfc3339()),
    }
}

pub(crate) fn log_status_to_dto(logger: &crossmail_obs::Logger) -> LogStatusDto {
    let filter = logger.filter();
    LogStatusDto {
        level: filter.level.as_str().to_ascii_lowercase(),
        protocol_payload: filter.protocol_payload,
        files: logger
            .log_files()
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        dir: logger.log_dir().map(|p| p.display().to_string()),
    }
}
