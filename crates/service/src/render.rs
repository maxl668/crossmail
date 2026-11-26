//! 安全渲染（S6-01 / S6-02）：把「读正文」升级成「读正文 + 净化 + 资源策略 + 钓鱼提示」。
//!
//! 业务规则在这里收口，UI 只负责展示：
//! * 远程图片是否放行，由**发件人放行白名单**决定，不由 UI 自由开关；
//! * 「单次放行」写成短期记录（默认 10 分钟），刷新页面仍然生效，过期自动失效，
//!   这样用户不必每滑动一次就点一次「显示图片」；
//! * 发件人信任评估每次都实时算（域名会变，缓存结论比不缓存更危险）。

use chrono::{Duration, Utc};
use crossmail_core::{Address, MailError, MailResult};
use crossmail_sanitize::{
    analyze_sender, sanitize, SanitizePolicy, SanitizeReport, SenderTrust, TrustLevel,
};
use crossmail_storage::{AllowanceRow, Database};
use std::sync::{Arc, Mutex};

use crate::lock_db;
use crate::mail::MailService;

/// 「单次放行」的存活时长：够用户把这封邮件看完，又不至于明天还在放行。
pub const ONCE_TTL_MINUTES: i64 = 10;

/// 放行范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AllowScope {
    /// 单次：短期有效，过期自动失效
    Once,
    /// 该发件人邮箱永久放行
    Sender,
    /// 该发件人可注册域永久放行（同一家公司后续邮件都不用再点）
    Domain,
}

impl AllowScope {
    pub fn as_str(self) -> &'static str {
        match self {
            AllowScope::Once => "once",
            AllowScope::Sender => "sender",
            AllowScope::Domain => "domain",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "once" => AllowScope::Once,
            "sender" => AllowScope::Sender,
            "domain" => AllowScope::Domain,
            _ => return None,
        })
    }
}

/// 本次渲染的资源许可。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AssetPermission {
    /// 按白名单决定（默认）
    #[default]
    Inherit,
    /// 本次强制放行（用户点了「显示图片」但没勾「以后都显示」）
    AllowOnce,
    /// 强制阻断（用户手动关掉，优先级最高）
    ForceBlock,
}

/// 渲染请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderRequest {
    /// true 优先 HTML，false 走纯文本
    pub prefer_html: bool,
    pub permission: AssetPermission,
}

impl Default for RenderRequest {
    fn default() -> Self {
        Self {
            prefer_html: true,
            permission: AssetPermission::Inherit,
        }
    }
}

impl RenderRequest {
    pub fn html() -> Self {
        Self::default()
    }

    pub fn text() -> Self {
        Self {
            prefer_html: false,
            ..Self::default()
        }
    }

    pub fn with_permission(mut self, permission: AssetPermission) -> Self {
        self.permission = permission;
        self
    }
}

/// 渲染结果。
#[derive(Debug, Clone)]
pub struct RenderedBody {
    pub email_id: String,
    /// 已净化的 HTML（`prefer_html = false` 时为 None）
    pub html: Option<String>,
    /// 纯文本视图。HTML 渲染时由净化后的内容派生，保证两者内容一致。
    pub text: Option<String>,
    pub trust: SenderTrust,
    /// 净化统计（纯文本渲染时为 None）
    pub sanitize: Option<SanitizeReport>,
    /// 本次是否放行了远程图片
    pub remote_images_allowed: bool,
    /// 被拦截的资源地址，UI 用它显示「已阻止 N 项远程内容」
    pub blocked_urls: Vec<String>,
}

impl RenderedBody {
    pub fn blocked_count(&self) -> usize {
        self.sanitize.as_ref().map(|r| r.blocked.len()).unwrap_or(0)
    }

    /// UI 是否应显示安全提示条
    pub fn needs_security_bar(&self) -> bool {
        self.blocked_count() > 0 || self.trust.level != TrustLevel::Safe
    }
}

/// 渲染服务。
pub struct RenderService {
    db: Arc<Mutex<Database>>,
    mail: MailService,
}

impl RenderService {
    pub fn new(db: Arc<Mutex<Database>>) -> Self {
        Self {
            mail: MailService::new(Arc::clone(&db)),
            db,
        }
    }

    pub fn with_mail(db: Arc<Mutex<Database>>, mail: MailService) -> Self {
        Self { db, mail }
    }

    /// 读并渲染正文。
    pub fn render(&self, email_id: &str, req: &RenderRequest) -> MailResult<RenderedBody> {
        let email = self.mail.read(email_id)?;
        let trust = analyze_sender(
            email.from.first().unwrap_or(&Address::new("")),
            &email.reply_to,
        );

        let allowed = match req.permission {
            AssetPermission::ForceBlock => false,
            AssetPermission::AllowOnce => true,
            AssetPermission::Inherit => self.is_allowed(&trust)?,
        };

        if !req.prefer_html {
            let text = self.mail.read_body(email_id, false)?;
            return Ok(RenderedBody {
                email_id: email_id.to_string(),
                html: None,
                text,
                trust,
                sanitize: None,
                remote_images_allowed: allowed,
                blocked_urls: Vec::new(),
            });
        }

        let raw = self.mail.read_body(email_id, true)?;
        let Some(raw) = raw else {
            return Ok(RenderedBody {
                email_id: email_id.to_string(),
                html: None,
                text: None,
                trust,
                sanitize: None,
                remote_images_allowed: allowed,
                blocked_urls: Vec::new(),
            });
        };

        let policy = if allowed {
            SanitizePolicy::default().with_remote_images()
        } else {
            SanitizePolicy::default()
        };
        let sanitized = sanitize(&raw, &policy);
        let blocked_urls = sanitized
            .report
            .blocked
            .iter()
            .map(|b| b.url.clone())
            .collect();
        Ok(RenderedBody {
            email_id: email_id.to_string(),
            html: Some(sanitized.html),
            text: Some(sanitized.plain),
            trust,
            sanitize: Some(sanitized.report),
            remote_images_allowed: allowed,
            blocked_urls,
        })
    }

    /// 只看发件人信任（列表页/悬停提示用，不做正文 IO）。
    pub fn trust(&self, email_id: &str) -> MailResult<SenderTrust> {
        let email = self.mail.read(email_id)?;
        Ok(analyze_sender(
            email.from.first().unwrap_or(&Address::new("")),
            &email.reply_to,
        ))
    }

    /// 放行发件人的远程资源。返回写入白名单用的键。
    pub fn allow(&self, email_id: &str, scope: AllowScope) -> MailResult<String> {
        let trust = self.trust(email_id)?;
        let (key, expires_at) = match scope {
            AllowScope::Once => (
                trust.address.clone(),
                Some(Utc::now() + Duration::minutes(ONCE_TTL_MINUTES)),
            ),
            AllowScope::Sender => (trust.address.clone(), None),
            AllowScope::Domain => {
                if trust.registrable_domain.is_empty() {
                    return Err(MailError::InvalidInput(
                        "该发件人没有可识别的域名，无法按域名放行".into(),
                    ));
                }
                (trust.registrable_domain.clone(), None)
            }
        };
        lock_db(&self.db)?.put_allowance(&key, scope.as_str(), Utc::now(), expires_at)?;
        Ok(key)
    }

    /// 撤销放行（单次 / 发件人 / 域名都清掉）。
    pub fn revoke(&self, email_id: &str) -> MailResult<usize> {
        let trust = self.trust(email_id)?;
        let db = lock_db(&self.db)?;
        let mut removed = db.delete_allowance(&trust.address)?;
        if !trust.registrable_domain.is_empty() {
            removed += db.delete_allowance(&trust.registrable_domain)?;
        }
        Ok(removed)
    }

    /// 当前白名单（设置页展示）。
    pub fn allowances(&self) -> MailResult<Vec<AllowanceRow>> {
        let db = lock_db(&self.db)?;
        db.purge_expired_allowances(Utc::now())?;
        db.list_allowances()
    }

    /// 该发件人是否在白名单内（含过期清理）。
    pub fn is_allowed(&self, trust: &SenderTrust) -> MailResult<bool> {
        let db = lock_db(&self.db)?;
        db.purge_expired_allowances(Utc::now())?;
        if !trust.address.is_empty() && db.get_allowance(&trust.address)?.is_some() {
            return Ok(true);
        }
        if !trust.registrable_domain.is_empty()
            && db.get_allowance(&trust.registrable_domain)?.is_some()
        {
            return Ok(true);
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{BodyCache, CacheConfig, CacheKind};
    use crossmail_core::{Email, Flag};
    use crossmail_storage::Database;

    struct Fixture {
        db: Arc<Mutex<Database>>,
        cache: Arc<BodyCache>,
        svc: RenderService,
        dir: std::path::PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let mut db = Database::open_in_memory().unwrap();
            db.migrate().unwrap();
            let db = Arc::new(Mutex::new(db));
            let dir =
                std::env::temp_dir().join(format!("crossmail-render-{}-{tag}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let cache = Arc::new(
                BodyCache::new(
                    Arc::clone(&db),
                    CacheConfig::new(dir.join("cache"), 8 * 1024 * 1024),
                )
                .unwrap(),
            );
            let mail = MailService::with_cache(Arc::clone(&db), Arc::clone(&cache));
            let svc = RenderService::with_mail(Arc::clone(&db), mail);
            Self {
                db,
                cache,
                svc,
                dir,
            }
        }

        /// 建账号/文件夹/邮件元数据，正文落缓存（真实链路里正文本来就只在缓存/文件里）。
        fn seed(&self, id: &str, from: &str, html: &str) {
            {
                let db = self.db.lock().unwrap();
                db.upsert_account(&crossmail_core::Account::new(
                    "acc1",
                    "me@example.com",
                    crossmail_core::ProtocolKind::Imap,
                ))
                .unwrap();
                let mut box_ = crossmail_core::Mailbox::new("mb1", "acc1", "INBOX");
                box_.role = crossmail_core::MailboxRole::Inbox;
                db.upsert_mailbox(&box_).unwrap();
                let mut email = Email::new(id, "acc1", "mb1");
                email.from = vec![Address::new(from)];
                email.subject = "s".into();
                email.flags = vec![Flag::Seen];
                db.upsert_email(&email).unwrap();
            }
            // 锁必须在上一条语句结束时已释放：BodyCache::store 内部要拿同一把锁
            self.cache
                .store(id, CacheKind::HtmlBody, None, html.as_bytes())
                .unwrap();
        }
    }

    #[test]
    fn blocks_remote_images_by_default() {
        let f = Fixture::new("block");
        f.seed(
            "e1",
            "news@shop.example",
            r#"<p>hi</p><img src="https://t.example.com/p.gif">"#,
        );
        let out = f.svc.render("e1", &RenderRequest::html()).unwrap();
        assert!(!out.remote_images_allowed);
        assert_eq!(out.blocked_count(), 1);
        assert!(out.needs_security_bar());
        let html = out.html.unwrap();
        assert!(!html.contains("t.example.com"));
        assert!(html.contains("data:image/gif"));
        assert_eq!(out.blocked_urls, vec!["https://t.example.com/p.gif"]);
    }

    #[test]
    fn allow_once_permits_images_without_persisting() {
        let f = Fixture::new("once");
        f.seed(
            "e1",
            "news@shop.example",
            r#"<img src="https://cdn.example.com/a.png">"#,
        );
        let out = f
            .svc
            .render(
                "e1",
                &RenderRequest::html().with_permission(AssetPermission::AllowOnce),
            )
            .unwrap();
        assert!(out.remote_images_allowed);
        assert_eq!(out.blocked_count(), 0);
        assert!(out.html.unwrap().contains("https://cdn.example.com/a.png"));
        // 单次放行不落库
        assert!(f.svc.allowances().unwrap().is_empty());
    }

    #[test]
    fn permanent_allowance_applies_to_later_renders() {
        let f = Fixture::new("perm");
        f.seed(
            "e1",
            "news@shop.example",
            r#"<img src="https://cdn.example.com/a.png">"#,
        );
        let key = f.svc.allow("e1", AllowScope::Sender).unwrap();
        assert_eq!(key, "news@shop.example");
        let out = f.svc.render("e1", &RenderRequest::html()).unwrap();
        assert!(out.remote_images_allowed);
        assert_eq!(f.svc.allowances().unwrap().len(), 1);

        // 撤销后回到默认阻断
        f.svc.revoke("e1").unwrap();
        let out = f.svc.render("e1", &RenderRequest::html()).unwrap();
        assert!(!out.remote_images_allowed);
    }

    #[test]
    fn domain_scope_covers_other_senders_of_same_company() {
        let f = Fixture::new("domain");
        f.seed(
            "e1",
            "news@shop.example",
            r#"<img src="https://cdn.example.com/a.png">"#,
        );
        f.seed(
            "e2",
            "billing@shop.example",
            r#"<img src="https://cdn.example.com/b.png">"#,
        );
        f.svc.allow("e1", AllowScope::Domain).unwrap();
        let out = f.svc.render("e2", &RenderRequest::html()).unwrap();
        assert!(out.remote_images_allowed, "同域发件人应继承放行");
    }

    #[test]
    fn force_block_wins_over_allowance() {
        let f = Fixture::new("force");
        f.seed(
            "e1",
            "news@shop.example",
            r#"<img src="https://cdn.example.com/a.png">"#,
        );
        f.svc.allow("e1", AllowScope::Sender).unwrap();
        let out = f
            .svc
            .render(
                "e1",
                &RenderRequest::html().with_permission(AssetPermission::ForceBlock),
            )
            .unwrap();
        assert!(!out.remote_images_allowed);
        assert_eq!(out.blocked_count(), 1);
    }

    #[test]
    fn expired_once_allowance_is_purged() {
        let f = Fixture::new("expired");
        f.seed("e1", "news@shop.example", "<p>x</p>");
        {
            // 直接写一条已过期的记录，模拟「昨天点的显示图片」
            let db = f.db.lock().unwrap();
            db.put_allowance(
                "news@shop.example",
                "once",
                Utc::now() - Duration::hours(1),
                Some(Utc::now() - Duration::minutes(1)),
            )
            .unwrap();
        }
        assert!(f.svc.allowances().unwrap().is_empty(), "过期记录应被清理");
        let out = f.svc.render("e1", &RenderRequest::html()).unwrap();
        assert!(!out.remote_images_allowed);
    }

    #[test]
    fn text_mode_returns_plain_text_only() {
        let f = Fixture::new("text");
        f.seed("e1", "a@b.example", "<p>hello</p>");
        let out = f.svc.render("e1", &RenderRequest::text()).unwrap();
        assert!(out.html.is_none());
        assert!(out.sanitize.is_none());
    }

    #[test]
    fn html_mode_derives_matching_plain_text() {
        let f = Fixture::new("plain");
        f.seed("e1", "a@b.example", "<p>第一行</p><p>第二行</p>");
        let out = f.svc.render("e1", &RenderRequest::html()).unwrap();
        let text = out.text.unwrap();
        assert!(text.contains("第一行") && text.contains("第二行"));
    }

    #[test]
    fn empty_sender_does_not_raise_alarm() {
        // 回归：草稿缺 From 时曾被判成「IP 直连」丁 danger，安全提示条常驻
        let f = Fixture::new("nosender");
        f.seed("e1", "", "<p>x</p>");
        let out = f.svc.render("e1", &RenderRequest::html()).unwrap();
        assert_eq!(out.trust.level, TrustLevel::Safe);
        assert!(out.trust.summary().is_empty());
    }

    #[test]
    fn phishing_warning_surfaces_without_body() {
        let f = Fixture::new("phish");
        f.seed("e1", "service@xn--80ak6aa92e.com", "<p>x</p>");
        let trust = f.svc.trust("e1").unwrap();
        assert!(trust.suspicious());
        assert_eq!(trust.level, TrustLevel::Danger);
    }
}
