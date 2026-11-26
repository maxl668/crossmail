//! 协议无关的同步结果 / 推送通知（编排层对上层的输出）。
//!
//! 刻意不复用 `sync::PushSyncEvent` / `sync::WakeupReason` 作为对外类型：那些是内部驱动
//! 细节（带 `ConflictReport`、`Duration` 等），而 UI 只需要「发生了什么 + 未读角标变成几」。
//! 这里做一次收敛，FFI 层再拍平成 UDL dictionary，避免把整个 `crossmail-sync` 的类型
//! 泄漏到跨语言契约里。

/// 一次同步的变更摘要（由 `SyncOutcome` 收敛而来）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncSummary {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub unchanged: u64,
    pub has_more: bool,
    pub full_resync: bool,
    pub new_state: Option<String>,
}

/// 单个文件夹的未读数（角标明细）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxUnread {
    pub mailbox_id: String,
    pub unread: u64,
}

/// 触发一次同步的原因（把 `sync::WakeupReason` 收敛成对外枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushReason {
    Push,
    Resync,
    Closing,
}

impl PushReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PushReason::Push => "push",
            PushReason::Resync => "resync",
            PushReason::Closing => "closing",
        }
    }
}

/// 编排层向上层（FFI / UI）汇报的一条通知。
///
/// 每条 `Synced` 都带**同步后的未读角标**：`unread_total` 是全账号合计，
/// `unread_by_mailbox` 是分箱明细。UI 拿到即可直接刷角标，不必再反查一次库。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushNotice {
    Synced {
        reason: PushReason,
        summary: SyncSummary,
        unread_total: u64,
        unread_by_mailbox: Vec<MailboxUnread>,
        /// 本次合并了多少条推送信号（1 表示没合并）。
        coalesced: u64,
    },
    Failed {
        reason: PushReason,
        error: String,
        /// `None` 表示不可重试，驱动即将退出。
        retry_in_ms: Option<u64>,
    },
    Closed,
}

impl PushNotice {
    /// 供 FFI 层拍平成 dictionary 用的事件类别串。
    pub fn kind(&self) -> &'static str {
        match self {
            PushNotice::Synced { .. } => "synced",
            PushNotice::Failed { .. } => "failed",
            PushNotice::Closed => "closed",
        }
    }
}
