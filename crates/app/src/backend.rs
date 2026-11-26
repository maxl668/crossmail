//! Provider 工厂：把「账号 + 明文凭证」变成可同步的协议适配器。
//!
//! 为什么是枚举而不是 `Box<dyn MailProvider>`：`MailProvider` 有 `async fn` 且带
//! `&dyn ProgressReporter` 参数，不是 dyn 兼容的（`SyncEngine<P>` 也要求 `P: Sized`）。
//! 因此按协议分派成 [`Backend`] 两个变体，调用方 `match` 后各自把 `Arc<P>` 喂给泛型
//! `SyncEngine::new`。这是不改动已评审 trait 签名的唯一干净做法。

use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Account, ProtocolKind, SecurityMode};
use crossmail_imap::provider::{
    ImapProvider, ImapProviderConfig, SmtpAuth, TcpSmtpConnector,
};
use crossmail_imap::transport::Endpoint;
use crossmail_jmap::{AuthMethod, JmapProvider};
use std::sync::Arc;

/// 一条已按账号协议装配好的同步后端。
pub enum Backend {
    Jmap(Arc<JmapProvider>),
    Imap(Arc<ImapProvider>),
}

/// 构造后端。IMAP 需要真建连（TLS + 认证），故为异步；JMAP 的会话是懒加载，
/// 这里只装配客户端，第一次请求时才抓 session。
///
/// `secret` 是已从凭证仓解出的明文，只在函数调用期间存在。
pub async fn build_backend(account: &Account, secret: &str) -> MailResult<Backend> {
    if secret.is_empty() {
        return Err(MailError::Authentication("账号凭证为空，无法连接".into()));
    }
    match account.protocol {
        ProtocolKind::Jmap => {
            let cfg = account.jmap.as_ref().ok_or_else(|| {
                MailError::InvalidInput(format!("账号 {} 声明为 JMAP 但没有端点配置", account.id))
            })?;
            if cfg.endpoint.trim().is_empty() {
                return Err(MailError::InvalidInput("JMAP 端点不能为空".into()));
            }
            // 真机 JMAP 只用 Basic（用户 = 邮箱地址）。
            let auth = AuthMethod::basic(account.email_address.clone(), secret.to_string());
            let provider =
                JmapProvider::new(cfg.endpoint.clone(), auth, account.email_address.clone());
            Ok(Backend::Jmap(Arc::new(provider)))
        }
        ProtocolKind::Imap => {
            let cfg = account.imap.as_ref().ok_or_else(|| {
                MailError::InvalidInput(format!(
                    "账号 {} 声明为 IMAP 但没有主机/端口配置",
                    account.id
                ))
            })?;
            // IMAP 传输层目前只实现隐式 TLS（993）；STARTTLS 需明文握手后升级，
            // 尚未实现。静默拿 143 当隐式 TLS 连会报一堆看不懂的握手错，这里明确挡掉。
            if cfg.security == SecurityMode::StartTls {
                return Err(MailError::Unsupported(
                    "IMAP STARTTLS 尚未实现，请使用隐式 TLS（993）".into(),
                ));
            }
            let ep = Endpoint::new(cfg.host.clone(), cfg.port);
            let pcfg = ImapProviderConfig::new(
                account.id.clone(),
                account.email_address.clone(),
                account.email_address.clone(), // 登录名取邮箱地址（真机一致）
                secret.to_string(),
            );
            let provider = ImapProvider::connect(&ep, &account.email_address, secret, pcfg).await?;
            // SMTP 通道必须在这里挂上：`send_email` 只看 `ImapProvider::smtp`，
            // 不装配时它一律返回「该账号未配置 SMTP 发信」——即使账号里已经有
            // 完整的 smtp 配置。这条曾经没人调用 `with_smtp`，导致发信能力在
            // 编排层整段空转（探针实证：`ORCHESTRATION_SEND_ERR=不支持的能力:
            // 该账号未配置 SMTP 发信`）。
            let provider = match account.smtp.as_ref() {
                Some(smtp) => provider.with_smtp(SmtpAuth {
                    connector: Box::new(TcpSmtpConnector::new(
                        smtp.host.clone(),
                        smtp.security,
                    )),
                    username: account.email_address.clone(),
                    secret: secret.to_string(),
                }),
                // 没有 SMTP 配置就是纯收信账号（只读客户端），保持原样。
                None => provider,
            };
            Ok(Backend::Imap(Arc::new(provider)))
        }
    }
}
