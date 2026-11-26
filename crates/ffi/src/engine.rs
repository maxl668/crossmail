//! FFI 引擎门面（S5-01）：把业务层能力收敛成一组对 UI 友好的同步接口。
//!
//! 设计取舍：
//! * **只做转发与转换**，不复制业务规则。所有校验、加密、缓存策略都在
//!   `crossmail-service` 里，避免同一套规则在两层各写一遍然后慢慢分叉。
//! * 引擎内部持有 `Arc<Mutex<Database>>`（SQLite 连接非 Sync），所有方法都是
//!   短锁访问，禁止跨方法嵌套持锁——否则会自锁（S3/S4 已经踩过两次）。

use crate::dto::*;
use crate::push::{PushSubscription, SyncReportDto};
use crate::PushObserver;
use crossmail_core::model as core;
use crossmail_core::provider::CancellationToken;
use crossmail_obs::{LogFilter, LogLevel, Logger, RotateConfig};
use crossmail_service::{
    AccountService, AllowScope, AssetPermission, BodyCache, CacheConfig, ChaChaSecretCodec,
    DiagnosticsService, MailService, RenderRequest, RenderService, SearchService, SecretCodec,
    SecretVault,
};
use crossmail_storage::Database;
use std::sync::{Arc, Mutex};

/// 跨平台可调用的引擎句柄。UI 侧一次 `open_engine`，之后全程复用。
pub struct CrossMailEngine {
    accounts: AccountService,
    mail: MailService,
    search: SearchService,
    cache: Arc<BodyCache>,
    render: RenderService,
    diagnostics: DiagnosticsService,
    logger: Arc<Logger>,
    /// 编排层（crossmail-app）同步/推送要用的同一份库连接。
    /// 与各 service 共享 `Arc<Mutex<Database>>`，短锁访问，不跨方法持锁。
    db: Arc<Mutex<Database>>,
}

impl CrossMailEngine {
    // ------------------------------------------------------------ 账号

    pub fn list_accounts(&self) -> FfiResult<Vec<AccountDto>> {
        let list = self.accounts.list().map_err(CrossMailError::from)?;
        Ok(list.iter().map(account_to_dto).collect())
    }

    pub fn get_account(&self, account_id: String) -> FfiResult<AccountDto> {
        let account = self
            .accounts
            .get(&account_id)
            .map_err(CrossMailError::from)?;
        Ok(account_to_dto(&account))
    }

    pub fn create_account(&self, input: NewAccountDto) -> FfiResult<AccountDto> {
        let new_account = new_account_from_dto(&input)?;
        let account = self
            .accounts
            .create(new_account)
            .map_err(CrossMailError::from)?;
        Ok(account_to_dto(&account))
    }

    pub fn update_account(
        &self,
        account_id: String,
        patch: AccountPatchDto,
    ) -> FfiResult<AccountDto> {
        let patch = account_patch_from_dto(&patch)?;
        let account = self
            .accounts
            .update(&account_id, patch)
            .map_err(CrossMailError::from)?;
        Ok(account_to_dto(&account))
    }

    pub fn delete_account(&self, account_id: String) -> FfiResult<()> {
        self.accounts
            .delete(&account_id)
            .map_err(CrossMailError::from)
    }

    pub fn rotate_secret(&self, account_id: String, secret: String) -> FfiResult<()> {
        self.accounts
            .rotate_secret(&account_id, &secret)
            .map_err(CrossMailError::from)
    }

    // ------------------------------------------------------------ 文件夹

    pub fn list_mailboxes(&self, account_id: String) -> FfiResult<Vec<MailboxDto>> {
        let list = self
            .mail
            .list_mailboxes(&account_id)
            .map_err(CrossMailError::from)?;
        Ok(list.iter().map(mailbox_to_dto).collect())
    }

    /// 补齐标准系统文件夹。
    ///
    /// 为什么需要它：`archive` / `delete` 依赖 Archive / Trash 角色存在，账号刚建好
    /// 还没同步时这些文件夹都不在，UI 点「归档」会直接报「没有归档文件夹」。
    /// 这里按角色建本地占位，等服务端同步回来再按真实 id 对齐。
    pub fn ensure_system_mailboxes(&self, account_id: String) -> FfiResult<Vec<MailboxDto>> {
        for (role, name) in [
            (core::MailboxRole::Inbox, "Inbox"),
            (core::MailboxRole::Drafts, "Drafts"),
            (core::MailboxRole::Sent, "Sent"),
            (core::MailboxRole::Trash, "Trash"),
            (core::MailboxRole::Archive, "Archive"),
            (core::MailboxRole::Junk, "Junk"),
        ] {
            self.mail
                .ensure_mailbox(&account_id, role, name)
                .map_err(CrossMailError::from)?;
        }
        self.list_mailboxes(account_id)
    }

    pub fn unread_counts(&self, account_id: String) -> FfiResult<Vec<MailboxCountDto>> {
        let counts = self
            .mail
            .unread_counts(&account_id)
            .map_err(CrossMailError::from)?;
        Ok(counts
            .into_iter()
            .map(|(mailbox_id, unread)| MailboxCountDto {
                mailbox_id,
                unread: unread as u64,
            })
            .collect())
    }

    // ------------------------------------------------------------ 邮件

    pub fn list_emails(&self, account_id: String, query: EmailQueryDto) -> FfiResult<EmailPageDto> {
        let q = email_query_from_dto(&query);
        let page = self
            .mail
            .list(&account_id, &q)
            .map_err(CrossMailError::from)?;
        Ok(EmailPageDto {
            items: page.items.iter().map(email_to_dto).collect(),
            total: page.total,
            next_offset: page.next_offset,
        })
    }

    pub fn get_email(&self, email_id: String) -> FfiResult<EmailDto> {
        let email = self.mail.read(&email_id).map_err(CrossMailError::from)?;
        Ok(email_to_dto(&email))
    }

    /// 本地没有正文缓存时，按 I-005 的「按需回源」语义取一次并落缓存。
    ///
    /// 同步阶段只落元数据，正文要等用户真正打开邮件才拉。失败只记日志并退回
    /// 原来的空正文结果：离线状态下点开一封邮件不该让整个阅读窗格报错。
    /// 取回后 `MailService::save_body` 同时回写 `body_path`，下次读直接命中缓存。
    fn ensure_body(&self, email_id: &str) {
        if matches!(self.mail.read_body(email_id, false), Ok(Some(_))) {
            return;
        }
        let result = (|| -> FfiResult<()> {
            let email = self.mail.read(email_id).map_err(CrossMailError::from)?;
            let account = self
                .accounts
                .get(&email.account_id)
                .map_err(CrossMailError::from)?;
            let secret = self
                .accounts
                .resolve_secret(&account)
                .map_err(CrossMailError::from)?;
            let full = crossmail_app::fetch_email_blocking(&account, &secret, email_id)
                .map_err(CrossMailError::from)?;
            self.mail
                .save_body(
                    email_id,
                    full.text_body.as_deref(),
                    full.html_body.as_deref(),
                )
                .map_err(CrossMailError::from)?;
            Ok(())
        })();
        match result {
            Ok(()) => self
                .logger
                .info("body", format!("正文已回源落缓存 email={email_id}")),
            Err(e) => self
                .logger
                .warn("body", format!("正文回源失败 email={email_id}: {e}")),
        }
    }

    /// 读正文：优先返回请求的格式，缺失时回退到另一种（
    /// 很多邮件只有纯文本或只有 HTML，UI 不该因为格式不对就显示空白）。
    pub fn read_body(&self, email_id: String, prefer_html: bool) -> FfiResult<Option<String>> {
        self.ensure_body(&email_id);
        self.mail
            .read_body(&email_id, prefer_html)
            .map_err(CrossMailError::from)
    }

    pub fn set_read(&self, email_id: String, read: bool) -> FfiResult<EmailDto> {
        let email = self
            .mail
            .set_read(&email_id, read)
            .map_err(CrossMailError::from)?;
        Ok(email_to_dto(&email))
    }

    pub fn toggle_star(&self, email_id: String, starred: bool) -> FfiResult<EmailDto> {
        let email = self
            .mail
            .toggle_star(&email_id, starred)
            .map_err(CrossMailError::from)?;
        Ok(email_to_dto(&email))
    }

    pub fn move_email(&self, email_id: String, mailbox_id: String) -> FfiResult<EmailDto> {
        let email = self
            .mail
            .move_to(&email_id, &mailbox_id)
            .map_err(CrossMailError::from)?;
        Ok(email_to_dto(&email))
    }

    pub fn archive_email(&self, email_id: String) -> FfiResult<EmailDto> {
        let email = self.mail.archive(&email_id).map_err(CrossMailError::from)?;
        Ok(email_to_dto(&email))
    }

    pub fn delete_email(&self, email_id: String) -> FfiResult<DeleteOutcomeDto> {
        let outcome = self.mail.delete(&email_id).map_err(CrossMailError::from)?;
        Ok(DeleteOutcomeDto {
            email_id: outcome.email_id,
            trashed: outcome.trashed,
        })
    }

    // ------------------------------------------------------------ 草稿

    pub fn save_draft(&self, account_id: String, draft: DraftDto) -> FfiResult<EmailDto> {
        let draft = draft_from_dto(&draft);
        let email = self
            .mail
            .save_draft(&account_id, &draft)
            .map_err(CrossMailError::from)?;
        Ok(email_to_dto(&email))
    }

    pub fn list_drafts(&self, account_id: String, limit: u32) -> FfiResult<Vec<EmailDto>> {
        let limit = if limit == 0 { 50 } else { limit };
        let list = self
            .mail
            .list_drafts(&account_id, limit)
            .map_err(CrossMailError::from)?;
        Ok(list.iter().map(email_to_dto).collect())
    }

    pub fn mark_sent(
        &self,
        draft_id: String,
        sent_mailbox_id: Option<String>,
    ) -> FfiResult<EmailDto> {
        let email = self
            .mail
            .mark_sent(&draft_id, sent_mailbox_id.as_deref())
            .map_err(CrossMailError::from)?;
        Ok(email_to_dto(&email))
    }

    // ------------------------------------------------------------ 发信

    /// 发信：本地草稿 → 协议提交通道 → 本地转「已发送」。
    ///
    /// 顺序是有意为之的三段：
    /// 1. **先落草稿**。SMTP 认证失败、连不上、被服务器拒收都是常态故障，
    ///    此时用户敲的正文不能在内存里蒸发掉；草稿留在草稿箱，改完可直接重发。
    /// 2. **再真投递**。任何失败都原样抛给 UI（不吞成「已发送」），日志只记
    ///    账号/草稿 id，不记正文与凭证。
    /// 3. **最后本地记账**。清 Draft 标志、打 Seen、落到「已发送」文件夹；
    ///    账号还没有该文件夹时先补本地占位，避免这封邮件在列表里彻底消失。
    ///
    /// 已知边界（不是 bug）：IMAP 只做 SMTP 提交，不会 APPEND 一份到服务端
    /// Sent 目录，服务端「已发送」要等下次同步由服务器自身行为补齐。
    pub fn send_email(&self, account_id: String, draft: DraftDto) -> FfiResult<SendReceiptDto> {
        let account = self
            .accounts
            .get(&account_id)
            .map_err(CrossMailError::from)?;
        let message = outgoing_from_draft(&account, &draft);
        message
            .validate()
            .map_err(|e| CrossMailError::from(crossmail_core::MailError::InvalidInput(e)))?;

        // 1) 落草稿：失败即中断，此时还没有任何东西发出去。
        let local = self
            .mail
            .save_draft(&account_id, &draft_from_dto(&draft))
            .map_err(CrossMailError::from)?;

        // 2) 真投递。
        let secret = self
            .accounts
            .resolve_secret(&account)
            .map_err(CrossMailError::from)?;
        let receipt = match crossmail_app::send_message_blocking(&account, &secret, message) {
            Ok(receipt) => receipt,
            Err(e) => {
                self.logger.warn(
                    "send",
                    format!("发信失败 account={account_id} draft={} err={e}", local.id),
                );
                return Err(e.into());
            }
        };

        // 3) 本地记账。IMAP 走 SMTP 提交，服务器不会自动归档已发送——provider 已经
        //    补了一次 APPEND 并回传「哪个文件夹、哪个 UID」。本地就用**同一个身份**
        //    记账，否则下次同步会把服务端那一封按 `账号/文件夹/UID` 再插一行，
        //    已发送里出现两封一模一样的邮件。JMAP 的 server_id 是提交 id 不是邮件 id，
        //    不能拿来拼主键，按协议区分。
        let server_uid = match account.protocol {
            core::ProtocolKind::Imap => receipt
                .server_id
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok()),
            _ => None,
        };
        let (email, sent_mailbox_id) = match (server_uid, receipt.sent_mailbox_id.as_deref()) {
            (Some(uid), Some(folder)) => {
                let email = self
                    .mail
                    .mark_sent_on_server(&local.id, folder, folder, uid)
                    .map_err(CrossMailError::from)?;
                (email, folder.to_string())
            }
            _ => {
                self.logger.warn(
                    "send",
                    format!(
                        "服务端已发送未归档（APPEND 失败或服务端无 UIDPLUS），仅本地记账 draft={}",
                        local.id
                    ),
                );
                let sent_box = self
                    .mail
                    .ensure_mailbox(&account_id, core::MailboxRole::Sent, "Sent")
                    .map_err(CrossMailError::from)?;
                let email = self
                    .mail
                    .mark_sent(&local.id, Some(sent_box.id.as_str()))
                    .map_err(CrossMailError::from)?;
                (email, sent_box.id)
            }
        };
        self.logger.info(
            "send",
            format!(
                "发信成功 account={account_id} message_id={} local={} mailbox={sent_mailbox_id}",
                receipt.message_id, email.id
            ),
        );
        Ok(SendReceiptDto {
            email_id: email.id,
            message_id: receipt.message_id,
            sent_at: to_system_time(receipt.sent_at),
            sent_mailbox_id,
        })
    }

    // ------------------------------------------------------------ 搜索

    pub fn search(
        &self,
        account_id: String,
        keyword: String,
        scope: SearchScopeDto,
        limit: u32,
        offset: u32,
    ) -> FfiResult<EmailPageDto> {
        let limit = if limit == 0 { 50 } else { limit };
        let page = self
            .search
            .search(
                &account_id,
                &keyword,
                search_scope_to_service(scope),
                limit,
                offset,
            )
            .map_err(CrossMailError::from)?;
        Ok(EmailPageDto {
            items: page.items.iter().map(email_to_dto).collect(),
            total: page.total,
            next_offset: page.next_offset,
        })
    }

    // ------------------------------------------------------------ 缓存

    pub fn cache_status(&self) -> FfiResult<CacheStatusDto> {
        let usage_bytes = self.cache.usage().map_err(CrossMailError::from)?;
        let quota_bytes = self.cache.config().quota_bytes;
        Ok(CacheStatusDto {
            usage_bytes,
            quota_bytes,
            over_quota: usage_bytes > quota_bytes,
        })
    }

    pub fn enforce_cache_quota(&self) -> FfiResult<EvictionReportDto> {
        let report = self.cache.enforce_quota().map_err(CrossMailError::from)?;
        Ok(EvictionReportDto {
            removed_count: report.removed.len() as u32,
            freed_bytes: report.freed_bytes,
            usage_after: report.usage_after,
            foreign_entries: report.foreign_entries as u32,
        })
    }

    pub fn clear_cache(&self) -> FfiResult<u32> {
        let n = self.cache.clear().map_err(CrossMailError::from)?;
        Ok(n as u32)
    }

    // ------------------------------------------------------ 安全渲染（S6-01/02）

    /// 读正文并净化。`allow_remote_once` 只影响本次渲染，不落库；
    /// 想「以后都显示」走 [`Self::allow_sender_remote_images`]。
    pub fn render_body(
        &self,
        email_id: String,
        prefer_html: bool,
        allow_remote_once: bool,
    ) -> FfiResult<RenderedBodyDto> {
        self.ensure_body(&email_id);
        let req = RenderRequest {
            prefer_html,
            permission: if allow_remote_once {
                AssetPermission::AllowOnce
            } else {
                AssetPermission::Inherit
            },
        };
        let body = self
            .render
            .render(&email_id, &req)
            .map_err(CrossMailError::from)?;
        let dto = rendered_to_dto(&body);
        if dto.blocked_count > 0 {
            self.logger.info(
                "render",
                format!("已阻止 {} 项远程资源 email={email_id}", dto.blocked_count),
            );
        }
        if dto.trust_level != "safe" {
            self.logger.warn(
                "render",
                format!(
                    "发件人可疑({}) email={email_id} 原因={}",
                    dto.trust_level, dto.trust_message
                ),
            );
        }
        Ok(dto)
    }

    pub fn sender_trust(&self, email_id: String) -> FfiResult<SenderTrustDto> {
        let trust = self.render.trust(&email_id).map_err(CrossMailError::from)?;
        Ok(sender_trust_to_dto(&trust))
    }

    /// 放行该发件人的远程图片。返回写入白名单的键（邮箱或可注册域）。
    pub fn allow_sender_remote_images(&self, email_id: String, scope: String) -> FfiResult<String> {
        let scope = AllowScope::parse(&scope).ok_or_else(|| CrossMailError::InvalidInput {
            message: format!("未知的放行范围：{scope}（可选 once / sender / domain）"),
        })?;
        let key = self
            .render
            .allow(&email_id, scope)
            .map_err(CrossMailError::from)?;
        self.logger.info(
            "render",
            format!("已放行远程资源 key={key} scope={}", scope.as_str()),
        );
        Ok(key)
    }

    pub fn revoke_sender_remote_images(&self, email_id: String) -> FfiResult<u32> {
        let removed = self
            .render
            .revoke(&email_id)
            .map_err(CrossMailError::from)?;
        self.logger.info(
            "render",
            format!("已撤销放行 email={email_id} 条数={removed}"),
        );
        Ok(removed as u32)
    }

    pub fn list_remote_allowances(&self) -> FfiResult<Vec<AllowanceDto>> {
        let rows = self.render.allowances().map_err(CrossMailError::from)?;
        Ok(rows.iter().map(allowance_to_dto).collect())
    }

    // ------------------------------------------------------- 可观测（S6-04）

    pub fn export_diagnostics(&self, dir: String) -> FfiResult<String> {
        let path = self
            .diagnostics
            .export(dir, env!("CARGO_PKG_VERSION"), std::env::consts::OS)
            .map_err(CrossMailError::from)?;
        Ok(path.display().to_string())
    }

    pub fn set_log_level(&self, level: String) -> FfiResult<String> {
        let parsed = LogLevel::parse(&level).ok_or_else(|| CrossMailError::InvalidInput {
            message: format!("未知日志级别：{level}（可选 error/warn/info/debug/trace）"),
        })?;
        self.logger.set_level(parsed);
        self.logger
            .info("obs", format!("日志级别已切换为 {}", parsed.as_str()));
        Ok(parsed.as_str().to_ascii_lowercase())
    }

    pub fn set_protocol_payload_logging(&self, enabled: bool) -> FfiResult<()> {
        self.logger.set_protocol_payload(enabled);
        self.logger.warn(
            "obs",
            format!("协议报文日志{}", if enabled { "已开启" } else { "已关闭" }),
        );
        Ok(())
    }

    pub fn log_status(&self) -> FfiResult<LogStatusDto> {
        Ok(log_status_to_dto(&self.logger))
    }

    pub fn rotate_logs(&self) -> FfiResult<bool> {
        self.logger
            .rotate_now()
            .map_err(|e| CrossMailError::Storage {
                message: format!("日志归档失败：{e}"),
            })
    }

    // ------------------------------------------------ 同步与推送（S6-16）

    /// 立即同步一次账号（阻塞）。按账号协议装配 provider、解出凭证、跑一轮增量同步。
    /// 失败直接抛给 UI，不做「半残成功」。
    pub fn sync_account(&self, account_id: String) -> FfiResult<SyncReportDto> {
        let account = self
            .accounts
            .get(&account_id)
            .map_err(CrossMailError::from)?;
        let secret = self
            .accounts
            .resolve_secret(&account)
            .map_err(CrossMailError::from)?;
        let report = crossmail_app::sync_account_blocking(
            &self.db,
            &account,
            &secret,
            CancellationToken::new(),
        )
        .map_err(CrossMailError::from)?;
        Ok(report.into())
    }

    /// 订阅推送。立即返回句柄；事件在后台经派发线程回调 observer。
    /// observer 由 UDL callback interface 生成，跨语言实现；闭包在 `subscribe_push`
    /// 的专用派发线程里被调用，不占 tokio worker，也不在 UI 主线程。
    pub fn subscribe_push(
        &self,
        account_id: String,
        observer: Box<dyn PushObserver>,
    ) -> FfiResult<Arc<PushSubscription>> {
        let account = self
            .accounts
            .get(&account_id)
            .map_err(CrossMailError::from)?;
        let secret = self
            .accounts
            .resolve_secret(&account)
            .map_err(CrossMailError::from)?;
        let handle = crossmail_app::subscribe_push(
            Arc::clone(&self.db),
            account,
            secret,
            move |notice: crossmail_app::PushNotice| observer.on_event(notice.into()),
        )
        .map_err(CrossMailError::from)?;
        Ok(Arc::new(PushSubscription::new(handle)))
    }
}

/// 日志目录：优先跟随数据库文件（同盘同生命周期的数据放一起），
/// 内存库或路径无父目录时退化到缓存目录下的 `logs`。
fn resolve_log_dir(config: &EngineConfig) -> std::path::PathBuf {
    let from_db = if config.db_path == ":memory:" {
        None
    } else {
        std::path::Path::new(&config.db_path)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.join("logs"))
    };
    from_db.unwrap_or_else(|| {
        if config.cache_dir.trim().is_empty() {
            std::env::temp_dir().join("crossmail-logs")
        } else {
            std::path::PathBuf::from(&config.cache_dir).join("logs")
        }
    })
}

/// 打开引擎：建库 → 迁移 → 装凭证仓 → 装文件缓存。
///
/// 任何一步失败都直接抛给调用方，不做「降级启动」——带着半残状态跑起来，
/// 后面会在更难定位的地方炸。
pub fn open_engine(config: EngineConfig) -> FfiResult<Arc<CrossMailEngine>> {
    let mut db = if config.db_path == ":memory:" {
        Database::open_in_memory().map_err(CrossMailError::from)?
    } else {
        if let Some(parent) = std::path::Path::new(&config.db_path).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| CrossMailError::Storage {
                    message: format!("创建数据库目录失败：{e}"),
                })?;
            }
        }
        Database::open(&config.db_path).map_err(CrossMailError::from)?
    };
    db.migrate().map_err(CrossMailError::from)?;

    let db = Arc::new(Mutex::new(db));

    let codec: Arc<dyn SecretCodec> = match &config.master_key_b64 {
        Some(key) if !key.trim().is_empty() => {
            Arc::new(ChaChaSecretCodec::from_exported(key.trim()).map_err(CrossMailError::from)?)
        }
        // 没有主密钥：本进程随机生成。旧凭证解不开是预期行为，生产必须由
        // 平台密钥库注入，这里只是让开发期能跑起来。
        _ => Arc::new(ChaChaSecretCodec::generate()),
    };
    let vault = Arc::new(SecretVault::new(Arc::clone(&db), codec));

    let cache = Arc::new(
        BodyCache::new(
            Arc::clone(&db),
            CacheConfig::new(config.cache_dir.clone(), config.cache_quota_bytes),
        )
        .map_err(CrossMailError::from)?,
    );

    // 日志目录不可写不应阻断启动：日志是旁路，降级为内存日志继续跑。
    let log_dir = resolve_log_dir(&config);
    let logger = match Logger::to_dir(&log_dir, RotateConfig::default(), LogFilter::default()) {
        Ok(l) => Arc::new(l),
        Err(_) => Arc::new(Logger::in_memory(LogFilter::default())),
    };
    logger.info(
        "engine",
        format!(
            "引擎启动 version={} db={} cache_dir={}",
            env!("CARGO_PKG_VERSION"),
            config.db_path,
            config.cache_dir
        ),
    );

    let mail = MailService::with_cache(Arc::clone(&db), Arc::clone(&cache));
    let render = RenderService::with_mail(Arc::clone(&db), mail.clone());
    let diagnostics = DiagnosticsService::new(Arc::clone(&db))
        .with_cache(Arc::clone(&cache))
        .with_logger(Arc::clone(&logger));

    Ok(Arc::new(CrossMailEngine {
        accounts: AccountService::new(Arc::clone(&db), vault),
        mail,
        search: SearchService::new(Arc::clone(&db)),
        cache,
        render,
        diagnostics,
        logger,
        db: Arc::clone(&db),
    }))
}
