//! 协议抽象层（需求 A-001 / A-004 / A-005 / A-006 / A-007）。
//!
//! 本文件只定义契约，不含任何协议实现；S1/S2 的 JMAP、IMAP 客户端各自独立实现，
//! S3 才提供 Provider 适配层。提前定义契约可保证协议实现不被上层反向污染。

use crate::error::{MailError, MailResult};
use crate::model::*;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// 服务端能力集：不支持的能力自动降级（J-001 / I-004 / I-007）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    pub jmap_mail: bool,
    pub condstore: bool,
    pub uidplus: bool,
    pub imap_move: bool,
    pub imap_idle: bool,
    pub gmail_labels: bool,
    pub push: bool,
    pub blob_upload: bool,
}

/// 轻量级取消令牌（A-004）：所有耗时操作需在循环中检查。
#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    flag: Arc<AtomicBool>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    pub fn check(&self) -> MailResult<()> {
        if self.is_cancelled() {
            Err(MailError::Cancelled)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncPhase {
    Mailboxes,
    Headers,
    Bodies,
    Flags,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub phase: SyncPhase,
    pub processed: u64,
    pub total: u64,
}

impl Progress {
    pub fn percent(&self) -> u8 {
        if self.total == 0 {
            return 0;
        }
        ((self.processed.min(self.total) * 100 / self.total).min(100)) as u8
    }
}

/// 进度回调（A-004）：UI 层实现，避免核心库依赖 UI。
pub trait ProgressReporter: Send + Sync {
    fn report(&self, progress: Progress);
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRequest {
    pub account_id: Id,
    pub mailbox_id: Option<Id>,
    pub since_state: Option<String>,
    pub since_modseq: Option<u64>,
    pub limit: u32,
    pub include_body: bool,
}

impl SyncRequest {
    pub fn new(account_id: impl Into<Id>) -> Self {
        Self {
            account_id: account_id.into(),
            mailbox_id: None,
            since_state: None,
            since_modseq: None,
            limit: 500,
            include_body: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncPage {
    pub emails: Vec<Email>,
    pub deleted_ids: Vec<Id>,
    pub mailbox_changes: Vec<Mailbox>,
    pub new_state: Option<String>,
    pub new_modseq: Option<u64>,
    pub has_more: bool,
    /// 冲突后是否执行了全量校准（A-005）
    pub full_resync: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxDraft {
    pub name: String,
    pub parent_id: Option<Id>,
    pub role: MailboxRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub account_id: Id,
    pub email_address: String,
    pub capabilities: ProviderCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentReceipt {
    pub message_id: String,
    pub server_id: Option<Id>,
    pub sent_mailbox_id: Option<Id>,
    pub sent_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub id: String,
    pub size: u64,
    pub content_type: String,
}

/// 离线操作类型（A-006 / S3-06）。冲突优先级见 `conflict_priority`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OperationKind {
    SetFlag {
        email_id: Id,
        flag: Flag,
        enabled: bool,
    },
    Move {
        email_ids: Vec<Id>,
        from: Id,
        to: Id,
    },
    Copy {
        email_ids: Vec<Id>,
        /// 邮件**当前**所在文件夹。`email_ids` 里内嵌的文件夹是「上一次同步时」的快照，
        /// 服务器 MOVE 后会失效（IMAP 的邮件身份是 `(文件夹, UID)`，移动即换 UID）。
        /// 空字符串表示调用方没给，回退到 id 内嵌值。
        #[serde(default)]
        from: Id,
        to: Id,
    },
    Delete {
        email_ids: Vec<Id>,
        /// 同 `Copy::from`：这里缺它会让删除打到已不存在的旧 UID 上。
        #[serde(default)]
        from: Id,
    },
    Archive {
        email_ids: Vec<Id>,
        from: Id,
    },
    Send {
        draft_id: Id,
    },
    CreateMailbox {
        draft: MailboxDraft,
    },
}

/// 从本地主键 `账号/文件夹/UID` 里取文件夹段。
///
/// 只认「末段是纯数字 UID」的形态（文件夹自身可含 `/`，所以从右侧切）；其余一律 `None`，
/// 让调用方原样保留 id——JMAP 那类服务端 id（`qeaaaaeb`）走的就是这条路。
pub fn folder_segment(id: &str) -> Option<&str> {
    let (head, tail) = id.rsplit_once('/')?;
    if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (account, folder) = head.split_once('/')?;
    if account.is_empty() || folder.is_empty() {
        return None;
    }
    Some(folder)
}

/// 把本地主键里的**文件夹段**改指到 `from`，返回「邮件当前位置」的 id。
///
/// 为什么需要：本地主键形如 `账号/文件夹/UID`，其中的文件夹是**上一次同步时**的位置快照。
/// IMAP 的邮件身份是 `(文件夹, UID)`，服务器 MOVE 会给邮件分配新 UID，旧主键随即失效。
/// `move_emails` 有 `from` 参数能绕开这个问题，而 `copy_emails` / `delete_emails` 的签名
/// 里没有——于是离线回放只能拿 id 内嵌的旧文件夹去 `SELECT`，再对旧 UID 发 `UID COPY`/`UID MOVE`。
///
/// 真机对**不存在的 UID** 一律回 `OK`（`OK No messages were copied.` / `OK UID STORE completed`），
/// 所以这种操作不是报错，是**静默空转**：队列清空、零错误、服务器上什么都没有发生。
/// 在回放层用本函数改写 id 的文件夹段，就能在不动 `MailProvider` 签名的前提下消除这个缺口。
///
/// 只对 `账号/文件夹/UID` 形态生效，其余原样返回，因此可安全地泛型调用。
/// `from` 为空时同样原样返回——表示调用方没有更权威的位置信息，只能沿用 id 内嵌值。
pub fn retarget_email_id(id: &str, from: &str) -> String {
    if from.trim().is_empty() {
        return id.to_string();
    }
    let Some(folder) = folder_segment(id) else {
        return id.to_string();
    };
    if folder == from {
        return id.to_string();
    }
    let Some((account, _)) = id.split_once('/') else {
        return id.to_string();
    };
    let uid = id.rsplit_once('/').map(|(_, u)| u).unwrap_or_default();
    format!("{account}/{from}/{uid}")
}

/// A-005：冲突按「删除 > 移动 > 标记」优先级处理，数值越大越优先。
pub fn conflict_priority(kind: &OperationKind) -> u8 {
    match kind {
        OperationKind::Delete { .. } => 30,
        OperationKind::Move { .. } | OperationKind::Archive { .. } => 20,
        OperationKind::Copy { .. } => 15,
        OperationKind::SetFlag { .. } => 10,
        OperationKind::Send { .. } => 25,
        OperationKind::CreateMailbox { .. } => 5,
    }
}

/// 一次移动引起的**身份变更**：旧本地主键 → 新本地主键。
///
/// 只在服务器改了邮件身份时产生（IMAP + UIDPLUS 的 `COPYUID`）；JMAP 与
/// 未提供 COPYUID 的服务器返回空映射，表示「id 没变，无需改写」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdRemap {
    pub from: Id,
    pub to: Id,
}

impl IdRemap {
    pub fn new(from: impl Into<Id>, to: impl Into<Id>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
        }
    }
}

impl OperationKind {
    /// 把队列操作里引用的邮件主键从 `from` 改写成 `to`。
    ///
    /// 移动成功后必须连带改写**同一条邮件后续排队的操作**，否则「先移动、再标星」
    /// 的第二条会打向已经不存在的旧 UID，而服务端会静默回 OK。
    /// 只改邮件主键；`Move`/`Archive` 的 `from`/`to` 是文件夹 id，不参与替换。
    ///
    /// 但 `Copy`/`Delete` 的 `from` 是**邮件当前所在文件夹**，主键一旦被改写，它就必须
    /// 跟着指向新文件夹：否则回放层会拿新的邮件主键配旧的文件夹再重定向一次，
    /// 把刚搬到新位置的邮件又指回原处，等于亲手把 remap 抹掉。
    pub fn remap_email_id(&mut self, from: &Id, to: &Id) {
        fn swap(id: &mut Id, from: &Id, to: &Id) -> bool {
            if id == from {
                *id = to.clone();
                true
            } else {
                false
            }
        }
        match self {
            OperationKind::SetFlag { email_id, .. } => {
                swap(email_id, from, to);
            }
            OperationKind::Move { email_ids, .. } | OperationKind::Archive { email_ids, .. } => {
                for id in email_ids.iter_mut() {
                    swap(id, from, to);
                }
            }
            OperationKind::Copy {
                email_ids,
                from: mailbox,
                ..
            }
            | OperationKind::Delete {
                email_ids,
                from: mailbox,
            } => {
                let mut moved = false;
                for id in email_ids.iter_mut() {
                    moved |= swap(id, from, to);
                }
                if moved {
                    if let Some(folder) = folder_segment(to) {
                        *mailbox = folder.to_string();
                    }
                }
            }
            OperationKind::Send { draft_id } => {
                swap(draft_id, from, to);
            }
            OperationKind::CreateMailbox { .. } => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfflineOperation {
    pub id: Id,
    pub account_id: Id,
    pub kind: OperationKind,
    pub created_at: DateTime<Utc>,
    pub attempts: u32,
    pub last_error: Option<String>,
}

impl OfflineOperation {
    pub fn new(id: impl Into<Id>, account_id: impl Into<Id>, kind: OperationKind) -> Self {
        Self {
            id: id.into(),
            account_id: account_id.into(),
            kind,
            created_at: Utc::now(),
            attempts: 0,
            last_error: None,
        }
    }
}

/// MailProvider 统一接口（A-001）。上层业务完全不感知底层协议。
#[async_trait]
pub trait MailProvider: Send + Sync {
    fn protocol(&self) -> ProtocolKind;
    fn capabilities(&self) -> ProviderCapabilities;

    async fn verify_credentials(&self) -> MailResult<AccountInfo>;
    async fn list_mailboxes(&self) -> MailResult<Vec<Mailbox>>;
    async fn create_mailbox(&self, draft: MailboxDraft) -> MailResult<Mailbox>;
    async fn rename_mailbox(&self, id: &Id, name: &str) -> MailResult<()>;
    async fn delete_mailbox(&self, id: &Id) -> MailResult<()>;

    async fn sync_emails(
        &self,
        req: SyncRequest,
        cancel: CancellationToken,
        reporter: Option<&dyn ProgressReporter>,
    ) -> MailResult<SyncPage>;

    async fn fetch_email(&self, id: &Id, include_body: bool) -> MailResult<Email>;
    async fn set_flags(&self, ids: &[Id], flag: Flag, enabled: bool) -> MailResult<()>;

    /// 移动邮件，并回传**身份映射**。
    ///
    /// 为什么需要返回值：IMAP 的邮件身份是 `(文件夹, UID)`，服务器 MOVE 会在目标文件夹
    /// 重新分配 UID（UIDPLUS 的 `COPYUID` 能拿到新旧对应关系）。本地主键 `账号/文件夹/UID`
    /// 是移动前算出来的，不回写就会失效——后续「给这封邮件加标记 / 删除」会打到不存在的旧 UID，
    /// 而服务端对这种 STORE **静默回 OK**，表现为「队列清空、零报错，但服务器什么都没发生」。
    ///
    /// 返回空 vec 表示身份没变（JMAP 的 emailId 稳定；或服务器未提供 COPYUID）。
    async fn move_emails(&self, ids: &[Id], from: &Id, to: &Id) -> MailResult<Vec<IdRemap>>;

    async fn copy_emails(&self, ids: &[Id], to: &Id) -> MailResult<()>;
    async fn delete_emails(&self, ids: &[Id]) -> MailResult<()>;

    async fn save_draft(&self, msg: &OutgoingMessage) -> MailResult<Id>;
    async fn send_email(&self, msg: OutgoingMessage) -> MailResult<SentReceipt>;

    async fn upload_blob(&self, data: &[u8], content_type: &str) -> MailResult<BlobRef>;
    async fn download_blob(&self, id: &str) -> MailResult<Vec<u8>>;
}
