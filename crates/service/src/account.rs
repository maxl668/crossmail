//! 账号业务层（S4-04）：增删改查 + 凭证存储接口。
//!
//! 关键约束：`Account` 里只放 `Credential::Keyring { alias }`，明文凭证进
//! `SecretVault`；删账号必须连带清掉它名下所有凭证别名，避免幽灵密钥残留。

use chrono::Utc;
use crossmail_core::{
    Account, Credential, Id, ImapConfig, JmapConfig, MailError, MailResult, ProtocolKind,
    SecurityMode, SmtpConfig,
};
use crossmail_storage::Database;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::credential::SecretVault;
use crate::lock_db;

/// 凭证别名根前缀：`crossmail.account.{id}` 一批，方便按前缀级联清理。
pub const ALIAS_ROOT: &str = "crossmail.account.";

/// 协议端点（IMAP / SMTP 共用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    pub security: SecurityMode,
}

impl Endpoint {
    pub fn new(host: impl Into<String>, port: u16, security: SecurityMode) -> Self {
        Self {
            host: host.into(),
            port,
            security,
        }
    }

    fn validate(&self, label: &str) -> MailResult<()> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err(MailError::InvalidInput(format!("{label} 主机不能为空")));
        }
        if host.chars().any(|c| c.is_whitespace()) {
            return Err(MailError::InvalidInput(format!("{label} 主机含空格")));
        }
        if self.port == 0 {
            return Err(MailError::InvalidInput(format!("{label} 端口不合法")));
        }
        Ok(())
    }
}

/// 新建账号时的协议配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolSetup {
    Jmap {
        endpoint: String,
    },
    Imap {
        imap: Endpoint,
        smtp: Option<Endpoint>,
    },
}

impl ProtocolSetup {
    pub fn kind(&self) -> ProtocolKind {
        match self {
            ProtocolSetup::Jmap { .. } => ProtocolKind::Jmap,
            ProtocolSetup::Imap { .. } => ProtocolKind::Imap,
        }
    }

    fn validate(&self) -> MailResult<()> {
        match self {
            ProtocolSetup::Jmap { endpoint } => {
                let e = endpoint.trim();
                if !(e.starts_with("https://") || e.starts_with("http://")) {
                    return Err(MailError::InvalidInput(
                        "JMAP 端点必须是 http(s) URL".into(),
                    ));
                }
                Ok(())
            }
            ProtocolSetup::Imap { imap, smtp } => {
                imap.validate("IMAP")?;
                if let Some(s) = smtp {
                    s.validate("SMTP")?;
                }
                Ok(())
            }
        }
    }
}

/// 新建账号输入。
#[derive(Debug, Clone)]
pub struct NewAccount {
    pub email_address: String,
    pub display_name: Option<String>,
    pub setup: ProtocolSetup,
    /// 明文凭证（密码 / 应用专用密码 / OAuth refresh token 原文）
    pub secret: String,
    pub sync_days: Option<u32>,
    pub poll_interval_secs: Option<u32>,
}

impl NewAccount {
    pub fn new(
        email_address: impl Into<String>,
        setup: ProtocolSetup,
        secret: impl Into<String>,
    ) -> Self {
        Self {
            email_address: email_address.into(),
            display_name: None,
            setup,
            secret: secret.into(),
            sync_days: None,
            poll_interval_secs: None,
        }
    }
}

/// 账号局部更新：`Some(None)` 表示显式清空可空字段。
#[derive(Debug, Clone, Default)]
pub struct AccountPatch {
    pub display_name: Option<Option<String>>,
    pub sync_days: Option<u32>,
    pub poll_interval_secs: Option<u32>,
    pub jmap_endpoint: Option<String>,
    pub imap: Option<Endpoint>,
    pub smtp: Option<Option<Endpoint>>,
    /// 轮换凭证（明文，内部封存）
    pub secret: Option<String>,
}

/// 账号业务服务。
pub struct AccountService {
    db: Arc<Mutex<Database>>,
    vault: Arc<SecretVault>,
}

impl AccountService {
    pub fn new(db: Arc<Mutex<Database>>, vault: Arc<SecretVault>) -> Self {
        Self { db, vault }
    }

    /// 新建账号：校验 → 凭证封存 → 落库。
    pub fn create(&self, input: NewAccount) -> MailResult<Account> {
        let email = normalize_email(&input.email_address)?;
        input.setup.validate()?;
        let sync_days = input.sync_days.unwrap_or(30);
        let poll = input.poll_interval_secs.unwrap_or(300);
        validate_sync_days(sync_days)?;
        validate_poll(poll)?;

        let id = Uuid::new_v4().to_string();
        if self.address_exists(&email, input.setup.kind())? {
            return Err(MailError::Conflict(format!("账号已存在: {email}")));
        }

        let alias = alias_for(&id);
        self.vault.put(&alias, &input.secret)?;
        let credential = Credential::Keyring {
            alias: alias.clone(),
        };
        let mut account = Account::new(id.clone(), email, input.setup.kind());
        account.display_name = input.display_name;
        account.sync_days = sync_days;
        account.poll_interval_secs = poll;
        account.created_at = Utc::now();
        match &input.setup {
            ProtocolSetup::Jmap { endpoint } => {
                account.jmap = Some(JmapConfig {
                    endpoint: endpoint.trim().to_string(),
                    credential,
                });
            }
            ProtocolSetup::Imap { imap, smtp } => {
                account.imap = Some(ImapConfig {
                    host: imap.host.trim().to_string(),
                    port: imap.port,
                    security: imap.security,
                    credential: credential.clone(),
                });
                account.smtp = smtp.as_ref().map(|s| SmtpConfig {
                    host: s.host.trim().to_string(),
                    port: s.port,
                    security: s.security,
                    credential,
                });
            }
        }

        if let Err(e) = lock_db(&self.db)?.upsert_account(&account) {
            // 落库失败不能留下孤儿密钥
            let _ = self.vault.delete(&alias);
            return Err(e);
        }
        Ok(account)
    }

    pub fn get(&self, id: &str) -> MailResult<Account> {
        lock_db(&self.db)?
            .get_account(id)?
            .ok_or_else(|| MailError::NotFound(format!("账号不存在: {id}")))
    }

    pub fn list(&self) -> MailResult<Vec<Account>> {
        let ids = lock_db(&self.db)?.list_accounts()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            // 逐个短锁读取，避免长时间持有连接锁
            if let Some(a) = lock_db(&self.db)?.get_account(&id)? {
                out.push(a);
            }
        }
        Ok(out)
    }

    pub fn update(&self, id: &str, patch: AccountPatch) -> MailResult<Account> {
        let mut account = self.get(id)?;
        if let Some(name) = patch.display_name {
            account.display_name = name.filter(|s| !s.trim().is_empty());
        }
        if let Some(days) = patch.sync_days {
            validate_sync_days(days)?;
            account.sync_days = days;
        }
        if let Some(poll) = patch.poll_interval_secs {
            validate_poll(poll)?;
            account.poll_interval_secs = poll;
        }
        if let Some(endpoint) = patch.jmap_endpoint {
            let jmap = account
                .jmap
                .as_mut()
                .ok_or_else(|| MailError::InvalidInput("该账号不是 JMAP 账号".into()))?;
            ProtocolSetup::Jmap {
                endpoint: endpoint.clone(),
            }
            .validate()?;
            jmap.endpoint = endpoint.trim().to_string();
        }
        if let Some(imap) = patch.imap {
            imap.validate("IMAP")?;
            let cfg = account
                .imap
                .as_mut()
                .ok_or_else(|| MailError::InvalidInput("该账号不是 IMAP 账号".into()))?;
            cfg.host = imap.host.trim().to_string();
            cfg.port = imap.port;
            cfg.security = imap.security;
        }
        if let Some(smtp) = patch.smtp {
            match smtp {
                None => account.smtp = None,
                Some(ep) => {
                    ep.validate("SMTP")?;
                    let credential = account
                        .imap
                        .as_ref()
                        .map(|c| c.credential.clone())
                        .or_else(|| account.smtp.as_ref().map(|c| c.credential.clone()))
                        .ok_or_else(|| {
                            MailError::InvalidInput("缺少凭证引用，无法配置 SMTP".into())
                        })?;
                    account.smtp = Some(SmtpConfig {
                        host: ep.host.trim().to_string(),
                        port: ep.port,
                        security: ep.security,
                        credential,
                    });
                }
            }
        }
        if let Some(secret) = patch.secret {
            self.vault.put(&alias_for(id), &secret)?;
        }
        lock_db(&self.db)?.upsert_account(&account)?;
        Ok(account)
    }

    /// 凭证轮换：旧密文被覆盖，历史明文不可恢复。
    pub fn rotate_secret(&self, id: &str, secret: &str) -> MailResult<()> {
        self.get(id)?;
        self.vault.put(&alias_for(id), secret)
    }

    /// 连接时按账号取明文凭证。
    pub fn resolve_secret(&self, account: &Account) -> MailResult<String> {
        let alias = credential_alias(account)
            .ok_or_else(|| MailError::InvalidInput("账号未配置凭证引用".into()))?;
        self.vault
            .get(alias)?
            .ok_or_else(|| MailError::Authentication(format!("账号凭证缺失: {alias}")))
    }

    /// 删除账号：级联删本地数据 + 清理全部凭证别名。
    pub fn delete(&self, id: &str) -> MailResult<()> {
        self.get(id)?;
        lock_db(&self.db)?.delete_account(id)?;
        self.vault.delete_prefix(&format!("{ALIAS_ROOT}{id}"))?;
        Ok(())
    }

    /// 同协议下邮箱地址是否已存在（大小写不敏感）。
    pub fn address_exists(&self, email: &str, kind: ProtocolKind) -> MailResult<bool> {
        let target = email.trim().to_ascii_lowercase();
        for account in self.list()? {
            if account.protocol == kind && account.email_address.to_ascii_lowercase() == target {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// 找出凭证别名（便于 UI 显示"凭证已配置"）。
    pub fn credential_alias(&self, account: &Account) -> MailResult<Option<String>> {
        Ok(credential_alias(account).map(|s| s.to_string()))
    }
}

fn credential_alias(account: &Account) -> Option<&str> {
    account
        .imap
        .as_ref()
        .map(|c| &c.credential)
        .or_else(|| account.jmap.as_ref().map(|c| &c.credential))
        .or_else(|| account.smtp.as_ref().map(|c| &c.credential))
        .map(|c| match c {
            Credential::Keyring { alias } => alias.as_str(),
            Credential::OAuth2 { alias, .. } => alias.as_str(),
        })
}

fn alias_for(account_id: &str) -> String {
    format!("{ALIAS_ROOT}{account_id}")
}

/// 邮箱地址规范化 + 基本校验（不追求 RFC 完备，拦住明显错误即可）。
pub fn normalize_email(raw: &str) -> MailResult<String> {
    let email = raw.trim().to_ascii_lowercase();
    if email.is_empty() {
        return Err(MailError::InvalidInput("邮箱地址不能为空".into()));
    }
    if email.chars().any(|c| c.is_whitespace()) {
        return Err(MailError::InvalidInput("邮箱地址不能含空格".into()));
    }
    let mut parts = email.split('@');
    let (local, domain) = match (parts.next(), parts.next(), parts.next()) {
        (Some(l), Some(d), None) => (l, d),
        _ => return Err(MailError::InvalidInput(format!("邮箱格式不合法: {email}"))),
    };
    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return Err(MailError::InvalidInput(format!("邮箱格式不合法: {email}")));
    }
    if domain.starts_with('.') || domain.ends_with('.') {
        return Err(MailError::InvalidInput(format!("邮箱域名不合法: {email}")));
    }
    Ok(email)
}

fn validate_sync_days(days: u32) -> MailResult<()> {
    if !(1..=3650).contains(&days) {
        return Err(MailError::InvalidInput("同步天数需在 1~3650 天之间".into()));
    }
    Ok(())
}

fn validate_poll(secs: u32) -> MailResult<()> {
    if secs < 30 {
        return Err(MailError::InvalidInput(
            "轮询间隔不得小于 30 秒（避免被服务端限流）".into(),
        ));
    }
    Ok(())
}

/// 供上层判断 Id 是否可用。
pub fn ensure_id(id: &str, label: &str) -> MailResult<Id> {
    if id.trim().is_empty() {
        return Err(MailError::InvalidInput(format!("{label} 不能为空")));
    }
    Ok(id.to_string())
}
