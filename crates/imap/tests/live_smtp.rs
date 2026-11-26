//! SMTP 真发信（I-009，QQ 邮箱 smtp.qq.com:465）+ QQ IMAP 拉回收件箱验证到达。
//!
//! 与 live_server 同款环境变量门控：没设凭据就静默跳过，CI 与离线开发不受影响。
//! 环境变量（由开发者本机私有的凭据文件解出；该密钥文件不进版本库）：
//! ```text
//! CROSSMAIL_LIVE_QQ_SMTP_HOST  默认 smtp.qq.com
//! CROSSMAIL_LIVE_QQ_SMTP_PORT  默认 465（隐式 TLS）
//! CROSSMAIL_LIVE_QQ_SMTP_USER  发件邮箱（授权码与该邮箱绑定）
//! CROSSMAIL_LIVE_QQ_SMTP_CODE  SMTP 授权码（不是 QQ 密码）
//! CROSSMAIL_LIVE_QQ_IMAP_HOST  默认 imap.qq.com（收件验证用，同一授权码）
//! ```
//!
//! 测试设计为**自发自收**：发件箱发给自己，再用 QQ IMAP 拉回收件箱确认邮件
//! 真实到达，最后删除清理——不依赖第三方收件人，不产生外部打扰。

use crossmail_core::error::ErrorKind;
use crossmail_core::model::{Address, MailboxRole, OutgoingMessage};
use crossmail_core::provider::{CancellationToken, MailProvider, SyncRequest};
use crossmail_imap::smtp::{self, SmtpClient};
use crossmail_imap::{connect_tls_endpoint, Endpoint, ImapProvider, ImapProviderConfig};
use std::time::Duration;

struct QqConfig {
    smtp_host: String,
    smtp_port: u16,
    user: String,
    code: String,
}

/// 读取 QQ SMTP 凭据；缺关键项返回 None（=跳过）。
fn qq_config() -> Option<QqConfig> {
    let user = std::env::var("CROSSMAIL_LIVE_QQ_SMTP_USER").ok()?;
    let code = std::env::var("CROSSMAIL_LIVE_QQ_SMTP_CODE").ok()?;
    if user.trim().is_empty() || code.trim().is_empty() {
        return None;
    }
    let smtp_port = std::env::var("CROSSMAIL_LIVE_QQ_SMTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(465);
    Some(QqConfig {
        smtp_host: std::env::var("CROSSMAIL_LIVE_QQ_SMTP_HOST")
            .ok()
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| "smtp.qq.com".into()),
        smtp_port,
        user,
        code,
    })
}

/// 建立已认证的 SMTP 客户端（465 隐式 TLS）。
async fn smtp_login(cfg: &QqConfig) -> SmtpClient<dyn smtp::SmtpIo> {
    let io = connect_tls_endpoint(
        &Endpoint::new(&cfg.smtp_host, cfg.smtp_port).with_timeout(Duration::from_secs(20)),
    )
    .await
    .expect("SMTP 建连（TLS）失败");
    let mut client = SmtpClient::new(Box::new(io));
    client.greet().await.expect("SMTP EHLO 失败");
    client
        .authenticate(&cfg.user, &cfg.code)
        .await
        .expect("SMTP 认证失败（检查授权码）");
    client
}

/// QQ IMAP（收件验证用，与 SMTP 同一授权码）。
async fn qq_imap(cfg: &QqConfig) -> ImapProvider {
    let imap_host = std::env::var("CROSSMAIL_LIVE_QQ_IMAP_HOST")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .unwrap_or_else(|| "imap.qq.com".into());
    let endpoint = Endpoint::new(imap_host, 993).with_timeout(Duration::from_secs(20));
    let provider_cfg = ImapProviderConfig::new(
        "qq-live-acct",
        cfg.user.clone(),
        cfg.user.clone(),
        cfg.code.clone(),
    );
    ImapProvider::connect(&endpoint, &cfg.user, &cfg.code, provider_cfg)
        .await
        .expect("QQ IMAP 建连 + 认证失败")
}

/// 自发自收一封测试邮件；返回主题 tag。
async fn send_test_mail(cfg: &QqConfig, client: &mut SmtpClient<dyn smtp::SmtpIo>) -> String {
    let tag = format!("CrossMail-smtp-{}", std::process::id());
    let mut msg = OutgoingMessage::new(
        "qq-live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} subject"),
    );
    msg.text_body = Some("crossmail SMTP live integration body".into());
    client.send_message(&msg).await.expect("SMTP DATA 发送失败");
    tag
}

/// 在 QQ 收件箱里轮询找主题含 tag 的邮件（QQ 自投递通常秒级，留足 90s）。
async fn wait_in_inbox(p: &ImapProvider, tag: &str) -> Option<String> {
    let inbox = p
        .list_mailboxes()
        .await
        .expect("QQ IMAP LIST 失败")
        .into_iter()
        .find(|m| m.role == MailboxRole::Inbox)
        .expect("QQ 邮箱缺 INBOX 角色");
    for _ in 0..18 {
        // QQ 不声明 CONDSTORE，增量游标意义不大：每轮直接全量拉收件箱（上限 50）
        let mut req = SyncRequest::new("qq-live-acct");
        req.mailbox_id = Some(inbox.id.clone());
        req.limit = 50;
        let page = p
            .sync_emails(req, CancellationToken::new(), None)
            .await
            .expect("QQ IMAP 同步收件箱失败");
        if let Some(mail) = page.emails.iter().find(|e| e.subject.contains(tag)) {
            return Some(mail.id.clone());
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    None
}

#[tokio::test]
async fn live_smtp_send_and_arrives_in_inbox() {
    let cfg = match qq_config() {
        Some(c) => c,
        None => {
            eprintln!("跳过：未设置 CROSSMAIL_LIVE_QQ_SMTP_USER/CODE（真联调需要 QQ 授权码）");
            return;
        }
    };

    // 1. SMTP 真发：465 隐式 TLS → EHLO → AUTH → DATA，250 即服务端接收
    let mut client = smtp_login(&cfg).await;
    let tag = send_test_mail(&cfg, &mut client).await;
    println!("SMTP 发出，tag={tag}");

    // 2. QQ IMAP 拉回收件箱：证明邮件不止被 SMTP 接受，还真实投递进了邮箱
    let p = qq_imap(&cfg).await;
    let mail_id = wait_in_inbox(&p, &tag)
        .await
        .expect("90s 内收件箱没有出现刚发出的测试邮件");
    println!("收件箱已到达，id={mail_id}");

    // 3. 清理：删除测试邮件（QQ 语义进「已删除」，不污染收件箱即可）
    p.delete_emails(std::slice::from_ref(&mail_id))
        .await
        .expect("清理测试邮件失败");
}

#[tokio::test]
async fn live_smtp_wrong_code_is_authentication_error() {
    let mut cfg = match qq_config() {
        Some(c) => c,
        None => {
            eprintln!("跳过：未设置 CROSSMAIL_LIVE_QQ_SMTP_USER/CODE");
            return;
        }
    };
    cfg.code = format!("{}wrong", &cfg.code[..cfg.code.len().saturating_sub(5)]);
    let io = connect_tls_endpoint(
        &Endpoint::new(&cfg.smtp_host, cfg.smtp_port).with_timeout(Duration::from_secs(20)),
    )
    .await
    .expect("SMTP 建连失败");
    let mut client = SmtpClient::new(Box::new(io));
    client.greet().await.expect("SMTP EHLO 失败");
    let err = client
        .authenticate(&cfg.user, &cfg.code)
        .await
        .expect_err("错授权码居然认证成功");
    assert!(
        matches!(err.kind(), ErrorKind::Authentication),
        "错授权码应归类为认证错误，实际: {err:?}"
    );
}
