// CrossMail Windows 桌面端：Tauri 命令层（S5-02 / 第一版）
//
// 设计原则：不复制核心逻辑，只做「Tauri 命令 ↔ crossmail-app 编排层 ↔ service 业务层」
// 的接线。所有耗时网络操作走 spawn_blocking；凭证只在内存短暂存在（SecretVault 密文仓）。
// release 构建隐藏控制台窗口（GUI 子系统）；debug 保留控制台便于看日志。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod mail_notifier;
use crossmail_app::{
    build_backend, fetch_email_blocking, send_message_blocking, subscribe_push,
    sync_account_blocking, Backend, PushHandle, PushNotice,
};
use crossmail_core::model::{Account, Address, Email, Mailbox, MailboxRole, OutgoingAttachment, OutgoingMessage};
use crossmail_core::provider::MailboxDraft;
use crossmail_core::MailProvider;
use crossmail_service::account::{AccountService, Endpoint, NewAccount, ProtocolSetup};
use crossmail_service::mail::MailService;
use crossmail_storage::{Database, EmailPage, EmailQuery, MailRule, SearchFields, SearchQuery};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};

/// 全局上下文：与 Tauri app 生命周期一致。
pub struct AppCtx {
    pub db: Arc<Mutex<Database>>,
    pub accounts: Arc<AccountService>,
    pub mails: Arc<MailService>,
    pub push_handles: Arc<Mutex<HashMap<String, PushHandle>>>,
}

// ---------- DTO ----------

#[derive(Deserialize, Clone)]
pub struct EndpointReq {
    pub host: String,
    pub port: u16,
    /// "implicit_tls" | "start_tls"
    pub security: String,
}

#[derive(Deserialize)]
pub struct NewAccountReq {
    pub email_address: String,
    pub display_name: Option<String>,
    /// "jmap" | "imap"
    pub kind: String,
    pub jmap_endpoint: Option<String>,
    pub imap: Option<EndpointReq>,
    pub smtp: Option<EndpointReq>,
    pub secret: String,
    pub sync_days: Option<u32>,
    /// test_account 专用：验证时是否顺带发测试邮件（默认 true）
    pub send_test: Option<bool>,
}

#[derive(Serialize)]
pub struct EmailPageDto {
    pub items: Vec<Email>,
    pub total: u32,
    pub next_offset: Option<u32>,
}

#[derive(Serialize)]
pub struct ReadEmailDto {
    pub email: Email,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    /// true 表示正文是本次回源拉的（首次阅读）
    pub fetched_remote: bool,
}

#[derive(Serialize)]
pub struct SyncSummaryDto {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub unchanged: u64,
    pub has_more: bool,
    pub full_resync: bool,
}

#[derive(Serialize, Clone)]
pub struct UnreadDto {
    pub mailbox_id: String,
    pub unread: u64,
}

#[derive(Serialize, Clone)]
#[serde(tag = "type")]
pub enum PushNoticeDto {
    Synced {
        reason: String,
        inserted: u64,
        updated: u64,
        deleted: u64,
        unchanged: u64,
        unread_total: u64,
        unread_by_mailbox: Vec<UnreadDto>,
        coalesced: u64,
    },
    Failed {
        reason: String,
        error: String,
        retry_in_ms: Option<u64>,
    },
    Closed {
        reason: String,
    },
}

// ---------- 内部工具 ----------

fn parse_security(s: &str) -> Result<crossmail_core::model::SecurityMode, String> {
    match s {
        "implicit_tls" => Ok(crossmail_core::model::SecurityMode::ImplicitTls),
        "start_tls" => Ok(crossmail_core::model::SecurityMode::StartTls),
        other => Err(format!("未知安全模式：{other}")),
    }
}

/// 拿到账号 + 解密后的凭证（网络操作前置步骤）。
fn account_with_secret(
    accounts: &AccountService,
    account_id: &str,
) -> Result<(Account, String), String> {
    let account = accounts.get(account_id).map_err(|e| e.to_string())?;
    let secret = accounts.resolve_secret(&account).map_err(|e| e.to_string())?;
    Ok((account, secret))
}

/// 解析 "Name <a@b.c>" / "a@b.c" 为 Address。
fn parse_address(s: &str) -> Address {
    let s = s.trim();
    if let (Some(lt), Some(gt)) = (s.find('<'), s.rfind('>')) {
        if lt < gt {
            let name = s[..lt].trim().trim_matches('"');
            let email = s[lt + 1..gt].trim();
            return Address {
                name: if name.is_empty() { None } else { Some(name.to_string()) },
                email: email.to_string(),
            };
        }
    }
    Address { name: None, email: s.to_string() }
}

// ---------- 命令：账号 ----------

#[tauri::command]
fn list_accounts(ctx: State<AppCtx>) -> Result<Vec<Account>, String> {
    ctx.accounts.list().map_err(|e| e.to_string())
}

#[tauri::command]
async fn create_account(ctx: State<'_, AppCtx>, req: NewAccountReq) -> Result<Account, String> {
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let setup = match req.kind.as_str() {
            "jmap" => ProtocolSetup::Jmap {
                endpoint: req
                    .jmap_endpoint
                    .ok_or("JMAP 账号必须提供 endpoint")?,
            },
            "imap" => ProtocolSetup::Imap {
                imap: {
                    let e = req.imap.ok_or("IMAP 账号必须提供收件服务器")?;
                    Endpoint::new(e.host, e.port, parse_security(&e.security)?)
                },
                smtp: req.smtp.map(|e| {
                    Endpoint::new(e.host, e.port, parse_security(&e.security).unwrap_or(
                        crossmail_core::model::SecurityMode::ImplicitTls,
                    ))
                }),
            },
            other => return Err(format!("未知协议类型：{other}")),
        };
        let mut input = NewAccount::new(req.email_address, setup, req.secret);
        input.display_name = req.display_name;
        input.sync_days = req.sync_days;
        accounts.create(input).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 账号设置更新（v1.0 显示名/同步天数/凭证；v1.3 服务器信息）。
/// 空字段表示不修改；密码字段为 None 不动、Some("") 拒绝、有值即轮换。
#[derive(Deserialize)]
pub struct UpdateAccountReq {
    pub display_name: Option<String>,
    pub sync_days: Option<u32>,
    pub secret: Option<String>,
    pub jmap_endpoint: Option<String>,
    pub imap: Option<EndpointReq>,
    pub smtp: Option<EndpointReq>,
}

#[tauri::command]
fn update_account(
    ctx: State<'_, AppCtx>,
    app: tauri::AppHandle,
    account_id: String,
    req: UpdateAccountReq,
) -> Result<Account, String> {
    use crossmail_service::account::AccountPatch;
    let mut patch = AccountPatch::default();
    if let Some(name) = req.display_name {
        let name = name.trim().to_string();
        patch.display_name = Some(if name.is_empty() { None } else { Some(name) });
    }
    patch.sync_days = req.sync_days;
    if let Some(sec) = req.secret {
        if sec.trim().is_empty() {
            return Err("密码不能为空".to_string());
        }
        patch.secret = Some(sec);
    }
    if let Some(ep) = req.jmap_endpoint {
        if ep.trim().is_empty() {
            return Err("JMAP 端点不能为空".to_string());
        }
        patch.jmap_endpoint = Some(ep.trim().to_string());
    }
    if let Some(e) = req.imap {
        patch.imap = Some(Endpoint::new(
            e.host.trim().to_string(),
            e.port,
            parse_security(&e.security)?,
        ));
    }
    if let Some(e) = req.smtp {
        patch.smtp = Some(Some(Endpoint::new(
            e.host.trim().to_string(),
            e.port,
            parse_security(&e.security)?,
        )));
    }
    let updated = ctx
        .accounts
        .update(&account_id, patch)
        .map_err(|e| e.to_string())?;
    // 服务器信息/凭证变更后，旧推送会话已失效——取消旧 handle，
    // 用户下次手动同步或重启时按新配置重建。
    if let Some(h) = ctx.push_handles.lock().unwrap().remove(&account_id) {
        h.cancel();
    }
    let _ = app;
    Ok(updated)
}

/// 连接验证（v1.3）：添加账号前先试登录，不改任何数据。
/// 返回 {login_ok, send_ok, detail}；send_test=true 时再发一封「发给自己」的
/// 测试邮件验证 SMTP/发送链路（IMAP 验证以 list_mailboxes 成功为准）。
#[tauri::command]
async fn test_account(req: NewAccountReq) -> Result<serde_json::Value, String> {
    let secret = req.secret.clone();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("运行时启动失败：{e}"))?;
    rt.block_on(async move {
        let setup = match req.kind.as_str() {
            "jmap" => ProtocolSetup::Jmap {
                endpoint: req
                    .jmap_endpoint
                    .clone()
                    .ok_or("JMAP 账号必须提供 endpoint")?,
            },
            "imap" => ProtocolSetup::Imap {
                imap: {
                    let e = req.imap.clone().ok_or("IMAP 账号必须提供收件服务器")?;
                    Endpoint::new(e.host, e.port, parse_security(&e.security)?)
                },
                smtp: req.smtp.clone().map(|e| {
                    Endpoint::new(e.host, e.port, parse_security(&e.security).unwrap_or(
                        crossmail_core::model::SecurityMode::ImplicitTls,
                    ))
                }),
            },
            other => return Err(format!("未知协议类型：{other}")),
        };
        // 探测账号：id 固定 "probe"，不落库；build_backend 只读 config+secret，
        // credential 字段填占位值即可。
        let credential = crossmail_core::model::Credential::Keyring {
            alias: "probe".to_string(),
        };
        let mut probe = crossmail_core::model::Account::new(
            "probe".to_string(),
            req.email_address.clone(),
            setup.kind(),
        );
        probe.display_name = req.display_name.clone();
        probe.sync_days = req.sync_days.unwrap_or(30);
        match &setup {
            ProtocolSetup::Jmap { endpoint } => {
                probe.jmap = Some(crossmail_core::model::JmapConfig {
                    endpoint: endpoint.trim().to_string(),
                    credential: credential.clone(),
                });
            }
            ProtocolSetup::Imap { imap, smtp } => {
                probe.imap = Some(crossmail_core::model::ImapConfig {
                    host: imap.host.trim().to_string(),
                    port: imap.port,
                    security: imap.security,
                    credential: credential.clone(),
                });
                probe.smtp = smtp.as_ref().map(|s| crossmail_core::model::SmtpConfig {
                    host: s.host.trim().to_string(),
                    port: s.port,
                    security: s.security,
                    credential: credential.clone(),
                });
            }
        }

        probe_check(probe, secret, req.send_test.unwrap_or(true)).await
    })
}

/// 账号设置页「验证连接」（v1.6）：用界面里正在编辑的服务器配置探测；
/// 密码留空则用已保存的密码，不重输。默认只验登录，不发测试邮件。
#[derive(Deserialize)]
pub struct TestAccountUpdateReq {
    pub account_id: String,
    pub jmap_endpoint: Option<String>,
    pub imap: Option<EndpointReq>,
    pub smtp: Option<EndpointReq>,
    pub secret: Option<String>,
    pub send_test: Option<bool>,
}

#[tauri::command]
async fn test_account_update(
    ctx: State<'_, AppCtx>,
    req: TestAccountUpdateReq,
) -> Result<serde_json::Value, String> {
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (mut probe, stored_secret) = account_with_secret(&accounts, &req.account_id)?;
        // 应用界面里正在编辑的覆盖值（未填的字段沿用账号现有配置）
        if let Some(ep) = req.jmap_endpoint.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(j) = probe.jmap.as_mut() { j.endpoint = ep.to_string(); }
        }
        if let Some(e) = req.imap.as_ref() {
            if let Some(ic) = probe.imap.as_mut() {
                ic.host = e.host.trim().to_string();
                ic.port = e.port;
                if let Ok(sec) = parse_security(&e.security) { ic.security = sec; }
            }
        }
        if let Some(e) = req.smtp.as_ref() {
            if let Some(sc) = probe.smtp.as_mut() {
                sc.host = e.host.trim().to_string();
                sc.port = e.port;
                if let Ok(sec) = parse_security(&e.security) { sc.security = sec; }
            }
        }
        let secret = match req.secret.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => stored_secret,
        };
        tauri::async_runtime::block_on(probe_check(probe, secret, req.send_test.unwrap_or(false)))
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 连接探测公共实现（v1.6 抽出）：登录 + 可选发自测试邮件。async 供 test_account 直接 await。
async fn probe_check(
    probe: crossmail_core::model::Account,
    secret: String,
    send_test: bool,
) -> Result<serde_json::Value, String> {
    let mut login_ok = false;
    let mut send_ok: Option<bool> = None;
    let mut detail = String::new();

    // 1) 收件侧：建 provider = 登录成功
    match crossmail_app::build_backend(&probe, &secret).await {
        Ok(_p) => login_ok = true,
        Err(e) => detail = format!("登录失败：{e}"),
    }

    // 2) 发件侧（可选）：发一封给自己的测试邮件
    if login_ok && send_test {
        let from = vec![Address {
            name: probe.display_name.clone(),
            email: probe.email_address.clone(),
        }];
        let msg = OutgoingMessage {
            account_id: "probe".to_string(),
            from,
            to: vec![parse_address(&probe.email_address)],
            cc: vec![],
            bcc: vec![],
            subject: "CrossMail 连接测试".to_string(),
            text_body: Some("这是一封连接测试邮件，收到即说明收发链路正常。".to_string()),
            html_body: None,
            attachments: vec![],
            in_reply_to: None,
            references: vec![],
            draft_id: None,
        };
        match crossmail_app::send_message_blocking(&probe, &secret, msg) {
            Ok(_) => send_ok = Some(true),
            Err(e) => {
                send_ok = Some(false);
                if detail.is_empty() {
                    detail = format!("收件登录成功，但发信失败：{e}");
                }
            }
        }
    }

    let ok = login_ok && send_ok.unwrap_or(true);
    Ok(serde_json::json!({ "ok": ok, "login_ok": login_ok, "send_ok": send_ok, "detail": detail }))
}

/// 新建文件夹（v1.3）：服务器端创建 + 落本地库。
#[tauri::command]
async fn create_mailbox(
    ctx: State<'_, AppCtx>,
    account_id: String,
    name: String,
) -> Result<Mailbox, String> {
    let accounts = ctx.accounts.clone();
    let db = ctx.db.clone();
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("文件夹名不能为空".to_string());
    }
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("运行时启动失败：{e}"))?;
        rt.block_on(async move {
            let p = crossmail_app::build_backend(&account, &secret)
                .await
                .map_err(|e| e.to_string())?;
            let mb = match &p {
                Backend::Jmap(p) => p
                    .create_mailbox(MailboxDraft {
                        name: name.clone(),
                        parent_id: None,
                        role: MailboxRole::Other,
                    })
                    .await
                    .map_err(|e| e.to_string())?,
                Backend::Imap(p) => p
                    .create_mailbox(MailboxDraft {
                        name: name.clone(),
                        parent_id: None,
                        role: MailboxRole::Other,
                    })
                    .await
                    .map_err(|e| e.to_string())?,
            };
            db.lock().unwrap().upsert_mailbox(&mb).map_err(|e| e.to_string())?;
            Ok(mb)
        })
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 删除文件夹（v1.3）：服务器端删除 + 本地库移除（JMAP/IMAP provider 均有实现）。
#[tauri::command]
async fn delete_mailbox(ctx: State<'_, AppCtx>, account_id: String, mailbox_id: String) -> Result<(), String> {
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("运行时启动失败：{e}"))?;
        rt.block_on(async move {
            let p = crossmail_app::build_backend(&account, &secret)
                .await
                .map_err(|e| e.to_string())?;
            let r = match &p {
                Backend::Jmap(p) => p.delete_mailbox(&mailbox_id).await,
                Backend::Imap(p) => p.delete_mailbox(&mailbox_id).await,
            };
            r.map_err(|e| e.to_string())?;
            Ok(())
        })
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

// ---------- 收信规则（v1.3） ----------

fn new_rule_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("rule-{:x}", n)
}

#[tauri::command]
fn list_rules(ctx: State<AppCtx>, account_id: String) -> Result<Vec<MailRule>, String> {
    let db = ctx.db.clone();
    let db = db.lock().unwrap();
    db.list_rules(&account_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_rule(
    ctx: State<AppCtx>,
    account_id: String,
    name: String,
    field: String,
    keyword: String,
    target_mailbox_id: String,
) -> Result<MailRule, String> {
    let field = field.trim().to_lowercase();
    if !["from", "to", "subject"].contains(&field.as_str()) {
        return Err(format!("不支持的匹配字段：{field}"));
    }
    if keyword.trim().is_empty() {
        return Err("关键词不能为空".to_string());
    }
    let rule = MailRule {
        id: new_rule_id(),
        account_id,
        name: name.trim().to_string(),
        field,
        keyword: keyword.trim().to_string(),
        target_mailbox_id,
        enabled: true,
        created_at: String::new(),
    };
    let db = ctx.db.clone();
    {
        let db = db.lock().unwrap();
        db.insert_rule(&rule).map_err(|e| e.to_string())?;
    }
    Ok(rule)
}

#[tauri::command]
fn delete_rule(ctx: State<AppCtx>, rule_id: String) -> Result<(), String> {
    let db = ctx.db.clone();
    let db = db.lock().unwrap();
    db.delete_rule(&rule_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn toggle_rule(ctx: State<AppCtx>, rule_id: String, enabled: bool) -> Result<(), String> {
    let db = ctx.db.clone();
    let db = db.lock().unwrap();
    db.update_rule_enabled(&rule_id, enabled)
        .map_err(|e| e.to_string())
}

/// 应用规则核心（v1.3）：命令与「同步后自动应用」共用。
/// 对账号的全部启用规则逐条匹配本地库（字段搜索），命中且不在目标文件夹的
/// 邮件 → 服务器端移动（复用 move_emails trait）。返回 {matched, moved, failed}。
async fn apply_rules_core(
    accounts: Arc<AccountService>,
    db: Arc<Mutex<Database>>,
    account_id: String,
) -> Result<serde_json::Value, String> {
    let pairs = {
        let db = db.lock().unwrap();
        let rules = db.list_rules(&account_id).map_err(|e| e.to_string())?;
        let mut pairs = Vec::new();
        for r in rules.into_iter().filter(|r| r.enabled) {
            let mut fields = SearchFields::default();
            match r.field.as_str() {
                "from" => fields.from = true,
                "to" => fields.to = true,
                _ => fields.subject = true,
            }
            let q = SearchQuery::new(r.keyword.clone())
                .with_fields(fields)
                .with_limit(500);
            let page = db.search_page(&account_id, &q).map_err(|e| e.to_string())?;
            for e in page.items {
                pairs.push((r.clone(), e));
            }
        }
        pairs
    };
    if pairs.is_empty() {
        return Ok(serde_json::json!({ "matched": 0, "moved": 0, "failed": 0 }));
    }
    let (account, secret) = account_with_secret(&accounts, &account_id)?;
    let backend = build_backend(&account, &secret).await.map_err(|e| e.to_string())?;
    let mut matched = 0usize;
    let mut moved = 0usize;
    let mut failed = 0usize;
    for (rule, email) in &pairs {
        matched += 1;
        // 已在目标文件夹（首个 mailbox 即目标）的不动
        if email.mailbox_ids.first().map(|s| s.as_str())
            == Some(rule.target_mailbox_id.as_str())
        {
            continue;
        }
        let from = email.mailbox_ids.first().cloned().unwrap_or_default();
        let r = match &backend {
            Backend::Jmap(p) => p
                .move_emails(&[email.id.clone()], &from, &rule.target_mailbox_id)
                .await
                .map(|_| ()),
            Backend::Imap(p) => p
                .move_emails(&[email.id.clone()], &from, &rule.target_mailbox_id)
                .await
                .map(|_| ()),
        };
        if r.is_ok() {
            moved += 1;
        } else {
            failed += 1;
        }
    }
    Ok(serde_json::json!({ "matched": matched, "moved": moved, "failed": failed }))
}

/// 应用规则（v1.3）：手动触发入口（规则管理对话框「立即应用」）。
#[tauri::command]
async fn apply_rules(
    ctx: State<'_, AppCtx>,
    account_id: String,
) -> Result<serde_json::Value, String> {
    apply_rules_core(ctx.accounts.clone(), ctx.db.clone(), account_id).await
}

#[tauri::command]
async fn delete_account(ctx: State<'_, AppCtx>, account_id: String) -> Result<(), String> {
    let accounts = ctx.accounts.clone();
    let handles = ctx.push_handles.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // 先停推送
        if let Some(h) = handles.lock().unwrap().remove(&account_id) {
            h.cancel();
        }
        accounts.delete(&account_id).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

// ---------- 命令：目录 / 邮件 ----------

#[tauri::command]
fn list_mailboxes(ctx: State<AppCtx>, account_id: String) -> Result<Vec<Mailbox>, String> {
    ctx.mails.list_mailboxes(&account_id).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_emails(
    ctx: State<AppCtx>,
    account_id: String,
    mailbox_id: Option<String>,
    unread_only: Option<bool>,
    offset: Option<u32>,
) -> Result<EmailPageDto, String> {
    let mut q = EmailQuery::new().with_offset(offset.unwrap_or(0));
    if let Some(mb) = mailbox_id {
        q = q.in_mailbox(mb);
    }
    if unread_only.unwrap_or(false) {
        q = q.unread_only();
    }
    let EmailPage { items, total, next_offset } =
        ctx.mails.list(&account_id, &q).map_err(|e| e.to_string())?;
    Ok(EmailPageDto { items, total, next_offset })
}

/// 全文搜索（v0.2）：主题/发件人/收件人三字段，走 storage 层 search_page。
#[tauri::command]
fn search_emails(
    ctx: State<AppCtx>,
    account_id: String,
    keyword: String,
    offset: Option<u32>,
) -> Result<EmailPageDto, String> {
    let db = ctx.db.clone();
    let db = db.lock().map_err(|e| e.to_string())?;
    let mut q = SearchQuery::new(&keyword);
    q.offset = offset.unwrap_or(0);
    q.limit = 100;
    let EmailPage { items, total, next_offset } =
        db.search_page(&account_id, &q).map_err(|e| e.to_string())?;
    Ok(EmailPageDto { items, total, next_offset })
}

/// 附件下载（v0.2）：按 blobId 经 provider 回源取回并写入用户选定路径。
#[tauri::command]
async fn download_attachment(
    ctx: State<'_, AppCtx>,
    account_id: String,
    blob_id: String,
    save_path: String,
) -> Result<u64, String> {
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("下载运行时启动失败：{e}"))?;
        let data: Vec<u8> = runtime.block_on(async {
            match build_backend(&account, &secret).await.map_err(|e| e.to_string())? {
                Backend::Jmap(p) => p.download_blob(&blob_id).await.map_err(|e| e.to_string()),
                Backend::Imap(p) => p.download_blob(&blob_id).await.map_err(|e| e.to_string()),
            }
        })?;
        std::fs::write(&save_path, &data)
            .map_err(|e| format!("写入 {save_path} 失败：{e}"))?;
        Ok(data.len() as u64)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn read_email(
    ctx: State<'_, AppCtx>,
    account_id: String,
    email_id: String,
) -> Result<ReadEmailDto, String> {
    let mails = ctx.mails.clone();
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut text = mails.read_body(&email_id, false).map_err(|e| e.to_string())?;
        let mut html = mails.read_body(&email_id, true).map_err(|e| e.to_string())?;
        let mut fetched_remote = false;
        if text.is_none() && html.is_none() {
            // 首次阅读：回源拉正文（BODY.PEEK，不会标已读）
            let (account, secret) = account_with_secret(&accounts, &account_id)?;
            let fetched = fetch_email_blocking(&account, &secret, &email_id)
                .map_err(|e| e.to_string())?;
            text = fetched.text_body.clone();
            html = fetched.html_body.clone();
            fetched_remote = true;
            // v1.4 修复：回源结果落缓存——此前每次打开都会重新走网络拉正文，
            // 导致「已读过的邮件重复打开也慢」（网络慢时体感尤其明显）。
            if let Err(e) = mails.save_body(&email_id, text.as_deref(), html.as_deref()) {
                eprintln!("正文落缓存失败（不影响本次阅读）：{e}");
            }
        }
        let email = mails.set_read(&email_id, true).map_err(|e| e.to_string())?;
        Ok(ReadEmailDto { email, text_body: text, html_body: html, fetched_remote })
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

#[tauri::command]
fn set_read(ctx: State<AppCtx>, email_id: String, read: bool) -> Result<Email, String> {
    ctx.mails.set_read(&email_id, read).map_err(|e| e.to_string())
}

#[tauri::command]
fn toggle_star(ctx: State<AppCtx>, email_id: String, starred: bool) -> Result<Email, String> {
    ctx.mails.toggle_star(&email_id, starred).map_err(|e| e.to_string())
}

// ---------- 命令：同步 / 发信 / 推送 ----------

#[tauri::command]
async fn sync_account(
    ctx: State<'_, AppCtx>,
    account_id: String,
) -> Result<SyncSummaryDto, String> {
    let accounts = ctx.accounts.clone();
    let db = ctx.db.clone();
    let rule_accounts = ctx.accounts.clone();
    let rule_db = ctx.db.clone();
    let rule_account_id = account_id.clone();
    let dto = tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let summary = sync_account_blocking(&db, &account, &secret, Default::default())
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(SyncSummaryDto {
            inserted: summary.inserted,
            updated: summary.updated,
            deleted: summary.deleted,
            unchanged: summary.unchanged,
            has_more: summary.has_more,
            full_resync: summary.full_resync,
        })
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?;
    let dto = dto?;
    // 同步落地后自动应用收信规则（后台执行，不阻塞本次同步返回）
    tauri::async_runtime::spawn(async move {
        if let Err(e) = apply_rules_core(rule_accounts, rule_db, rule_account_id).await {
            eprintln!("同步后自动应用规则失败：{e}");
        }
    });
    Ok(dto)
}

#[tauri::command]
async fn send_email(
    ctx: State<'_, AppCtx>,
    account_id: String,
    subject: String,
    to: Vec<String>,
    cc: Option<Vec<String>>,
    bcc: Option<Vec<String>>,
    body: String,
    html_body: Option<String>,
    attachments: Option<Vec<AttReq>>,
    in_reply_to: Option<String>,
    references: Option<Vec<String>>,
) -> Result<serde_json::Value, String> {
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let from = vec![Address { name: account.display_name.clone(), email: account.email_address.clone() }];
        let msg = OutgoingMessage {
            account_id: account.id.clone(),
            from,
            to: to.iter().map(|s| parse_address(s)).collect(),
            cc: cc.unwrap_or_default().iter().map(|s| parse_address(s)).collect(),
            bcc: bcc.unwrap_or_default().iter().map(|s| parse_address(s)).collect(),
            subject,
            text_body: Some(body),
            html_body,
            attachments: decode_attachments(attachments)?,
            in_reply_to,
            references: references.unwrap_or_default(),
            draft_id: None,
        };
        let receipt = send_message_blocking(&account, &secret, msg).map_err(|e| e.to_string())?;
        serde_json::to_value(&receipt).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

// ---------- eml 归档（v1.1） ----------

/// 自定义数据目录标记文件（与 exe 同级，v1.2）。内容为一行绝对路径；文件不存在/为空 = 默认便携位置。
const LOCATION_FILE: &str = "crossmail-data.location";

fn exe_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
}

fn read_custom_location() -> Option<std::path::PathBuf> {
    let s = std::fs::read_to_string(exe_dir()?.join(LOCATION_FILE)).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(t))
    }
}

/// 数据目录：有自定义标记则用之，否则 exe 同级 crossmail-data\（便携化，v0.2）。
fn data_dir() -> Result<std::path::PathBuf, String> {
    if let Some(p) = read_custom_location() {
        return Ok(p);
    }
    exe_dir()
        .map(|d| d.join("crossmail-data"))
        .ok_or_else(|| "无法定位数据目录".to_string())
}

/// 主题转安全文件名基名（v1.3）：控制符剔除、Windows 非法字符替换为空格、
/// 截断 80 字符；主题为空回退 email_id（同样做安全替换）。
fn eml_base_name(subject: &str, email_id: &str) -> String {
    let sanitize = |s: &str| -> String {
        let cleaned: String = s
            .chars()
            .filter(|c| !c.is_control())
            .map(|c| {
                if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                    ' '
                } else {
                    c
                }
            })
            .collect();
        cleaned.trim().chars().take(80).collect()
    };
    let base = sanitize(subject);
    if base.is_empty() {
        sanitize(email_id)
    } else {
        base
    }
}

/// 归档文件名：首选 `{主题}.eml`。
fn eml_file_name(subject: &str, email_id: &str) -> String {
    format!("{}.eml", eml_base_name(subject, email_id))
}

/// 同主题多封邮件的次选名：`{主题} [id尾6].eml`（id 尾稳定 → 幂等）。
fn eml_file_name_with_suffix(subject: &str, email_id: &str) -> String {
    let tail: String = email_id.chars().rev().take(6).collect();
    format!("{} [{}].eml", eml_base_name(subject, email_id), tail)
}

/// 从 eml 原文头部提取 Message-ID（不含尖括号）。
fn eml_message_id(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(16_384)];
    let s = String::from_utf8_lossy(head);
    for line in s.lines() {
        let l = line.trim();
        if l.is_empty() {
            break; // 头部结束
        }
        if let Some(v) = l
            .strip_prefix("Message-ID:")
            .or_else(|| l.strip_prefix("Message-Id:"))
        {
            return Some(
                v.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string(),
            );
        }
    }
    None
}

/// 在 eml 缓存目录里挑本邮件的归档文件：主题名优先，用 Message-ID 判定归属，
/// 不匹配（同主题另一封）时落 `{主题} [id尾].eml`。
fn pick_eml_path(dir: &std::path::Path, subject: &str, email_id: &str, message_id: Option<&str>) -> std::path::PathBuf {
    let primary = dir.join(eml_file_name(subject, email_id));
    let alt = dir.join(eml_file_name_with_suffix(subject, email_id));
    if alt.is_file() {
        return alt;
    }
    if primary.is_file() {
        let owned = std::fs::read(&primary)
            .ok()
            .and_then(|b| eml_message_id(&b))
            .zip(message_id.map(|m| m.trim().trim_start_matches('<').trim_end_matches('>').to_string()))
            .map(|(a, b)| a == b)
            .unwrap_or(false);
        if owned {
            return primary;
        }
        return alt;
    }
    primary
}

/// 拉取整封原始报文（RFC822）。JMAP 用 blobId（email_get 必返回）；
/// IMAP 用空 part_id —— `BODY[]` 即整封，provider 的 download_blob 契约内直接支持。
async fn fetch_raw_message(
    account: &Account,
    secret: &str,
    email_id: &str,
    jmap_blob_id: Option<&str>,
) -> Result<Vec<u8>, String> {
    use crossmail_app::build_backend;
    match build_backend(account, secret).await.map_err(|e| e.to_string())? {
        crossmail_app::Backend::Jmap(p) => {
            let blob = jmap_blob_id
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "该邮件缺少 blobId，无法归档 eml".to_string())?;
            p.download_blob(blob).await.map_err(|e| e.to_string())
        }
        crossmail_app::Backend::Imap(p) => {
            // 空部件 = BODY[] = 整封原始报文；CTE 解码在 provider 内对
            // 找不到部件编码的情况回落 SevenBit（原样），正符合 eml 需求
            p.download_blob(&format!("{email_id}#"))
                .await
                .map_err(|e| e.to_string())
        }
    }
}

/// 归档一封邮件为 .eml（幂等）：已存在直接返回路径，不重复拉取。
/// 阅读时由前端自动触发；eml 目录永不参与 LRU 淘汰。
#[tauri::command]
async fn archive_eml(
    ctx: State<'_, AppCtx>,
    account_id: String,
    email_id: String,
) -> Result<String, String> {
    let accounts = ctx.accounts.clone();
    let mails = ctx.mails.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let dir = data_dir()?.join("eml");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 eml 目录失败：{e}"))?;
        let email = mails.read(&email_id).map_err(|e| e.to_string())?;
        let path = pick_eml_path(&dir, &email.subject, &email_id, email.message_id.as_deref());
        if path.is_file() {
            return Ok(path.to_string_lossy().to_string());
        }
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let raw = tauri::async_runtime::block_on(fetch_raw_message(
            &account,
            &secret,
            &email_id,
            email.blob_id.as_deref(),
        ))?;
        std::fs::write(&path, &raw).map_err(|e| format!("写入 eml 失败：{e}"))?;
        Ok(path.to_string_lossy().to_string())
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 导出 .eml 到用户指定位置（另存为路径由前端传入；内容不足时回源拉取）。
#[tauri::command]
async fn export_eml(
    ctx: State<'_, AppCtx>,
    account_id: String,
    email_id: String,
    save_path: String,
) -> Result<String, String> {
    let accounts = ctx.accounts.clone();
    let mails = ctx.mails.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // 优先读已归档的 eml，避免再走一次网络
        if let Ok(dir) = data_dir() {
            if let Ok(email) = mails.read(&email_id) {
                let cached = pick_eml_path(&dir.join("eml"), &email.subject, &email_id, email.message_id.as_deref());
                if cached.is_file() {
                    let bytes = std::fs::read(&cached).map_err(|e| e.to_string())?;
                    std::fs::write(&save_path, &bytes).map_err(|e| e.to_string())?;
                    return Ok(format!("{save_path}（{} 字节）", bytes.len()));
                }
            }
        }
        let email = mails.read(&email_id).map_err(|e| e.to_string())?;
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let raw = tauri::async_runtime::block_on(fetch_raw_message(
            &account,
            &secret,
            &email_id,
            email.blob_id.as_deref(),
        ))?;
        let n = raw.len();
        std::fs::write(&save_path, &raw).map_err(|e| e.to_string())?;
        Ok(format!("{save_path}（{n} 字节）"))
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 查询当前数据目录（v1.2）。custom = 是否来自自定义标记。
#[tauri::command]
fn get_data_location() -> Result<serde_json::Value, String> {
    let dir = data_dir()?;
    Ok(serde_json::json!({
        "path": dir.to_string_lossy(),
        "custom": read_custom_location().is_some(),
    }))
}

/// 设置数据目录（v1.2）：写入 exe 同级标记文件，重启生效。None = 恢复默认。
/// 不迁移已有数据——用户可手动把旧目录内容拷到新位置（db/key/eml/缓存目录）。
#[tauri::command]
fn set_data_location(path: Option<String>) -> Result<(), String> {
    let file = exe_dir()
        .ok_or_else(|| "无法定位 exe 目录".to_string())?
        .join(LOCATION_FILE);
    match path {
        Some(p) => {
            let t = p.trim();
            if t.is_empty() {
                return Err("路径不能为空".to_string());
            }
            std::fs::write(&file, t).map_err(|e| format!("写入标记文件失败：{e}"))?;
        }
        None => {
            let _ = std::fs::remove_file(&file);
        }
    }
    Ok(())
}

// ---------- 附件（v0.4） ----------

/// 前端传来的附件：内容为 base64。
#[derive(Deserialize)]
pub struct AttReq {
    pub name: String,
    pub content_type: String,
    pub data: String,
}

/// 返回给前端的已选附件文件。
#[derive(Serialize)]
pub struct AttFile {
    pub name: String,
    pub content_type: String,
    pub data: String,
}

const MAX_ATT_BYTES: u64 = 20 * 1024 * 1024;

fn decode_attachments(atts: Option<Vec<AttReq>>) -> Result<Vec<OutgoingAttachment>, String> {
    use base64::Engine as _;
    atts.unwrap_or_default()
        .into_iter()
        .map(|a| {
            let data = base64::engine::general_purpose::STANDARD
                .decode(&a.data)
                .map_err(|e| format!("附件 {} base64 解码失败：{e}", a.name))?;
            Ok(OutgoingAttachment { name: a.name, content_type: a.content_type, data })
        })
        .collect()
}

/// 读取本地文件作为待发附件（base64 回传前端，随发送/存草稿一起提交）。
#[tauri::command]
fn read_attachment_files(paths: Vec<String>) -> Result<Vec<AttFile>, String> {
    use base64::Engine as _;
    paths
        .iter()
        .map(|p| {
            let meta = std::fs::metadata(p).map_err(|e| format!("读取 {p} 失败：{e}"))?;
            if meta.len() > MAX_ATT_BYTES {
                return Err(format!("{p} 超过 20MB 限制"));
            }
            let data = std::fs::read(p).map_err(|e| format!("读取 {p} 失败：{e}"))?;
            let name = std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| p.clone());
            let content_type = mime_guess::from_path(p)
                .first_or_octet_stream()
                .to_string();
            Ok(AttFile {
                name,
                content_type,
                data: base64::engine::general_purpose::STANDARD.encode(&data),
            })
        })
        .collect()
}

// ---------- 草稿（v0.3） ----------

#[derive(Deserialize)]
pub struct DraftReq {
    pub to: Vec<String>,
    pub cc: Option<Vec<String>>,
    pub bcc: Option<Vec<String>>,
    pub subject: String,
    pub body: String,
    pub html_body: Option<String>,
    pub attachments: Option<Vec<AttReq>>,
    pub in_reply_to: Option<String>,
    pub references: Option<Vec<String>>,
}

impl DraftReq {
    fn into_draft(&self) -> crossmail_service::mail::Draft {
        use crossmail_service::mail::Draft;
        Draft::new(self.subject.clone())
            .to(self.to.iter().map(|s| parse_address(s)).collect())
            .text(self.body.clone())
    }
}

#[tauri::command]
fn save_draft(ctx: State<AppCtx>, account_id: String, req: DraftReq) -> Result<Email, String> {
    let mut draft = req.into_draft();
    draft.cc = req.cc.unwrap_or_default().iter().map(|s| parse_address(s)).collect();
    draft.bcc = req.bcc.unwrap_or_default().iter().map(|s| parse_address(s)).collect();
    draft.html_body = req.html_body;
    draft.attachments = decode_attachments(req.attachments)?;
    draft.in_reply_to = req.in_reply_to;
    draft.references = req.references.unwrap_or_default();
    ctx.mails
        .save_draft(&account_id, &draft)
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn list_drafts(ctx: State<AppCtx>, account_id: String) -> Result<Vec<Email>, String> {
    ctx.mails.list_drafts(&account_id, 100).map_err(|e| e.to_string())
}

#[tauri::command]
async fn send_draft(
    ctx: State<'_, AppCtx>,
    account_id: String,
    draft_id: String,
) -> Result<serde_json::Value, String> {
    let mails = ctx.mails.clone();
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let msg = mails.prepare_send(&draft_id).map_err(|e| e.to_string())?;
        let receipt = send_message_blocking(&account, &secret, msg).map_err(|e| e.to_string())?;
        mails
            .mark_sent(&draft_id, None)
            .map_err(|e| e.to_string())?;
        serde_json::to_value(&receipt).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

#[tauri::command]
fn delete_draft(ctx: State<AppCtx>, draft_id: String) -> Result<(), String> {
    ctx.mails.delete(&draft_id).map(|_| ()).map_err(|e| e.to_string())
}

// ---------- 服务器端搜索（v0.5，JMAP filter 直查） ----------

/// 服务器端搜索（v0.5 JMAP / v0.7 加 IMAP / v0.9 加 IMAP 逐文件夹进度事件）。
/// JMAP：Email/query filter 直查；IMAP：遍历本地已同步文件夹 SELECT + UID SEARCH，
/// 每个文件夹开始搜索前 emit `server-search-progress` {folder, found}。
#[tauri::command]
async fn server_search(
    ctx: State<'_, AppCtx>,
    app: AppHandle,
    account_id: String,
    keyword: String,
    limit: Option<u32>,
) -> Result<Vec<Email>, String> {
    let accounts = ctx.accounts.clone();
    let mails = ctx.mails.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("搜索运行时启动失败：{e}"))?;
        rt.block_on(async {
            match build_backend(&account, &secret).await.map_err(|e| e.to_string())? {
                Backend::Jmap(p) => {
                    let filter = serde_json::json!({
                        "operator": "OR",
                        "conditions": [
                            { "subject": keyword },
                            { "from": keyword },
                            { "to": keyword },
                        ]
                    });
                    let page = p
                        .client()
                        .email_query(Some(filter), limit.unwrap_or(50), 0)
                        .await
                        .map_err(|e| e.to_string())?;
                    p.client()
                        .email_get(&page.ids, true)
                        .await
                        .map_err(|e| e.to_string())
                }
                Backend::Imap(p) => {
                    // 文件夹清单取本地库（同步时 seed 的服务器文件夹名），从未同步过则只搜 INBOX
                    let mailboxes = mails
                        .list_mailboxes(&account_id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|m| m.id)
                        .collect::<Vec<_>>();
                    let mailboxes: Vec<String> = if mailboxes.is_empty() {
                        vec!["INBOX".to_string()]
                    } else {
                        mailboxes
                    };
                    let app2 = app.clone();
                    p.search_emails(
                        &mailboxes,
                        &keyword,
                        limit.unwrap_or(50) as usize,
                        Some(&|folder, found| {
                            let _ = app2.emit(
                                "server-search-progress",
                                serde_json::json!({ "folder": folder, "found": found }),
                            );
                        }),
                    )
                    .await
                    .map_err(|e| e.to_string())
                }
            }
        })
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 阅读服务器搜索结果单封（fetch_email 直取，不落本地库）。
#[tauri::command]
async fn server_read_email(
    ctx: State<'_, AppCtx>,
    account_id: String,
    email_id: String,
) -> Result<ReadEmailDto, String> {
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("读取运行时启动失败：{e}"))?;
        rt.block_on(async {
            let email = match build_backend(&account, &secret)
                .await
                .map_err(|e| e.to_string())?
            {
                Backend::Jmap(p) => {
                    p.fetch_email(&email_id, true).await.map_err(|e| e.to_string())?
                }
                Backend::Imap(p) => {
                    p.fetch_email(&email_id, true).await.map_err(|e| e.to_string())?
                }
            };
            let text = email.text_body.clone();
            let html = email.html_body.clone();
            Ok(ReadEmailDto { email, text_body: text, html_body: html, fetched_remote: true })
        })
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 服务器搜索结果的写操作（v0.6）：JMAP emailId 稳定，可直写 flags。
#[tauri::command]
async fn server_set_flag(
    ctx: State<'_, AppCtx>,
    account_id: String,
    email_id: String,
    flag: String,
    enabled: bool,
) -> Result<(), String> {
    use crossmail_core::model::Flag;
    let f = match flag.to_lowercase().as_str() {
        "seen" => Flag::Seen,
        "flagged" => Flag::Flagged,
        "answered" => Flag::Answered,
        "draft" => Flag::Draft,
        "deleted" => Flag::Deleted,
        other => Flag::Custom(other.to_string()),
    };
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("标记运行时启动失败：{e}"))?;
        rt.block_on(async {
            match build_backend(&account, &secret).await.map_err(|e| e.to_string())? {
                Backend::Jmap(p) => p.set_flags(&[email_id], f, enabled).await.map_err(|e| e.to_string()),
                Backend::Imap(p) => p.set_flags(&[email_id], f, enabled).await.map_err(|e| e.to_string()),
            }
        })
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 服务器搜索结果删除（v0.8）：复用 trait 的 `delete_emails`——
/// JMAP 走 Email/destroy；IMAP 走「移回收站」（回收站内则彻底 EXPUNGE）。
#[tauri::command]
async fn server_delete_email(
    ctx: State<'_, AppCtx>,
    account_id: String,
    email_id: String,
) -> Result<(), String> {
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("删除运行时启动失败：{e}"))?;
        rt.block_on(async {
            match build_backend(&account, &secret).await.map_err(|e| e.to_string())? {
                Backend::Jmap(p) => p.delete_emails(&[email_id]).await.map_err(|e| e.to_string()),
                Backend::Imap(p) => p.delete_emails(&[email_id]).await.map_err(|e| e.to_string()),
            }
        })
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 服务器搜索结果移动（v0.9）：复用 trait 的 `move_emails`。
/// `from_mailbox` 取邮件 `mailbox_ids[0]`（JMAP 是 patch 所需的源 mailboxId；
/// IMAP 传空则 provider 回落到 id 内嵌文件夹）。
#[tauri::command]
async fn server_move_email(
    ctx: State<'_, AppCtx>,
    account_id: String,
    email_id: String,
    from_mailbox: Option<String>,
    to_mailbox: String,
) -> Result<(), String> {
    let accounts = ctx.accounts.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let from = from_mailbox.unwrap_or_default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("移动运行时启动失败：{e}"))?;
        rt.block_on(async {
            match build_backend(&account, &secret).await.map_err(|e| e.to_string())? {
                Backend::Jmap(p) => p
                    .move_emails(&[email_id], &from, &to_mailbox)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                Backend::Imap(p) => p
                    .move_emails(&[email_id], &from, &to_mailbox)
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
            }
        })
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

/// 读取草稿在缓存中的原附件（编辑草稿带入，v0.5）。
#[tauri::command]
fn read_stored_attachments(
    ctx: State<AppCtx>,
    email_id: String,
    parts: Vec<String>,
) -> Result<Vec<AttFile>, String> {
    use base64::Engine as _;
    use crossmail_service::cache::CacheKind;
    let cache = ctx
        .mails
        .cache()
        .ok_or_else(|| "正文缓存未启用".to_string())?;
    parts
        .iter()
        .map(|part| {
            let data = cache
                .load(&email_id, CacheKind::Attachment, Some(part))
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("附件 {part} 不在缓存中"))?;
            Ok(AttFile {
                name: part.clone(),
                content_type: "application/octet-stream".to_string(),
                data: base64::engine::general_purpose::STANDARD.encode(&data),
            })
        })
        .collect()
}

#[tauri::command]
async fn push_subscribe(
    ctx: State<'_, AppCtx>,
    app: AppHandle,
    account_id: String,
) -> Result<bool, String> {
    let accounts = ctx.accounts.clone();
    let handles_arc = ctx.push_handles.clone();
    let db = ctx.db.clone();
    let app2 = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let (account, secret) = account_with_secret(&accounts, &account_id)?;
        let mut handles = handles_arc.lock().unwrap();
        // 已订阅则先取消，重建
        if let Some(old) = handles.remove(&account_id) {
            old.cancel();
        }
        let handle = subscribe_push(db, account, secret, move |notice| {
            // v1.7：新邮件数 >0 时，最小化/隐藏状态下弹托盘气泡
            let mut new_mail: Option<(u64, u64)> = None; // (inserted, unread_total)
            let dto = match notice {
                PushNotice::Synced { reason, summary, unread_total, unread_by_mailbox, coalesced } => {
                    if summary.inserted > 0 {
                        new_mail = Some((summary.inserted, unread_total));
                    }
                    PushNoticeDto::Synced {
                        reason: format!("{reason:?}"),
                        inserted: summary.inserted,
                        updated: summary.updated,
                        deleted: summary.deleted,
                        unchanged: summary.unchanged,
                        unread_total,
                        unread_by_mailbox: unread_by_mailbox.into_iter().map(|u| UnreadDto { mailbox_id: u.mailbox_id, unread: u.unread }).collect(),
                        coalesced,
                    }
                }
                PushNotice::Failed { reason, error, retry_in_ms } => {
                    PushNoticeDto::Failed { reason: format!("{reason:?}"), error, retry_in_ms }
                }
                PushNotice::Closed => {
                    PushNoticeDto::Closed { reason: "closed".into() }
                }
            };
            let _ = app2.emit("push-notice", dto);
            if let Some((inserted, unread_total)) = new_mail {
                // 仅在窗口最小化或隐藏时弹气泡；浏览状态由前端应用内提醒负责
                let bg = app2.get_webview_window("main")
                    .and_then(|w| {
                        let minimized = w.is_minimized().unwrap_or(false);
                        let visible = w.is_visible().unwrap_or(true);
                        if minimized || !visible { w.hwnd().ok() } else { None }
                    });
                if let Some(hwnd) = bg {
                    mail_notifier::notify_mail(
                        hwnd.0,
                        "CrossMail · 新邮件",
                        &format!("收到 {inserted} 封新邮件，未读共 {unread_total} 封"),
                    );
                }
            }
        })
        .map_err(|e| e.to_string())?;
        handles.insert(account_id, handle);
        Ok(true)
    })
    .await
    .map_err(|e| format!("任务失败：{e}"))?
}

#[tauri::command]
fn push_stop(ctx: State<AppCtx>, account_id: String) -> bool {
    ctx.push_handles
        .lock()
        .unwrap()
        .remove(&account_id)
        .map(|h| h.cancel())
        .unwrap_or(false)
}

// ---------- 入口 ----------

fn load_or_create_key(dir: &std::path::Path) -> Result<[u8; 32], String> {
    let path = dir.join("crossmail.key");
    if let Ok(bytes) = std::fs::read(&path) {
        if bytes.len() == 32 {
            let mut key = [0u8; 32];
            key.copy_from_slice(&bytes);
            return Ok(key);
        }
    }
    let key: [u8; 32] = rand::random();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::write(&path, key).map_err(|e| e.to_string())?;
    Ok(key)
}

/// 开机自启（v0.4）
#[tauri::command]
fn autostart_status(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt as _;
    app.autolaunch().is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
fn autostart_set(app: AppHandle, enable: bool) -> Result<bool, String> {
    use tauri_plugin_autostart::ManagerExt as _;
    let al = app.autolaunch();
    if enable {
        al.enable().map_err(|e| e.to_string())?;
    } else {
        al.disable().map_err(|e| e.to_string())?;
    }
    al.is_enabled().map_err(|e| e.to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        // 关闭按钮 → 隐藏到托盘（v0.4），真正退出走托盘菜单「退出」
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .setup(|app| {
            // 便携化（v0.2）+ 自定义数据目录（v1.2）：标记文件优先，其次 exe 同级 crossmail-data\。
            let data_dir = data_dir()?;
            std::fs::create_dir_all(&data_dir).map_err(|e| format!("无法创建数据目录：{e}"))?;
            if let Ok(old_dir) = app.path().app_data_dir() {
                for name in ["crossmail.db", "crossmail.key"] {
                    let old = old_dir.join(name);
                    let new = data_dir.join(name);
                    if old.is_file() && !new.exists() {
                        let _ = std::fs::copy(&old, &new);
                    }
                }
            }

            let key = load_or_create_key(&data_dir)?;
            // 修复（v1.0a）：打开后必须执行迁移——全新数据目录（空库）否则没有任何表，
            // 添加账号会报「no such table: accounts」。老库 migrate 幂等，重复调用无副作用。
            let db = Arc::new(Mutex::new({
                let mut d = Database::open(data_dir.join("crossmail.db"))
                    .map_err(|e| e.to_string())?;
                d.migrate().map_err(|e| e.to_string())?;
                d
            }));
            let vault = Arc::new(crossmail_service::credential::SecretVault::new(
                db.clone(),
                Arc::new(crossmail_service::credential::ChaChaSecretCodec::new(key)),
            ));
            let accounts = AccountService::new(db.clone(), vault);
            // v1.5 修复（关键）：此前用 MailService::new()（无缓存），read_body 永远查不到
            // 本地正文缓存 → 每次打开/重开邮件都重新建 TLS 连接回源拉全文，这是
            // 「已落盘 eml 的邮件重开也慢」的根因。挂上 BodyCache 后：
            // 首次阅读回源并落缓存（cache\ 目录，512MB LRU），此后重开秒开。
            let cache = Arc::new(crossmail_service::BodyCache::new(
                db.clone(),
                crossmail_service::CacheConfig::new(data_dir.join("cache"), 512 * 1024 * 1024),
            ).map_err(|e| e.to_string())?);
            let mails = MailService::with_cache(db.clone(), cache);

            app.manage(AppCtx {
                db,
                accounts: Arc::new(accounts),
                mails: Arc::new(mails),
                push_handles: Arc::new(Mutex::new(HashMap::new())),
            });

            // 托盘（v0.4）：左键唤起主窗口，右键菜单 显示/退出
            let show_item = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)
                .map_err(|e| e.to_string())?;
            let quit_item = MenuItem::with_id(app, "quit", "退出 CrossMail", true, None::<&str>)
                .map_err(|e| e.to_string())?;
            let tray_menu = Menu::with_items(app, &[&show_item, &quit_item])
                .map_err(|e| e.to_string())?;
            let icon = app
                .default_window_icon()
                .cloned()
                .ok_or_else(|| "缺少应用图标".to_string())?;
            TrayIconBuilder::with_id("crossmail-tray")
                .icon(icon)
                .tooltip("CrossMail")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        let app = tray.app_handle();
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                })
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.unminimize();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => {
                        if let Some(w) = app.get_webview_window("main") {
                            if let Ok(hwnd) = w.hwnd() {
                                mail_notifier::cleanup(hwnd.0);
                            }
                        }
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_accounts,
            create_account,
            update_account,
            delete_account,
            list_mailboxes,
            list_emails,
            search_emails,
            server_search,
            server_read_email,
            server_set_flag,
            server_delete_email,
            server_move_email,
            archive_eml,
            export_eml,
            get_data_location,
            set_data_location,
            test_account,
            test_account_update,
            create_mailbox,
            delete_mailbox,
            list_rules,
            save_rule,
            delete_rule,
            toggle_rule,
            apply_rules,
            read_stored_attachments,
            download_attachment,
            read_attachment_files,
            autostart_status,
            autostart_set,
            read_email,
            set_read,
            toggle_star,
            sync_account,
            send_email,
            save_draft,
            list_drafts,
            send_draft,
            delete_draft,
            push_subscribe,
            push_stop
        ])
        .run(tauri::generate_context!())
        .expect("CrossMail 启动失败");
}
