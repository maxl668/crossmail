//! 远端变更信号（推送 / 长连接）的协议无关抽象。
//!
//! 为什么这层要放在 `core`：JMAP 的 EventSource 在 `crossmail-jmap` 里，IMAP 的
//! IDLE 在 `crossmail-imap` 里，而消费方是 `crossmail-sync` 的同步引擎。三者都只依赖
//! `core`，把信号类型定在这里，同步引擎就不必反向依赖任何具体协议 crate。
//!
//! 三条来自真机实测的硬约束（决定了这个抽象只能长这样）：
//!
//! 1. **心跳不是数据**：JMAP 每约 30 秒发一次 `event: ping`，IMAP 侧也有 NOOP 保活。
//!    如果把它当变更信号，就等于每 30 秒白拉一次全量——所以适配器**必须吞掉心跳**，
//!    不能让它冒到同步层。
//! 2. **断线必然有缺口**：真机 SSE 从不下发 `id:`，`Last-Event-ID` 续传形同虚设；
//!    IDLE 也不补发断线期间的变化。因此「重新连上」必须显式表达为 [`RemoteSignal::Resync`]，
//!    让上层重新拉取对齐，绝不能因为「没收到变化」推断「没有变化」。
//! 3. **流会正常结束**：`closeafter=state` 的一次性订阅在首个 state 后由服务端主动断开，
//!    这是正常收尾而不是故障，对应 [`RemoteSignal::Closed`]。

use crate::error::MailResult;
use crate::model::Id;
use async_trait::async_trait;

/// 数据类型常量（与 JMAP 的 `changed[账号][类型]` 键名一致；IMAP 后续映射到同名）。
pub const DATA_TYPE_EMAIL: &str = "Email";
/// 见 [`DATA_TYPE_EMAIL`]。
pub const DATA_TYPE_MAILBOX: &str = "Mailbox";

/// 一条远端变更信号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteSignal {
    /// 某账号下某类数据发生了变化。
    ///
    /// `state` 是服务端给的版本串（JMAP 的 `state`、IMAP 的 `MODSEQ`），**仅作诊断与
    /// 幂等去重参考**：真正的增量游标由协议层在 `SyncPage::new_state` 里回传，同步引擎
    /// 只认那个，避免这里猜错导致漏数据。
    Changed {
        /// 服务端账号标识。
        ///
        /// 注意：这是**服务端**的账号 id（真机 Stalwart 上是 `"b"`），不是本地 `account_id`。
        /// 单账号客户端不必据此匹配；多账号路由时才需要映射表，此处不做隐式假设。
        account: Id,
        data_type: String,
        state: Option<Id>,
    },
    /// 断线重连成功：断线期间的变化不会补推，**必须重新拉取对齐**。
    Resync,
    /// 订阅流正常结束（一次性订阅收到首个 state，或对端干净关闭）。
    Closed,
}

impl RemoteSignal {
    pub fn changed(
        account: impl Into<Id>,
        data_type: impl Into<String>,
        state: Option<Id>,
    ) -> Self {
        Self::Changed {
            account: account.into(),
            data_type: data_type.into(),
            state,
        }
    }

    /// 数据类型（非 `Changed` 返回 `None`）。
    pub fn data_type(&self) -> Option<&str> {
        match self {
            Self::Changed { data_type, .. } => Some(data_type.as_str()),
            _ => None,
        }
    }
}

/// 远端变更信号的来源（推送流）。
///
/// 生命周期由调用方驱动：每次 [`RemoteWatcher::next_signal`] 取一条，内部负责重连与
/// 心跳过滤。这样既不需要把连接塞进后台任务（避免 `'static` 自引用），也能在离线
/// 测试里用假 watcher 完整覆盖驱动的合并、退避、取消路径。
///
/// **订阅顺序**：实现方必须保证「返回 watcher 时连接已建立」（JMAP 侧对应
/// `PushSession::connect`）。先拉快照后建连会静默丢掉两者之间的变更。
#[async_trait]
pub trait RemoteWatcher: Send {
    /// 取下一条信号；`Ok(None)` 表示流已结束（等价于 [`RemoteSignal::Closed`]）。
    ///
    /// 取消时返回 [`crate::error::MailError::Cancelled`]。
    async fn next_signal(&mut self) -> MailResult<Option<RemoteSignal>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_type_only_for_changed() {
        let changed = RemoteSignal::changed("b", DATA_TYPE_EMAIL, Some("s1".into()));
        assert_eq!(changed.data_type(), Some("Email"));
        assert_eq!(RemoteSignal::Resync.data_type(), None);
        assert_eq!(RemoteSignal::Closed.data_type(), None);
    }

    #[test]
    fn changed_carries_server_account_and_state() {
        let sig = RemoteSignal::changed("b", DATA_TYPE_MAILBOX, None);
        match sig {
            RemoteSignal::Changed {
                account,
                data_type,
                state,
            } => {
                assert_eq!(account, "b");
                assert_eq!(data_type, "Mailbox");
                assert_eq!(state, None);
            }
            other => panic!("信号类型不对: {other:?}"),
        }
    }
}
