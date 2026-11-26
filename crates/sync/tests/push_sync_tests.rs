//! S6-13：推送驱动增量同步的离线验证。
//!
//! 锁死的是四条「看起来能跑、真机会出事」的语义：
//! - 连成串的信号必须**合并成一次**同步，不能一条一拉；
//! - `Resync`（断线补齐）必须**立刻**拉，攒窗口等于扩大缺口；
//! - 同步失败必须**保留意图并退避重试**，不能丢掉这次变化；
//! - 不可重试错误（认证失效）必须**真的停止**，而不是无声刷屏。

use async_trait::async_trait;
use chrono::DateTime;
use crossmail_core::error::{ErrorKind, MailError, MailResult};
use crossmail_core::model::{Account, Email, Mailbox, MailboxRole, ProtocolKind};
use crossmail_core::provider::{CancellationToken, SyncPage};
use crossmail_core::watch::{RemoteSignal, RemoteWatcher, DATA_TYPE_EMAIL, DATA_TYPE_MAILBOX};
use crossmail_storage::{Database, SyncCursor};
use crossmail_sync::testing::{MockFailure, MockProvider};
use crossmail_sync::{
    run_push_sync, PushSyncConfig, PushSyncEvent, PushSyncReport, SyncEngine, WakeupReason,
};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ACC: &str = "acc";

// ---------------------------------------------------------------- 测试替身

/// 编排好的「信号源」：按顺序吐出信号、错误、关闭或沉默。
enum Feed {
    Signal(RemoteSignal),
    /// 干净关闭（`Ok(None)`）
    Closed,
    /// 读信号本身失败（真机上是连接异常）
    Fail(MailError),
    /// 沉默一段时间再关闭，用来验证窗口到期路径
    Quiet(Duration),
}

struct MockWatcher {
    queue: VecDeque<Feed>,
}

impl MockWatcher {
    fn new(feeds: Vec<Feed>) -> Self {
        Self {
            queue: feeds.into(),
        }
    }
}

#[async_trait]
impl RemoteWatcher for MockWatcher {
    async fn next_signal(&mut self) -> MailResult<Option<RemoteSignal>> {
        match self.queue.pop_front() {
            Some(Feed::Signal(s)) => Ok(Some(s)),
            Some(Feed::Closed) | None => Ok(None),
            Some(Feed::Fail(e)) => Err(e),
            Some(Feed::Quiet(d)) => {
                tokio::time::sleep(d).await;
                Ok(None)
            }
        }
    }
}

fn recorder() -> (
    Arc<Mutex<Vec<PushSyncEvent>>>,
    impl FnMut(PushSyncEvent) + Send,
) {
    let log: Arc<Mutex<Vec<PushSyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let handler = move |event: PushSyncEvent| {
        if let Ok(mut guard) = sink.lock() {
            guard.push(event);
        }
    };
    (log, handler)
}

fn reasons(log: &Arc<Mutex<Vec<PushSyncEvent>>>) -> Vec<WakeupReason> {
    log.lock()
        .expect("log")
        .iter()
        .filter_map(|event| match event {
            PushSyncEvent::Synced { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect()
}

fn failures(log: &Arc<Mutex<Vec<PushSyncEvent>>>) -> Vec<(String, Option<Duration>)> {
    log.lock()
        .expect("log")
        .iter()
        .filter_map(|event| match event {
            PushSyncEvent::Failed {
                error, retry_in, ..
            } => Some((error.clone(), *retry_in)),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------- 数据库

fn db() -> Arc<Mutex<Database>> {
    let mut db = Database::open_in_memory().expect("open db");
    db.migrate().expect("migrate");
    db.upsert_account(&Account::new(ACC, "user@example.com", ProtocolKind::Jmap))
        .expect("seed account");
    for (id, role) in [
        ("inbox", MailboxRole::Inbox),
        ("drafts", MailboxRole::Drafts),
    ] {
        let mut mb = Mailbox::new(id, ACC, id);
        mb.role = role;
        db.upsert_mailbox(&mb).expect("seed mailbox");
    }
    Arc::new(Mutex::new(db))
}

fn mail(id: &str) -> Email {
    let mut e = Email::new(id, ACC, "inbox");
    e.subject = format!("subject-{id}");
    e.received_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    e
}

fn page(state: &str, emails: Vec<Email>) -> SyncPage {
    SyncPage {
        emails,
        new_state: Some(state.into()),
        ..SyncPage::default()
    }
}

fn email_count(db: &Arc<Mutex<Database>>) -> usize {
    db.lock()
        .expect("db")
        .list_emails(ACC, 100)
        .expect("list")
        .len()
}

fn cursor(db: &Arc<Mutex<Database>>) -> SyncCursor {
    db.lock()
        .expect("db")
        .get_sync_cursor(ACC, "__all__")
        .expect("cursor")
}

/// 快速窗口：测试不等真实秒级延迟，但窗口必须大于 0 才谈得上合并。
fn fast_cfg() -> PushSyncConfig {
    PushSyncConfig {
        debounce: Duration::from_millis(40),
        max_delay: Duration::from_millis(200),
        error_backoff_base: Duration::from_millis(10),
        error_backoff_max: Duration::from_millis(40),
        ..PushSyncConfig::default()
    }
}

// ---------------------------------------------------------------- 用例

#[tokio::test]
async fn burst_of_signals_coalesces_into_single_sync() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    provider.queue_page(page("s1", vec![mail("a")]));
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    // 一次操作常同时动 Email 与 Mailbox，服务端还可能连推两次
    let mut watcher = MockWatcher::new(vec![
        Feed::Signal(RemoteSignal::changed(
            "b",
            DATA_TYPE_EMAIL,
            Some("s1".into()),
        )),
        Feed::Signal(RemoteSignal::changed(
            "b",
            DATA_TYPE_MAILBOX,
            Some("m1".into()),
        )),
        Feed::Signal(RemoteSignal::changed(
            "b",
            DATA_TYPE_EMAIL,
            Some("s2".into()),
        )),
    ]);
    let (_log, handler) = recorder();

    let report: PushSyncReport = run_push_sync(
        &engine,
        ACC,
        &mut watcher,
        &fast_cfg(),
        CancellationToken::new(),
        handler,
    )
    .await
    .expect("驱动正常退出");

    assert_eq!(report.runs, 1, "3 条信号只能换来 1 次同步");
    assert_eq!(
        provider.call_count("sync:"),
        1,
        "协议层只该被调用一次，否则合并等于没做"
    );
    assert_eq!(report.coalesced, 2, "省下 2 次同步");
    assert_eq!(report.ignored, 0);
    assert_eq!(report.last_state.as_deref(), Some("s1"));
    assert_eq!(email_count(&store), 1, "数据要真的落下");
    assert_eq!(cursor(&store).state.as_deref(), Some("s1"), "游标要推进");
}

#[tokio::test]
async fn window_flush_runs_without_waiting_for_close() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    provider.queue_page(page("s1", vec![mail("a")]));
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    // 一条信号之后就是沉默：只能靠窗口到期触发
    let mut watcher = MockWatcher::new(vec![
        Feed::Signal(RemoteSignal::changed("b", DATA_TYPE_EMAIL, None)),
        Feed::Quiet(Duration::from_millis(150)),
    ]);
    let (log, handler) = recorder();
    let cfg = PushSyncConfig {
        sync_on_close: false,
        ..fast_cfg()
    };

    let report = run_push_sync(
        &engine,
        ACC,
        &mut watcher,
        &cfg,
        CancellationToken::new(),
        handler,
    )
    .await
    .expect("驱动正常退出");

    assert_eq!(report.runs, 1);
    assert_eq!(reasons(&log), vec![WakeupReason::Push], "窗口到期触发");
}

#[tokio::test]
async fn resync_runs_immediately_even_with_long_window() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    provider.queue_page(page("s2", vec![mail("a")]));
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    let mut watcher = MockWatcher::new(vec![Feed::Signal(RemoteSignal::Resync)]);
    let (log, handler) = recorder();
    // 窗口配到 5 秒：若 Resync 也走窗口，这个用例会超时失败
    let cfg = PushSyncConfig {
        debounce: Duration::from_secs(5),
        max_delay: Duration::from_secs(5),
        sync_on_close: false,
        ..fast_cfg()
    };

    let started = Instant::now();
    let report = tokio::time::timeout(
        Duration::from_secs(2),
        run_push_sync(
            &engine,
            ACC,
            &mut watcher,
            &cfg,
            CancellationToken::new(),
            handler,
        ),
    )
    .await
    .expect("Resync 不能等合并窗口")
    .expect("驱动正常退出");

    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(report.runs, 1);
    assert_eq!(reasons(&log), vec![WakeupReason::Resync]);
}

#[tokio::test]
async fn resync_absorbs_pending_signals_into_one_run() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    provider.queue_page(page("s3", vec![mail("a")]));
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    // 先来一条变化把窗口打开，紧接着断线补齐：两者必须合成一次
    let mut watcher = MockWatcher::new(vec![
        Feed::Signal(RemoteSignal::changed("b", DATA_TYPE_EMAIL, None)),
        Feed::Signal(RemoteSignal::Resync),
    ]);
    let (log, handler) = recorder();
    let cfg = PushSyncConfig {
        debounce: Duration::from_secs(5),
        max_delay: Duration::from_secs(5),
        sync_on_close: false,
        ..fast_cfg()
    };

    let report = tokio::time::timeout(
        Duration::from_secs(2),
        run_push_sync(
            &engine,
            ACC,
            &mut watcher,
            &cfg,
            CancellationToken::new(),
            handler,
        ),
    )
    .await
    .expect("不该等到窗口到期")
    .expect("驱动正常退出");

    assert_eq!(report.runs, 1);
    assert_eq!(provider.call_count("sync:"), 1);
    assert_eq!(report.coalesced, 0, "只有 1 条待处理信号，没有「省下」");
    assert_eq!(reasons(&log), vec![WakeupReason::Resync]);
}

#[tokio::test]
async fn uninteresting_data_type_never_triggers_sync() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    // 只关心 Email：Mailbox / Thread 变化不该拉起同步
    let mut watcher = MockWatcher::new(vec![
        Feed::Signal(RemoteSignal::changed("b", DATA_TYPE_MAILBOX, None)),
        Feed::Signal(RemoteSignal::changed("b", "Thread", None)),
        Feed::Closed,
    ]);
    let (log, handler) = recorder();
    let cfg = PushSyncConfig {
        sync_on_close: false,
        ..PushSyncConfig::mail_only()
    };

    let report = run_push_sync(
        &engine,
        ACC,
        &mut watcher,
        &cfg,
        CancellationToken::new(),
        handler,
    )
    .await
    .expect("驱动正常退出");

    assert_eq!(report.runs, 0, "不关心的类型一次都不该拉");
    assert_eq!(report.ignored, 2);
    assert_eq!(provider.call_count("sync:"), 0);
    assert!(reasons(&log).is_empty());
}

#[tokio::test]
async fn closed_syncs_once_more_to_cover_the_tail() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    provider.queue_page(page("s1", vec![mail("a")]));
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    let mut watcher = MockWatcher::new(vec![
        Feed::Signal(RemoteSignal::changed("b", DATA_TYPE_EMAIL, None)),
        Feed::Quiet(Duration::from_millis(150)),
    ]);
    let (log, handler) = recorder();
    // sync_on_close 默认开：流结束前再对齐一次，防止尾部事件丢失
    let cfg = fast_cfg();

    let report = run_push_sync(
        &engine,
        ACC,
        &mut watcher,
        &cfg,
        CancellationToken::new(),
        handler,
    )
    .await
    .expect("驱动正常退出");

    assert_eq!(report.runs, 2, "窗口一次 + 关闭前收尾一次");
    assert_eq!(
        reasons(&log),
        vec![WakeupReason::Push, WakeupReason::Closing]
    );
    let closed = log
        .lock()
        .expect("log")
        .iter()
        .any(|e| matches!(e, PushSyncEvent::Closed));
    assert!(closed, "退出前必须发 Closed 事件，UI 才知道订阅断了");
}

#[tokio::test]
async fn retryable_failure_retries_then_lands_data() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    provider.fail_next(MockFailure::Network);
    provider.queue_page(page("s1", vec![mail("a"), mail("b")]));
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    let mut watcher = MockWatcher::new(vec![
        Feed::Signal(RemoteSignal::changed("b", DATA_TYPE_EMAIL, None)),
        Feed::Closed,
    ]);
    let (log, handler) = recorder();
    let cfg = PushSyncConfig {
        sync_on_close: false,
        ..fast_cfg()
    };

    let report = run_push_sync(
        &engine,
        ACC,
        &mut watcher,
        &cfg,
        CancellationToken::new(),
        handler,
    )
    .await
    .expect("网络抖动不该让驱动退出");

    assert_eq!(report.failures, 1);
    assert_eq!(report.runs, 1, "重试成功只算一次同步");
    let failed = failures(&log);
    assert_eq!(failed.len(), 1);
    assert!(failed[0].1.is_some(), "可重试错误必须给出重试间隔");
    assert_eq!(email_count(&store), 2, "重试之后数据要补齐");
    assert_eq!(cursor(&store).state.as_deref(), Some("s1"));
}

#[tokio::test]
async fn non_retryable_failure_stops_the_driver() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    provider.fail_next(MockFailure::Authentication);
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    let mut watcher = MockWatcher::new(vec![Feed::Signal(RemoteSignal::changed(
        "b",
        DATA_TYPE_EMAIL,
        None,
    ))]);
    let (log, handler) = recorder();

    let error = run_push_sync(
        &engine,
        ACC,
        &mut watcher,
        &fast_cfg(),
        CancellationToken::new(),
        handler,
    )
    .await
    .expect_err("认证失效必须冒出去");

    assert_eq!(error.kind(), ErrorKind::Authentication);
    assert_eq!(
        provider.call_count("sync:"),
        1,
        "不可重试的错误不该原地重试"
    );
    let failed = failures(&log);
    assert_eq!(failed.len(), 1);
    assert!(failed[0].1.is_none(), "不可重试时不得给重试间隔");
    assert!(cursor(&store).state.is_none(), "失败不能推进游标");
}

#[tokio::test]
async fn signal_read_failure_is_not_swallowed() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    // 推送通道自身报错（例如认证失效）：驱动不能装作没发生继续空转
    let mut watcher = MockWatcher::new(vec![Feed::Fail(MailError::Authentication(
        "推送通道凭证失效".into(),
    ))]);

    let error = run_push_sync(
        &engine,
        ACC,
        &mut watcher,
        &fast_cfg(),
        CancellationToken::new(),
        |_| {},
    )
    .await
    .expect_err("读信号失败要上抛");

    assert_eq!(error.kind(), ErrorKind::Authentication);
}

#[tokio::test]
async fn cancellation_takes_effect_immediately() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    // 队列里全是信号，但取消令牌一开始就是取消态
    let mut watcher = MockWatcher::new(vec![Feed::Signal(RemoteSignal::Resync)]);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = run_push_sync(&engine, ACC, &mut watcher, &fast_cfg(), cancel, |_| {})
        .await
        .expect_err("取消后必须退出");

    assert_eq!(error.kind(), ErrorKind::Cancelled);
    assert_eq!(provider.call_count("sync:"), 0);
}

#[tokio::test]
async fn second_sync_starts_from_advanced_cursor() {
    let provider = Arc::new(MockProvider::new());
    let store = db();
    provider.queue_page(page("s1", vec![mail("a")]));
    provider.queue_page(page("s2", vec![mail("a"), mail("b")]));
    let engine = SyncEngine::new(Arc::clone(&provider), Arc::clone(&store));

    // 两次独立触发（中间隔着关闭），验证游标确实在两次之间推进
    let mut watcher = MockWatcher::new(vec![
        Feed::Signal(RemoteSignal::Resync),
        Feed::Signal(RemoteSignal::Resync),
        Feed::Closed,
    ]);
    let cfg = PushSyncConfig {
        sync_on_close: false,
        ..fast_cfg()
    };

    let report = run_push_sync(
        &engine,
        ACC,
        &mut watcher,
        &cfg,
        CancellationToken::new(),
        |_| {},
    )
    .await
    .expect("驱动正常退出");

    assert_eq!(report.runs, 2);
    assert_eq!(provider.call_count("sync:"), 2);
    // 第二页只多了一封新邮件，若游标没推进就会重复走「新增」逻辑
    assert_eq!(email_count(&store), 2);
    assert_eq!(report.last_state.as_deref(), Some("s2"));
    assert_eq!(cursor(&store).state.as_deref(), Some("s2"));
}
