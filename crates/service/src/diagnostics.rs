//! 诊断导出（OBS-003）的业务侧装配：把「账号 → 同步状态 → 离线队列 → 缓存 → 日志」
//! 收成一份 [`DiagnosticsInput`]，交给 `crossmail-obs` 打成 tar 包。
//!
//! 这一层只做**取数与脱敏前的结构化**，不做脱敏——脱敏统一在 `crossmail-obs`
//! 里做，避免两个地方各写一套掩码规则然后慢慢分叉。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crossmail_core::MailResult;
use crossmail_obs::{DiagnosticAccount, DiagnosticsInput, Logger};
use crossmail_storage::Database;
use serde_json::{json, Map, Value};

use crate::cache::BodyCache;
use crate::lock_db;

/// 诊断包里带多少条内存日志
pub const RECENT_LOG_LINES: usize = 300;

/// 诊断服务。
pub struct DiagnosticsService {
    db: Arc<Mutex<Database>>,
    cache: Option<Arc<BodyCache>>,
    logger: Option<Arc<Logger>>,
}

impl DiagnosticsService {
    pub fn new(db: Arc<Mutex<Database>>) -> Self {
        Self {
            db,
            cache: None,
            logger: None,
        }
    }

    pub fn with_cache(mut self, cache: Arc<BodyCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    pub fn with_logger(mut self, logger: Arc<Logger>) -> Self {
        self.logger = Some(logger);
        self
    }

    /// 采集诊断数据（不落盘）。
    pub fn collect(&self, app_version: &str, platform: &str) -> MailResult<DiagnosticsInput> {
        let mut input = DiagnosticsInput::new(app_version, platform);

        // 数据库段：整段用一个作用域包住，出了花括号立刻放锁。
        // 后面读缓存占用时 `BodyCache::usage` 自己还要拿同一把锁——如果继续
        // 持着守卫去调，就是非重入 Mutex 的自锁死锁（S3/S4 各踩过一次）。
        {
            let db = lock_db(&self.db)?;
            let mut accounts = Vec::new();
            let mut sync_status = Map::new();
            let mut queue = Map::new();

            for account_id in db.list_accounts()? {
                let Some(account) = db.get_account(&account_id)? else {
                    continue;
                };
                let mailboxes = db.list_mailboxes(&account_id)?;
                let mut per_mailbox = Map::new();
                for m in &mailboxes {
                    per_mailbox.insert(
                        m.name.clone(),
                        json!({
                            "role": format!("{:?}", m.role).to_lowercase(),
                            "total": m.total,
                            "unread": m.unread,
                            "uid_validity": m.uid_validity,
                            "modseq": m.highest_modseq,
                            "state_present": m.state.is_some(),
                        }),
                    );
                }
                let pending = db.pending_operations(&account_id)?;
                accounts.push(DiagnosticAccount {
                    address: account.email_address.clone(),
                    protocol: format!("{:?}", account.protocol).to_lowercase(),
                    last_sync_at: None,
                    sync_state_len: mailboxes.iter().filter(|m| m.state.is_some()).count(),
                });
                sync_status.insert(account_id.clone(), Value::Object(per_mailbox));
                queue.insert(
                    account_id,
                    json!({
                        "pending": pending.len(),
                        "with_errors": pending.iter().filter(|o| o.last_error.is_some()).count(),
                    }),
                );
            }

            input.accounts = accounts;
            input.sync_status = Value::Object(sync_status);
            input.offline_queue = Value::Object(queue);
        }

        input.cache = match &self.cache {
            Some(cache) => {
                let usage = cache.usage()?;
                let quota = cache.config().quota_bytes;
                json!({
                    "usage_bytes": usage,
                    "quota_bytes": quota,
                    "over_quota": usage > quota,
                    "root": cache.root().display().to_string(),
                })
            }
            None => json!({ "configured": false }),
        };

        if let Some(logger) = &self.logger {
            input.recent_logs = logger
                .recent(RECENT_LOG_LINES)
                .iter()
                .map(|r| r.to_line())
                .collect();
            input.log_files = logger.log_files();
        }
        Ok(input)
    }

    /// 导出诊断包到目录，返回 tar 路径。
    pub fn export(
        &self,
        dir: impl Into<PathBuf>,
        app_version: &str,
        platform: &str,
    ) -> MailResult<PathBuf> {
        let input = self.collect(app_version, platform)?;
        let path = crossmail_obs::export(dir.into(), &input)
            .map_err(|e| crossmail_core::MailError::Storage(format!("导出诊断包失败：{e}")))?;
        if let Some(logger) = &self.logger {
            logger.info("diagnostics", format!("诊断包已导出: {}", path.display()));
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossmail_core::{Account, Address, Email, Mailbox, ProtocolKind};
    use crossmail_obs::{LogFilter, LogLevel};
    use crossmail_storage::Database;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("crossmail-svc-diag-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 每个用例一个独立目录：`cargo test` 默认多线程跑同一个测试二进制，
    /// 共用固定目录会让 A 用例的 `remove_dir_all` 撞掉 B 用例正在写的文件。
    fn setup(tag: &str) -> (Arc<Mutex<Database>>, PathBuf) {
        let mut db = Database::open_in_memory().unwrap();
        db.migrate().unwrap();
        let db = Arc::new(Mutex::new(db));
        {
            let db = db.lock().unwrap();
            db.upsert_account(&Account::new(
                "acc1",
                "zhangsan@example.com",
                ProtocolKind::Imap,
            ))
            .unwrap();
            let mut mb = Mailbox::new("mb1", "acc1", "INBOX");
            mb.total = 12;
            mb.unread = 3;
            mb.state = Some("42".into());
            db.upsert_mailbox(&mb).unwrap();
            let mut email = Email::new("e1", "acc1", "mb1");
            email.from = vec![Address::new("boss@corp.cn")];
            email.subject = "hi".into();
            db.upsert_email(&email).unwrap();
        }
        (db, temp_dir(tag))
    }

    #[test]
    fn collect_gathers_sync_and_queue_state() {
        let (db, _) = setup("collect");
        let svc = DiagnosticsService::new(db);
        let input = svc.collect("0.1.0", "windows-x64").unwrap();
        assert_eq!(input.accounts.len(), 1);
        assert_eq!(input.accounts[0].protocol, "imap");
        assert_eq!(input.accounts[0].sync_state_len, 1);
        assert!(input.sync_status.get("acc1").is_some());
        assert!(input.offline_queue.get("acc1").is_some());
    }

    #[test]
    fn export_produces_redacted_tar() {
        let (db, dir) = setup("redacted");
        let logger = Arc::new(Logger::in_memory(LogFilter::new(LogLevel::Debug)));
        logger.info("sync", "同步完成 owner=zhangsan@example.com");
        let svc = DiagnosticsService::new(db).with_logger(Arc::clone(&logger));
        let path = svc.export(&dir, "0.1.0", "android-arm64").unwrap();
        assert!(path.exists());
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("zhangsan@example.com"), "邮箱必须脱敏");
        assert!(text.contains("zh***@example.com"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cache_section_reflects_configuration() {
        let (db, dir) = setup("cache");
        let cache = Arc::new(
            BodyCache::new(
                Arc::clone(&db),
                crate::cache::CacheConfig::new(dir.join("cache"), 1024),
            )
            .unwrap(),
        );
        let svc = DiagnosticsService::new(Arc::clone(&db)).with_cache(cache);
        let input = svc.collect("0.1.0", "test").unwrap();
        assert_eq!(input.cache["quota_bytes"], 1024);
        std::fs::remove_dir_all(&dir).ok();
    }
}
