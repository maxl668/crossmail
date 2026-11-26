//! 推送驱动的增量同步（S6-13）：把「远端有变化」直接翻译成「现在就跑一次增量同步」。
//!
//! 为什么值得单独一层：S3-07 的 [`crate::scheduler::Scheduler`] 是**轮询**语义
//! （定时到期才拉），推到 5 分钟一次既慢又费。有了推送流，同步就变成事件驱动：
//! 收到变化 → 立刻同步。但直接「一条信号一次同步」也是错的，四个坑必须在这里堵住：
//!
//! 1. **信号会连成串**：一次操作常触发多条（Email + Mailbox），且服务端可能连推数次。
//!    逐条同步等于把一次改动拆成 N 次请求。这里用**合并窗口**（[`PushSyncConfig::debounce`]）
//!    把窗口内的信号攒成一次同步，同时用 [`PushSyncConfig::max_delay`] 封顶，
//!    避免持续变化把同步无限期推迟（饥饿）。
//! 2. **`Resync` 不能等**：断线期间的变化不会补推，攒窗口只会扩大缺口，必须立刻拉。
//! 3. **同步失败不能丢意图**：游标只在成功后推进（见 `SyncEngine`），所以失败后
//!    按指数退避重试同一次同步即可，不需要记住「哪些信号已经消费过」。
//! 4. **不可重试的错误必须冒出去**：认证失效重试一万次也不会成功，硬撑只会把日志刷满。
//!
//! 驱动本身不写库、不依赖任何协议：它只吃 [`RemoteWatcher`]，调 [`SyncEngine`]，
//! 把结果通过回调交出去（UI 刷新 / 未读角标 / 日志由上层决定）。

use crate::engine::{ConflictReport, SyncEngine, SyncOutcome};
use crossmail_core::error::MailResult;
use crossmail_core::model::Id;
use crossmail_core::provider::{CancellationToken, MailProvider};
use crossmail_core::watch::{RemoteSignal, RemoteWatcher, DATA_TYPE_EMAIL, DATA_TYPE_MAILBOX};
use std::time::Duration;
use tokio::time::Instant;

/// 一次同步的触发原因（排障时用来区分「推送来的」和「补对齐来的」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeupReason {
    /// 收到明确的「某类数据变了」，且合并窗口已到期。
    Push,
    /// 断线重连后的补齐：断线期间可能漏过事件，不受合并窗口约束。
    Resync,
    /// 订阅流结束前的收尾同步。
    Closing,
}

/// 推送驱动配置。
#[derive(Debug, Clone)]
pub struct PushSyncConfig {
    /// 合并窗口：窗口内到达的多条信号攒成一次同步。
    pub debounce: Duration,
    /// 最长等待：从第一条信号算起，超过这个时长必须同步，避免持续变化导致饥饿。
    pub max_delay: Duration,
    /// 关心的数据类型；空集合或含 `*` 表示全部关心。
    pub interests: Vec<String>,
    /// 订阅流结束时，即使没有待处理变化也补一次对齐（默认开，安全优先）。
    pub sync_on_close: bool,
    /// 可重试错误的退避起点。
    pub error_backoff_base: Duration,
    /// 可重试错误的退避上限。
    pub error_backoff_max: Duration,
    /// 限定只同步某个文件夹；`None` 表示整账号。
    pub mailbox_id: Option<Id>,
}

impl Default for PushSyncConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(400),
            max_delay: Duration::from_secs(5),
            interests: vec![DATA_TYPE_EMAIL.to_string(), DATA_TYPE_MAILBOX.to_string()],
            sync_on_close: true,
            error_backoff_base: Duration::from_millis(500),
            error_backoff_max: Duration::from_secs(30),
            mailbox_id: None,
        }
    }
}

impl PushSyncConfig {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只关心邮件本体（不关心文件夹结构变化）。
    pub fn mail_only() -> Self {
        Self {
            interests: vec![DATA_TYPE_EMAIL.to_string()],
            ..Self::default()
        }
    }

    /// 某类型是否值得触发同步。`interests` 为空或含 `*` 时全部关心。
    pub fn wants(&self, data_type: &str) -> bool {
        if self.interests.is_empty() || self.interests.iter().any(|t| t == "*") {
            return true;
        }
        self.interests
            .iter()
            .any(|t| t.eq_ignore_ascii_case(data_type))
    }

    fn window(&self) -> Duration {
        self.debounce
    }

    fn ceiling(&self) -> Duration {
        // 上限不能小于窗口，否则每来一条信号都立刻触发，合并形同虚设
        self.max_delay.max(self.debounce)
    }
}

/// 驱动产出的事件，交给上层（UI 刷新 / 未读角标 / 日志）。
#[derive(Debug, Clone)]
pub enum PushSyncEvent {
    /// 一次同步完成。
    Synced {
        reason: WakeupReason,
        outcome: SyncOutcome,
        conflicts: ConflictReport,
        /// 本次合并了多少条信号（1 表示没发生合并）。
        coalesced: usize,
    },
    /// 一次同步失败。`retry_in` 为 `None` 表示不可重试，驱动即将退出。
    Failed {
        reason: WakeupReason,
        error: String,
        retry_in: Option<Duration>,
    },
    /// 订阅流结束，驱动已退出。
    Closed,
}

/// 驱动累计统计。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushSyncReport {
    /// 实际执行的同步次数。
    pub runs: usize,
    /// 因合并而省下的同步次数（收到 3 条信号跑 1 次 → 省 2 次）。
    pub coalesced: usize,
    /// 因类型不关心而忽略的信号数。
    pub ignored: usize,
    /// 失败次数（含重试）。
    pub failures: usize,
    /// 最后一次同步成功后的服务端 state（排障用）。
    pub last_state: Option<String>,
}

/// 驱动状态机在每一轮只做一件事：要么处理一条信号，要么处理窗口到期。
enum Step {
    Signal(Option<RemoteSignal>),
    Flush,
}

/// 跑推送驱动的同步循环，直到取消或订阅流结束。
///
/// 调用顺序要求：**先拿到 watcher（连接已建立），再自己拉一次初始快照，然后调本函数。**
/// 反过来的话，快照与建连之间的变化会被静默丢掉。本函数不替你做初始同步，
/// 因为初始同步要不要阻塞 UI 是上层的事。
pub async fn run_push_sync<P, W, F>(
    engine: &SyncEngine<P>,
    account_id: &str,
    watcher: &mut W,
    cfg: &PushSyncConfig,
    cancel: CancellationToken,
    mut on_event: F,
) -> MailResult<PushSyncReport>
where
    P: MailProvider,
    W: RemoteWatcher + ?Sized,
    F: FnMut(PushSyncEvent),
{
    let mut report = PushSyncReport::default();
    // 已收到、还没被一次同步消化掉的信号数
    let mut pending = 0usize;
    let mut first_seen: Option<Instant> = None;
    let mut deadline: Option<Instant> = None;
    let mut backoff = cfg.error_backoff_base;

    loop {
        cancel.check()?;

        // 有窗口在等就等窗口到期或新信号，谁先到算谁；否则阻塞等信号。
        let step = match deadline {
            None => Step::Signal(watcher.next_signal().await?),
            Some(at) => tokio::select! {
                biased;
                _ = tokio::time::sleep_until(at) => Step::Flush,
                signal = watcher.next_signal() => Step::Signal(signal?),
            },
        };

        let mut action: Option<(WakeupReason, usize)> = None;
        let mut stop = false;

        match step {
            Step::Flush => action = Some((WakeupReason::Push, pending)),
            Step::Signal(Some(RemoteSignal::Changed { data_type, .. })) => {
                if cfg.wants(&data_type) {
                    pending += 1;
                    let now = Instant::now();
                    let first = *first_seen.get_or_insert(now);
                    let target = (now + cfg.window()).min(first + cfg.ceiling());
                    deadline = Some(deadline.map_or(target, |d| d.min(target)));
                } else {
                    report.ignored += 1;
                }
            }
            Step::Signal(Some(RemoteSignal::Resync)) => {
                // 缺口已经产生，攒窗口只会让缺口变大
                action = Some((WakeupReason::Resync, pending));
            }
            // `Ok(None)` 与 `Closed` 同义：订阅流结束
            Step::Signal(None) | Step::Signal(Some(RemoteSignal::Closed)) => {
                stop = true;
                action = if pending > 0 {
                    Some((WakeupReason::Closing, pending))
                } else if cfg.sync_on_close {
                    Some((WakeupReason::Closing, 0))
                } else {
                    None
                };
            }
        }

        if let Some((reason, coalesced)) = action {
            pending = 0;
            first_seen = None;
            deadline = None;

            // 失败只在「可重试」时重来：引擎的游标只在成功应用后推进，
            // 所以重试同一份请求是安全的，不会重复也不会漏。
            let pair = loop {
                match engine
                    .sync_with_report(account_id, cfg.mailbox_id.clone(), cancel.clone(), None)
                    .await
                {
                    Ok(pair) => break pair,
                    Err(error) if error.is_retryable() => {
                        report.failures += 1;
                        let wait = backoff;
                        backoff = (backoff * 2).min(cfg.error_backoff_max);
                        on_event(PushSyncEvent::Failed {
                            reason,
                            error: error.to_string(),
                            retry_in: Some(wait),
                        });
                        sleep_or_cancel(&cancel, wait).await?;
                    }
                    Err(error) => {
                        report.failures += 1;
                        on_event(PushSyncEvent::Failed {
                            reason,
                            error: error.to_string(),
                            retry_in: None,
                        });
                        return Err(error);
                    }
                }
            };

            let (outcome, conflicts) = pair;
            report.runs += 1;
            report.coalesced += coalesced.saturating_sub(1);
            if outcome.new_state.is_some() {
                report.last_state = outcome.new_state.clone();
            }
            backoff = cfg.error_backoff_base;
            on_event(PushSyncEvent::Synced {
                reason,
                outcome,
                conflicts,
                coalesced,
            });
        }

        if stop {
            on_event(PushSyncEvent::Closed);
            break;
        }
    }

    Ok(report)
}

/// 分片睡眠，保证取消能及时生效（长退避时不会卡住退出）。
async fn sleep_or_cancel(cancel: &CancellationToken, total: Duration) -> MailResult<()> {
    let step = Duration::from_millis(25);
    let mut left = total;
    while left > Duration::ZERO {
        cancel.check()?;
        let slice = left.min(step);
        tokio::time::sleep(slice).await;
        left -= slice;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_interests_cover_mail_and_mailbox() {
        let cfg = PushSyncConfig::default();
        assert!(cfg.wants(DATA_TYPE_EMAIL));
        assert!(cfg.wants(DATA_TYPE_MAILBOX));
        assert!(!cfg.wants("Thread"));
    }

    #[test]
    fn empty_or_wildcard_interests_accept_everything() {
        let cfg = PushSyncConfig {
            interests: Vec::new(),
            ..PushSyncConfig::default()
        };
        assert!(cfg.wants("Anything"));

        let cfg = PushSyncConfig::mail_only();
        assert!(cfg.wants("email"), "类型匹配忽略大小写");
        assert!(!cfg.wants(DATA_TYPE_MAILBOX));

        let cfg = PushSyncConfig {
            interests: vec!["*".into()],
            ..PushSyncConfig::default()
        };
        assert!(cfg.wants("Anything"));
    }

    #[test]
    fn ceiling_never_below_window() {
        // max_delay 配得比 debounce 小是常见误配：不能因此让合并窗口失效
        let cfg = PushSyncConfig {
            debounce: Duration::from_secs(2),
            max_delay: Duration::from_millis(100),
            ..PushSyncConfig::default()
        };
        assert_eq!(cfg.ceiling(), Duration::from_secs(2));
    }
}
