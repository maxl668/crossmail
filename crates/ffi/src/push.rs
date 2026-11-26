//! 推送同步的对外契约类型（S6-16）：一次性同步报告、推送事件、可取消订阅句柄。
//!
//! 事件从 `crossmail-app` 的 `PushNotice` 拍平成 dictionary —— UI 侧不必理解 Rust
//! 枚举，直接读 `kind` + 字段即可。
//!
//! 关于 `PushObserver`：UDL 的 `callback interface` 在 `export_for_udl`（UDL 模式）
//! 下**不会**生成 trait 定义本身——uniffi 0.28.3 的 `do_export` 只在非 UDL 模式把
//! 原 item 回填，scaffold 生成的胶水却要 `impl PushObserver for ...` 并引用
//! `Box<dyn PushObserver>`。所以 trait 必须在这里手写、由 lib.rs 根部 re-export，
//! 否则 `crate::PushObserver` 解析不到。`Send + Sync` 是跨线程回调的硬约束。

use crossmail_app::{MailboxUnread, PushNotice, SyncSummary};

/// UI 实现的推送观察回调（UDL `callback interface PushObserver` 的 Rust 侧定义）。
///
/// 由 uniffi scaffold 生成的 `UniFFICallbackHandlerPushObserver` 实现本 trait，
/// 把 foreign 侧（Kotlin / Swift / Python）的方法调用转成回调。方法签名必须与
/// UDL 中 `void on_event(PushEventDto event)` 严格一致。
pub trait PushObserver: Send + Sync {
    fn on_event(&self, event: PushEventDto);
}

/// 一次性同步的变更摘要。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReportDto {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub unchanged: u64,
    pub has_more: bool,
    pub full_resync: bool,
    pub new_state: Option<String>,
}

impl From<SyncSummary> for SyncReportDto {
    fn from(s: SyncSummary) -> Self {
        Self {
            inserted: s.inserted,
            updated: s.updated,
            deleted: s.deleted,
            unchanged: s.unchanged,
            has_more: s.has_more,
            full_resync: s.full_resync,
            new_state: s.new_state,
        }
    }
}

/// 单个文件夹的未读数（角标明细）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxUnreadDto {
    pub mailbox_id: String,
    pub unread: u64,
}

impl From<MailboxUnread> for MailboxUnreadDto {
    fn from(m: MailboxUnread) -> Self {
        Self {
            mailbox_id: m.mailbox_id,
            unread: m.unread,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushEventKindDto {
    Synced,
    Failed,
    Closed,
}

/// 一条推送事件。所有字段恒有值：非适用字段取 0 / 空串 / null，
/// UI 先看 `kind` 再决定读哪些字段，避免跨语言的「可选结构体」复杂度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushEventDto {
    pub kind: PushEventKindDto,
    /// push / resync / closing（Synced、Failed 时有效；Closed 为空串）
    pub reason: String,
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub unchanged: u64,
    /// 同步后的全账号未读合计 —— 未读角标直接读它
    pub unread_total: u64,
    pub unread_by_mailbox: Vec<MailboxUnreadDto>,
    pub coalesced: u64,
    /// Failed 时的错误文本
    pub error: String,
    /// Failed 且可重试时是退避毫秒；不可重试或成功时为 null
    pub retry_in_ms: Option<u64>,
}

impl From<PushNotice> for PushEventDto {
    fn from(n: PushNotice) -> Self {
        match n {
            PushNotice::Synced {
                reason,
                summary,
                unread_total,
                unread_by_mailbox,
                coalesced,
            } => Self {
                kind: PushEventKindDto::Synced,
                reason: reason.as_str().to_string(),
                inserted: summary.inserted,
                updated: summary.updated,
                deleted: summary.deleted,
                unchanged: summary.unchanged,
                unread_total,
                unread_by_mailbox: unread_by_mailbox.into_iter().map(Into::into).collect(),
                coalesced,
                error: String::new(),
                retry_in_ms: None,
            },
            PushNotice::Failed {
                reason,
                error,
                retry_in_ms,
            } => Self {
                kind: PushEventKindDto::Failed,
                reason: reason.as_str().to_string(),
                inserted: 0,
                updated: 0,
                deleted: 0,
                unchanged: 0,
                unread_total: 0,
                unread_by_mailbox: Vec::new(),
                coalesced: 0,
                error,
                retry_in_ms,
            },
            PushNotice::Closed => Self {
                kind: PushEventKindDto::Closed,
                reason: String::new(),
                inserted: 0,
                updated: 0,
                deleted: 0,
                unchanged: 0,
                unread_total: 0,
                unread_by_mailbox: Vec::new(),
                coalesced: 0,
                error: String::new(),
                retry_in_ms: None,
            },
        }
    }
}

/// 一个可取消的推送订阅。UI 持有它，`cancel()` 后后台线程收敛退出。
///
/// 内部就一个 `PushHandle`（其方法都是 `&self`），不需要额外加锁。
pub struct PushSubscription {
    handle: crossmail_app::PushHandle,
}

impl PushSubscription {
    pub fn new(handle: crossmail_app::PushHandle) -> Self {
        Self { handle }
    }

    pub fn cancel(&self) {
        let _ = self.handle.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.handle.is_cancelled()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossmail_app::{MailboxUnread, PushReason, SyncSummary};

    fn synced(unread_total: u64) -> PushNotice {
        PushNotice::Synced {
            reason: PushReason::Push,
            summary: SyncSummary {
                inserted: 2,
                updated: 1,
                deleted: 0,
                unchanged: 5,
                has_more: false,
                full_resync: false,
                new_state: Some("state-9".into()),
            },
            unread_total,
            unread_by_mailbox: vec![
                MailboxUnread {
                    mailbox_id: "inbox".into(),
                    unread: 3,
                },
                MailboxUnread {
                    mailbox_id: "work".into(),
                    unread: 1,
                },
            ],
            coalesced: 4,
        }
    }

    #[test]
    fn synced_notice_maps_unread_badge_and_summary() {
        let dto: PushEventDto = synced(4).into();
        assert_eq!(dto.kind, PushEventKindDto::Synced);
        assert_eq!(dto.reason, "push");
        assert_eq!(dto.inserted, 2);
        assert_eq!(dto.updated, 1);
        assert_eq!(dto.unchanged, 5);
        assert_eq!(dto.unread_total, 4, "未读角标必须原样跨层带出");
        assert_eq!(dto.unread_by_mailbox.len(), 2);
        assert_eq!(dto.unread_by_mailbox[0].mailbox_id, "inbox");
        assert_eq!(dto.unread_by_mailbox[0].unread, 3);
        assert_eq!(dto.unread_by_mailbox[1].mailbox_id, "work");
        assert_eq!(dto.coalesced, 4);
        assert!(dto.error.is_empty());
        assert!(dto.retry_in_ms.is_none());
    }

    #[test]
    fn failed_notice_maps_retry_and_error() {
        let dto: PushEventDto = PushNotice::Failed {
            reason: PushReason::Resync,
            error: "网络断了".into(),
            retry_in_ms: Some(1500),
        }
        .into();
        assert_eq!(dto.kind, PushEventKindDto::Failed);
        assert_eq!(dto.reason, "resync");
        assert_eq!(dto.error, "网络断了");
        assert_eq!(dto.retry_in_ms, Some(1500));
        assert_eq!(dto.unread_total, 0);
        assert!(dto.unread_by_mailbox.is_empty());
    }

    #[test]
    fn closed_notice_is_all_empty() {
        let dto: PushEventDto = PushNotice::Closed.into();
        assert_eq!(dto.kind, PushEventKindDto::Closed);
        assert_eq!(dto.reason, "");
        assert_eq!(dto.unread_total, 0);
        assert!(dto.unread_by_mailbox.is_empty());
        assert!(dto.error.is_empty());
        assert!(dto.retry_in_ms.is_none());
    }
}
