//! 离线操作回放（S3-06）。
//!
//! 契约（A-006 / S3-06 验收）：
//! * **严格 FIFO**：按入队顺序回放，一条失败即中止本轮，绝不越序；
//! * **网络类错误 ⇒ 保留队列、下次重试**（`deferred`）；
//! * **协议类错误 ⇒ 终止回放且不再自动重试**（`failed`，靠 `attempts` 上限做死信）；
//! * 回放成功即出队，保证「不丢失、不重复执行」。

use crossmail_core::error::{ErrorKind, MailError, MailResult};
use crossmail_core::model::{Address, Email, Flag, Id, MailboxRole, OutgoingMessage};
use crossmail_core::provider::{
    retarget_email_id, IdRemap, MailProvider, OfflineOperation, OperationKind,
};
use crossmail_storage::Database;
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug, Clone)]
pub struct ReplayConfig {
    /// 单条操作最大尝试次数，超过即视为死信不再重试
    pub max_attempts: u32,
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self { max_attempts: 5 }
    }
}

/// 把一批邮件主键的文件夹段改指到 `from`（当前位置）。
///
/// 只做一次字符串改写，不查库：`from` 由业务层在入队时记录，比 id 内嵌的同步快照新鲜。
/// 对不含文件夹结构的服务端 id（JMAP 一类）原样返回，因此可安全地泛型使用。
fn retarget_ids(ids: &[Id], from: &Id) -> Vec<Id> {
    if from.trim().is_empty() {
        return ids.to_vec();
    }
    ids.iter().map(|id| retarget_email_id(id, from)).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayError {
    pub op_id: Id,
    pub retryable: bool,
    /// 错误分类。`retryable` 只区分「要不要自动重试」，但**两种不可重试错误的处置完全不同**：
    /// 认证失效要提示用户重新登录，协议错误要提示这条操作本身不合法。
    /// 只给一个 bool 的话，UI 只能对用户说"失败了"，等于没提示。
    pub kind: ErrorKind,
    pub message: String,
}

impl ReplayError {
    /// 是否是「用户重新认证后就能继续」的错误（凭证过期 / 被吊销）。
    pub fn is_reauth_required(&self) -> bool {
        self.kind == ErrorKind::Authentication
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayReport {
    pub applied: usize,
    /// 因网络类错误被推迟（仍在队列中）
    pub deferred: usize,
    /// 因协议类错误被终止（仍在队列中，等人工处理或达上限）
    pub failed: usize,
    /// 已达尝试上限、本轮直接跳过的死信
    pub skipped: usize,
    pub errors: Vec<ReplayError>,
    /// 本轮发生的身份变更（服务器给邮件换了 UID）。
    ///
    /// 调用方拿到它才能跟上：本地主键已经换成 `to`，UI 手里的旧 id 立即失效。
    /// 不返回这个映射，上层就只能靠"再同步一次"来猜新 id。
    pub remaps: Vec<IdRemap>,
}

impl ReplayReport {
    /// 是否需要用户重新认证才能继续回放。
    ///
    /// 队列不会因此被清空：用户重新登录后直接再跑一轮即可，业务操作不会丢。
    /// 这是刻意的——把队列丢掉看着"干净"，实际是让用户的未发送邮件人间蒸发。
    pub fn needs_reauth(&self) -> bool {
        self.errors.iter().any(ReplayError::is_reauth_required)
    }

    /// 队列是否已清空（无推迟、无失败、无死信）
    pub fn is_drained(&self) -> bool {
        self.deferred == 0 && self.failed == 0 && self.skipped == 0
    }

    pub fn last_error(&self) -> Option<&ReplayError> {
        self.errors.last()
    }
}

pub struct OfflineReplayer<P: MailProvider> {
    provider: Arc<P>,
    db: Arc<Mutex<Database>>,
    cfg: ReplayConfig,
}

impl<P: MailProvider> OfflineReplayer<P> {
    pub fn new(provider: Arc<P>, db: Arc<Mutex<Database>>) -> Self {
        Self {
            provider,
            db,
            cfg: ReplayConfig::default(),
        }
    }

    pub fn with_config(provider: Arc<P>, db: Arc<Mutex<Database>>, cfg: ReplayConfig) -> Self {
        Self { provider, db, cfg }
    }

    fn db(&self) -> MailResult<MutexGuard<'_, Database>> {
        self.db
            .lock()
            .map_err(|_| MailError::Storage("数据库锁已损坏".into()))
    }

    /// 回放一个账号的全部待办操作。
    pub async fn replay(&self, account_id: &str) -> MailResult<ReplayReport> {
        let mut report = ReplayReport::default();
        // 每轮都重新从库里取「下一条」，而不是一次性把队列读进内存：
        // 移动成功后要把身份映射写回队列（同一条邮件后续的标星/删除还指着旧 UID），
        // 读死快照就会拿到没改写的旧操作——那正是「报告全绿、服务器没动」的成因。
        let mut skipped: Vec<Id> = Vec::new();
        loop {
            let next = self
                .db()?
                .pending_operations(account_id)?
                .into_iter()
                .find(|op| !skipped.contains(&op.id));
            let Some(op) = next else { break };
            if op.attempts >= self.cfg.max_attempts {
                // 死信：不再重试，也不阻塞后续操作（记下来避免原地打转）
                skipped.push(op.id.clone());
                report.skipped += 1;
                continue;
            }
            match self.apply_one(&op).await {
                Ok(remaps) => {
                    self.db()?.mark_operation_done(&op.id)?;
                    report.applied += 1;
                    report.remaps.extend(remaps);
                }
                Err(err) if err.is_retryable() => {
                    self.db()?
                        .record_operation_error(&op.id, &err.to_string())?;
                    report.deferred += 1;
                    report.errors.push(ReplayError {
                        op_id: op.id.clone(),
                        retryable: true,
                        kind: err.kind(),
                        message: err.to_string(),
                    });
                    // 网络类错误：中止本轮，保持 FIFO，下次网络恢复再从这一条开始
                    break;
                }
                Err(err) => {
                    self.db()?
                        .record_operation_error(&op.id, &err.to_string())?;
                    report.failed += 1;
                    report.errors.push(ReplayError {
                        op_id: op.id.clone(),
                        retryable: false,
                        kind: err.kind(),
                        message: err.to_string(),
                    });
                    // 协议类错误：终止回放，不做无意义重试
                    break;
                }
            }
        }
        Ok(report)
    }

    /// 把一条离线操作翻译成协议调用。
    ///
    /// 返回这次调用引起的身份变更（可能为空），由调用方写进报告并落库。
    pub async fn apply_one(&self, op: &OfflineOperation) -> MailResult<Vec<IdRemap>> {
        match &op.kind {
            OperationKind::SetFlag {
                email_id,
                flag,
                enabled,
            } => {
                self.provider
                    .set_flags(std::slice::from_ref(email_id), flag.clone(), *enabled)
                    .await?;
                Ok(Vec::new())
            }
            OperationKind::Move {
                email_ids,
                from,
                to,
            } => {
                let remaps = self.provider.move_emails(email_ids, from, to).await?;
                self.apply_remaps(&op.account_id, &remaps, to)?;
                Ok(remaps)
            }
            OperationKind::Copy {
                email_ids,
                from,
                to,
            } => {
                // `copy_emails` 的签名里没有 from，只能靠 id 里的文件夹定位源文件夹。
                // 而 id 里的文件夹是上次同步的快照——移动过一次就过期了，于是源文件夹里
                // 根本没有这些 UID。真机对不存在的 UID 回 OK，结果是「静默空转」。
                // 所以进 provider 之前，先把 id 的文件夹段改指到 `from`（业务层记录的当前位置）。
                let ids = retarget_ids(email_ids, from);
                self.provider.copy_emails(&ids, to).await?;
                Ok(Vec::new())
            }
            OperationKind::Delete { email_ids, from } => {
                let ids = retarget_ids(email_ids, from);
                self.provider.delete_emails(&ids).await?;
                Ok(Vec::new())
            }
            OperationKind::Archive { email_ids, from } => {
                let archive = self.archive_mailbox(&op.account_id)?;
                let remaps = self.provider.move_emails(email_ids, from, &archive).await?;
                self.apply_remaps(&op.account_id, &remaps, &archive)?;
                Ok(remaps)
            }
            OperationKind::Send { draft_id } => {
                let msg = self.outgoing_from_draft(&op.account_id, draft_id)?;
                self.provider.send_email(msg).await?;
                Ok(Vec::new())
            }
            OperationKind::CreateMailbox { draft } => {
                self.provider.create_mailbox(draft.clone()).await?;
                Ok(Vec::new())
            }
        }
    }

    /// 把服务器返回的身份映射落到本地：邮件记录换主键，**队列里引用旧主键的后续操作一起改写**。
    ///
    /// 不做的后果是「静默失败」：移动之后队列里「给这封标星」的那条还指着旧 UID，
    /// 服务端对不存在的 UID 回 OK，回放报告一片绿，用户看到的邮件却什么都没变。
    fn apply_remaps(
        &self,
        account_id: &str,
        remaps: &[IdRemap],
        new_mailbox: &Id,
    ) -> MailResult<()> {
        if remaps.is_empty() {
            return Ok(());
        }
        let db = self.db()?;
        for remap in remaps {
            db.remap_email(account_id, &remap.from, &remap.to, new_mailbox)?;
        }
        Ok(())
    }

    fn archive_mailbox(&self, account_id: &str) -> MailResult<Id> {
        self.db()?
            .list_mailboxes(account_id)?
            .into_iter()
            .find(|m| m.role == MailboxRole::Archive)
            .map(|m| m.id)
            .ok_or_else(|| MailError::Unsupported("账号缺少归档文件夹，无法回放归档操作".into()))
    }

    /// 从本地缓存草稿重建待发邮件。
    ///
    /// 局限：附件二进制不在 SQLite 里（只缓存元数据），因此带附件的离线发送会显式报不支持，
    /// 而不是发出一个丢附件的残缺邮件。
    fn outgoing_from_draft(&self, account_id: &str, draft_id: &Id) -> MailResult<OutgoingMessage> {
        let email: Email = self
            .db()?
            .get_email(draft_id)?
            .ok_or_else(|| MailError::NotFound(format!("草稿 {draft_id} 不在本地缓存")))?;
        if !email.attachments.is_empty() {
            return Err(MailError::Unsupported(
                "离线发送暂不支持重建附件，请联网后重新发送".into(),
            ));
        }
        let from = email
            .from
            .first()
            .cloned()
            .unwrap_or_else(|| Address::new(email.account_id.clone()));
        let mut msg =
            OutgoingMessage::new(account_id, from, email.to.clone(), email.subject.clone());
        msg.cc = email.cc.clone();
        msg.bcc = email.bcc.clone();
        msg.text_body = email.text_body.clone();
        msg.html_body = email.html_body.clone();
        msg.in_reply_to = email.in_reply_to.clone();
        msg.references = email.references.clone();
        msg.draft_id = Some(draft_id.clone());
        Ok(msg)
    }

    /// 便捷封装：把一个「设置标记」动作写入离线队列。
    pub fn queue_flag(
        &self,
        account_id: &str,
        email_id: &Id,
        flag: Flag,
        enabled: bool,
    ) -> MailResult<Id> {
        let id = uuid::Uuid::new_v4().to_string();
        let op = OfflineOperation::new(
            id.clone(),
            account_id,
            OperationKind::SetFlag {
                email_id: email_id.clone(),
                flag,
                enabled,
            },
        );
        self.db()?.enqueue(&op)?;
        Ok(id)
    }
}
