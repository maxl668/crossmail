//! 同步引擎（S3-04 / S3-05）：增量拉取 → 幂等落库 → 冲突裁决 → 状态校准。
//!
//! 设计要点：
//! * **幂等**：同一份数据重复应用不产生重复行、不重复计数（唯一键 + 合并判定）。
//! * **游标只在成功应用后推进**，失败不推进，避免丢数据。
//! * **全量校准**：协议层报告 `full_resync`（UIDVALIDITY 变化 / JMAP state 冲突）时，
//!   删除该文件夹内远端已不存在的本地行。

use crate::conflict::{merge, pending_ops_for_email, Dimension};
use crossmail_core::error::MailResult;
use crossmail_core::model::{Email, Id};
use crossmail_core::provider::{
    CancellationToken, MailProvider, ProgressReporter, SyncPage, SyncRequest,
};
use crossmail_storage::{Database, SyncCursor};
use std::sync::{Arc, Mutex, MutexGuard};

/// 引擎配置。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub page_limit: u32,
    pub include_body: bool,
    /// 全量校准时单文件夹扫描上限，防止异常账号把内存打满
    pub calibration_scan_limit: u32,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            page_limit: 500,
            include_body: false,
            calibration_scan_limit: 100_000,
        }
    }
}

/// 一次应用的结果；`unchanged` 是幂等性的直接证据。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApplyStats {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub deleted: usize,
    pub calibrated: usize,
}

impl ApplyStats {
    pub fn touched(&self) -> usize {
        self.inserted + self.updated + self.deleted + self.calibrated
    }
}

/// 一次同步的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    pub stats: ApplyStats,
    pub new_state: Option<String>,
    pub new_modseq: Option<u64>,
    pub full_resync: bool,
    pub has_more: bool,
}

/// 冲突裁决命中的最高维度统计，便于诊断「这次校准为什么改了数据」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictReport {
    pub delete_wins: usize,
    pub move_wins: usize,
    pub flag_wins: usize,
    pub local_overrides: usize,
}

pub struct SyncEngine<P: MailProvider> {
    provider: Arc<P>,
    /// SQLite 连接不是 `Sync`：用短锁串行化，保证引擎本身 `Send + Sync`，可跑在多线程运行时。
    db: Arc<Mutex<Database>>,
    cfg: EngineConfig,
}

fn cursor_key(mailbox_id: Option<&Id>) -> String {
    mailbox_id
        .map(|m| m.to_string())
        .unwrap_or_else(|| "__all__".to_string())
}

impl<P: MailProvider> SyncEngine<P> {
    pub fn new(provider: Arc<P>, db: Arc<Mutex<Database>>) -> Self {
        Self {
            provider,
            db,
            cfg: EngineConfig::default(),
        }
    }

    pub fn with_config(provider: Arc<P>, db: Arc<Mutex<Database>>, cfg: EngineConfig) -> Self {
        Self { provider, db, cfg }
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    /// 短锁：调用方必须只用于一次同步的 DB 访问，禁止跨 await 持有。
    fn db(&self) -> MailResult<MutexGuard<'_, Database>> {
        self.db
            .lock()
            .map_err(|_| crossmail_core::MailError::Storage("数据库锁已损坏".into()))
    }

    /// 同步一个账号（可选限定文件夹）。
    pub async fn sync(
        &self,
        account_id: &str,
        mailbox_id: Option<Id>,
        cancel: CancellationToken,
        reporter: Option<&dyn ProgressReporter>,
    ) -> MailResult<SyncOutcome> {
        self.sync_with_report(account_id, mailbox_id, cancel, reporter)
            .await
            .map(|(outcome, _)| outcome)
    }

    /// 同步并返回冲突裁决统计（S3-08 一致性验证用）。
    pub async fn sync_with_report(
        &self,
        account_id: &str,
        mailbox_id: Option<Id>,
        cancel: CancellationToken,
        reporter: Option<&dyn ProgressReporter>,
    ) -> MailResult<(SyncOutcome, ConflictReport)> {
        let key = cursor_key(mailbox_id.as_ref());
        let cursor = self.db()?.get_sync_cursor(account_id, &key)?;

        let req = SyncRequest {
            account_id: account_id.to_string(),
            mailbox_id: mailbox_id.clone(),
            since_state: cursor.state.clone(),
            since_modseq: cursor.modseq,
            limit: self.cfg.page_limit,
            include_body: self.cfg.include_body,
        };

        let page = self.provider.sync_emails(req, cancel, reporter).await?;
        let (stats, conflicts) = self
            .apply_page_with_report(account_id, mailbox_id.as_ref(), &page)
            .await?;

        // 应用成功后才推进游标：中途失败下次仍从旧游标重拉，宁可重复不可遗漏
        if let Some(state) = &page.new_state {
            self.db()?.set_sync_cursor(
                account_id,
                &key,
                &SyncCursor {
                    state: Some(state.clone()),
                    modseq: page.new_modseq,
                },
            )?;
        }

        Ok((
            SyncOutcome {
                stats,
                new_state: page.new_state,
                new_modseq: page.new_modseq,
                full_resync: page.full_resync,
                has_more: page.has_more,
            },
            conflicts,
        ))
    }

    /// 应用一页数据（幂等，可重复调用）。
    pub async fn apply_page(
        &self,
        account_id: &str,
        mailbox_id: Option<&Id>,
        page: &SyncPage,
    ) -> MailResult<ApplyStats> {
        self.apply_page_with_report(account_id, mailbox_id, page)
            .await
            .map(|(s, _)| s)
    }

    async fn apply_page_with_report(
        &self,
        account_id: &str,
        mailbox_id: Option<&Id>,
        page: &SyncPage,
    ) -> MailResult<(ApplyStats, ConflictReport)> {
        let mut stats = ApplyStats::default();
        let mut conflicts = ConflictReport::default();

        // 未上传的离线操作决定本地在哪些维度上有话语权
        let pending_ops = self
            .db()?
            .pending_operations(account_id)?
            .into_iter()
            .map(|op| op.kind)
            .collect::<Vec<_>>();

        for remote in &page.emails {
            // provider 返回的对象带的是**协议侧账号 id**（JMAP 的 accountId、
            // IMAP 的账号串），与本地 `accounts(id)` 不是同一命名空间。落库前
            // 必须归一化成本次同步的本地 account_id，否则 `emails.account_id`
            // 外键必挂，且 merge 会因 id 不同把每封邮件都误判成 updated。
            // 真机 FFI 账号用 UUID、服务端 accountId 是 `b`，此处不归一就写不进去。
            let mut remote = remote.clone();
            remote.account_id = account_id.to_string();

            let related = pending_ops_for_email(&pending_ops, &remote.id);
            // 先取出本地副本再 match：`match self.db()?... { .. }` 会把锁守卫活到整个 match 结束，
            // 分支里再取锁就会自锁（SQLite 连接锁不可重入）。
            let existing = self.db()?.get_email(&remote.id)?;
            match existing {
                None => {
                    self.db()?.upsert_email(&remote)?;
                    stats.inserted += 1;
                }
                Some(local) => {
                    let outcome = merge(&local, &remote, &related);
                    conflicts.local_overrides += outcome.local_overrides;
                    match outcome.winner {
                        Some(Dimension::Delete) => conflicts.delete_wins += 1,
                        Some(Dimension::Move) => conflicts.move_wins += 1,
                        Some(Dimension::Flag) => conflicts.flag_wins += 1,
                        None => {}
                    }
                    if outcome.email == local {
                        stats.unchanged += 1;
                    } else {
                        self.db()?.upsert_email(&outcome.email)?;
                        stats.updated += 1;
                    }
                }
            }
        }

        for id in &page.deleted_ids {
            // 有未上传操作时不能删本地行，否则用户的离线动作会凭空消失
            if !pending_ops_for_email(&pending_ops, id).is_empty() {
                continue;
            }
            if self.db()?.delete_email(id)? > 0 {
                stats.deleted += 1;
            }
        }

        if page.full_resync {
            if let Some(mailbox) = mailbox_id {
                stats.calibrated = self.calibrate(account_id, mailbox, page)?;
            }
        }

        Ok((stats, conflicts))
    }

    /// 全量校准：删除本地有、远端本页没有的行（仅限该文件夹）。
    fn calibrate(&self, account_id: &str, mailbox_id: &Id, page: &SyncPage) -> MailResult<usize> {
        let remote_ids: std::collections::BTreeSet<&str> =
            page.emails.iter().map(|e| e.id.as_str()).collect();
        let local = self.db()?.list_emails_in_mailbox(
            account_id,
            mailbox_id,
            self.cfg.calibration_scan_limit,
        )?;
        let mut removed = 0usize;
        for email in local {
            if remote_ids.contains(email.id.as_str()) {
                continue;
            }
            removed += self.db()?.delete_email(&email.id)?;
        }
        Ok(removed)
    }

    /// 构造一封「本地已存在、远端已消失」的判定，供上层做 UI 层删除动画。
    pub fn is_remote_missing(local: &Email, page: &SyncPage) -> bool {
        !page.emails.iter().any(|e| e.id == local.id)
    }
}
