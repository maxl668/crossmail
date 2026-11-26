//! app 编排层离线回归（S6-16）。
//!
//! 全部用内存库 + `crossmail_sync::testing::MockProvider`，不连真服务器。
//! 锁死四件「看着能跑、其实会出事」的事：
//! - `to_notice` 把内部事件收敛成对外通知，且**每条 Synced 都带最新未读角标**；
//! - 一次性同步真的把邮件落库，返回摘要字段与库内一致；
//! - `subscribe_push` 立即返回、回调在独立线程、取消后能收敛退出；
//! - 建连/凭证失败以 `Failed` 通知上报，而不是让 UI 永远等一个不来的回调。

use chrono::DateTime;
use crossmail_app::{sync_account_once, to_notice, PushNotice, PushReason};
use crossmail_core::model::{Account, Email, Flag, Mailbox, MailboxRole, ProtocolKind};
use crossmail_core::provider::{CancellationToken, SyncPage};
use crossmail_storage::Database;
use crossmail_sync::engine::{ConflictReport, SyncOutcome};
use crossmail_sync::testing::MockProvider;
use crossmail_sync::{PushSyncEvent, WakeupReason};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

const ACC: &str = "acc";

fn fresh_db() -> Arc<Mutex<Database>> {
    let mut db = Database::open_in_memory().expect("open db");
    db.migrate().expect("migrate");
    db.upsert_account(&Account::new(ACC, "user@example.com", ProtocolKind::Jmap))
        .expect("seed account");
    for (id, role) in [("inbox", MailboxRole::Inbox), ("work", MailboxRole::Other)] {
        let mut mb = Mailbox::new(id, ACC, id);
        mb.role = role;
        db.upsert_mailbox(&mb).expect("seed mailbox");
    }
    Arc::new(Mutex::new(db))
}

fn mail(id: &str, mailbox: &str, read: bool) -> Email {
    let mut e = Email::new(id, ACC, mailbox);
    e.received_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    if read {
        e.flags.push(Flag::Seen);
    }
    e
}

fn outcome(inserted: usize) -> SyncOutcome {
    SyncOutcome {
        stats: crossmail_sync::engine::ApplyStats {
            inserted,
            updated: 0,
            unchanged: 0,
            deleted: 0,
            calibrated: 0,
        },
        new_state: Some("state-42".into()),
        new_modseq: None,
        full_resync: false,
        has_more: false,
    }
}

// ---------------------------------------------------------------- 事件收敛

#[test]
fn synced_notice_carries_fresh_unread_badge() {
    let db = fresh_db();
    // 直接落库造未读：inbox 2 封未读 + 1 封已读，work 1 封未读 → 合计 3。
    {
        let guard = db.lock().unwrap();
        for e in [
            mail("a", "inbox", false),
            mail("b", "inbox", false),
            mail("c", "inbox", true),
            mail("d", "work", false),
        ] {
            guard.upsert_email(&e).unwrap();
        }
    }

    let event = PushSyncEvent::Synced {
        reason: WakeupReason::Push,
        outcome: outcome(2),
        conflicts: ConflictReport::default(),
        coalesced: 3,
    };
    let notice = to_notice(event, &db, ACC);

    match notice {
        PushNotice::Synced {
            reason,
            summary,
            unread_total,
            unread_by_mailbox,
            coalesced,
        } => {
            assert_eq!(reason, PushReason::Push);
            assert_eq!(unread_total, 3, "已读邮件不计入角标");
            assert_eq!(coalesced, 3);
            assert_eq!(summary.inserted, 2);
            assert_eq!(summary.new_state.as_deref(), Some("state-42"));
            let inbox = unread_by_mailbox
                .iter()
                .find(|m| m.mailbox_id == "inbox")
                .expect("应有 inbox 明细");
            assert_eq!(inbox.unread, 2);
            let work = unread_by_mailbox
                .iter()
                .find(|m| m.mailbox_id == "work")
                .expect("应有 work 明细");
            assert_eq!(work.unread, 1);
        }
        other => panic!("事件类别不对: {other:?}"),
    }
}

#[test]
fn failed_and_closed_notices_are_faithful() {
    let db = fresh_db();
    let failed = to_notice(
        PushSyncEvent::Failed {
            reason: WakeupReason::Resync,
            error: "网络断了".into(),
            retry_in: Some(Duration::from_millis(1500)),
        },
        &db,
        ACC,
    );
    match failed {
        PushNotice::Failed {
            reason,
            error,
            retry_in_ms,
        } => {
            assert_eq!(reason, PushReason::Resync);
            assert_eq!(error, "网络断了");
            assert_eq!(retry_in_ms, Some(1500));
        }
        other => panic!("应为 Failed: {other:?}"),
    }

    let closed = to_notice(PushSyncEvent::Closed, &db, ACC);
    assert_eq!(closed, PushNotice::Closed);
    assert_eq!(closed.kind(), "closed");

    // 不可重试错误 retry_in_ms 为 None
    let fatal = to_notice(
        PushSyncEvent::Failed {
            reason: WakeupReason::Push,
            error: "认证失效".into(),
            retry_in: None,
        },
        &db,
        ACC,
    );
    match fatal {
        PushNotice::Failed { retry_in_ms, .. } => assert_eq!(retry_in_ms, None),
        other => panic!("应为 Failed: {other:?}"),
    }
}

// ---------------------------------------------------------------- 一次性同步

#[tokio::test]
async fn sync_account_once_applies_page_and_returns_summary() {
    let db = fresh_db();
    let provider = Arc::new(MockProvider::new());
    provider.queue_page(SyncPage {
        emails: vec![mail("m1", "inbox", false), mail("m2", "inbox", false)],
        new_state: Some("s1".into()),
        ..SyncPage::default()
    });

    let summary = sync_account_once(&db, provider, ACC, CancellationToken::new())
        .await
        .expect("sync ok");

    assert_eq!(summary.inserted, 2);
    assert_eq!(summary.new_state.as_deref(), Some("s1"));
    assert!(!summary.has_more);
    // 落库证据：未读角标变成 2
    let unread = db.lock().unwrap().unread_counts(ACC).unwrap();
    assert_eq!(unread.get("inbox").copied(), Some(2));
}

#[tokio::test]
async fn sync_account_once_propagates_provider_failure() {
    use crossmail_sync::testing::MockFailure;
    let db = fresh_db();
    let provider = Arc::new(MockProvider::new());
    provider.fail_next(MockFailure::Network);
    let err = sync_account_once(&db, provider, ACC, CancellationToken::new())
        .await
        .expect_err("网络失败应冒泡");
    assert!(err.is_retryable(), "网络错误应可重试");
}

/// 真机探针（S6-16 补测）抓到的第二个缺陷：provider 返回的 `Email.account_id`
/// 是协议侧 id（JMAP 的 `b`），与本地 `accounts(id)`（FFI 用 UUID）不是同一
/// 命名空间。`SyncEngine::apply_page` 落库前必须归一化成本地 account_id，
/// 否则 `emails.account_id` 外键失败。纯 Mock 锁死，无需凭据。
#[tokio::test]
async fn sync_normalizes_remote_email_account_id_to_local() {
    let db = fresh_db(); // accounts(id) = ACC
    let provider = Arc::new(MockProvider::new());
    let mut remote = mail("m1", "inbox", false);
    remote.account_id = "protocol-side-b".into(); // 模拟服务端 accountId
    provider.queue_page(SyncPage {
        emails: vec![remote],
        new_state: Some("s1".into()),
        ..SyncPage::default()
    });

    let summary = sync_account_once(&db, provider.clone(), ACC, CancellationToken::new())
        .await
        .expect("远端 account_id 必须归一到本地，否则 emails 外键失败");
    assert_eq!(summary.inserted, 1);

    {
        let guard = db.lock().unwrap();
        let stored = guard.get_email("m1").unwrap().expect("邮件应已落库");
        assert_eq!(stored.account_id, ACC, "落库身份必须是本地账号 id");
    } // guard 在此出作用域，不跨下面的 await

    // 再同步同一封：account_id 已归一，merge 不应把它误判成 updated
    provider.queue_page(SyncPage {
        emails: vec![{
            let mut e = mail("m1", "inbox", false);
            e.account_id = "protocol-side-b".into();
            e
        }],
        new_state: Some("s2".into()),
        ..SyncPage::default()
    });
    let again = sync_account_once(&db, provider, ACC, CancellationToken::new())
        .await
        .expect("二次同步");
    assert_eq!(again.updated, 0, "归一后重复同步不得误判为更新");
    assert_eq!(again.unchanged, 1);
}

/// 真机探针（S6-16 补测）抓到的缺陷：本地库没有服务器真实 folder id 时，
/// 初始快照 `upsert_email` 写 `email_mailboxes` 撞外键，同步直接失败，
/// 推送会话甚至静默收尾成 `Closed`。这里用纯 Mock 锁死「同步前先落库 folder」，
/// 无需真服务器凭据——把 seed_mailboxes 去掉，本用例立刻 FOREIGN KEY 失败。
#[tokio::test]
async fn sync_seeds_remote_mailboxes_before_applying_page() {
    let db = fresh_db();
    // 一个本地库里根本不存在的服务器文件夹（真机 JMAP 常是 `d`/`b` 这类短 id）
    let mut remote = Mailbox::new("remote-only", ACC, "Remote");
    remote.role = MailboxRole::Other;
    let provider = Arc::new(MockProvider::new());
    provider.set_mailboxes(vec![remote]);
    provider.queue_page(SyncPage {
        emails: vec![mail("m1", "remote-only", false)],
        new_state: Some("s1".into()),
        ..SyncPage::default()
    });

    let summary = sync_account_once(&db, provider, ACC, CancellationToken::new())
        .await
        .expect("服务器 folder 必须先落库，否则 email_mailboxes 外键失败");

    assert_eq!(summary.inserted, 1);
    let unread = db.lock().unwrap().unread_counts(ACC).unwrap();
    assert_eq!(
        unread.get("remote-only").copied(),
        Some(1),
        "新 folder 必须已落库，邮件才能挂上去并被角标统计"
    );
}

// ------------------------------------------------------------ 订阅线程/取消

#[test]
fn subscribe_push_reports_startup_failure_and_closes() {
    let db = fresh_db();
    let account = Account::new(ACC, "user@example.com", ProtocolKind::Jmap);
    let (tx, rx) = mpsc::channel();
    // 空凭证在 build_backend 阶段即失败，走真实的「失败通知 + 收尾」路径，
    // 不需要连服务器，也不依赖网络时序。
    let handle = crossmail_app::subscribe_push(db, account, String::new(), move |n| {
        let _ = tx.send(n);
    })
    .expect("订阅启动成功");

    let mut kinds = Vec::new();
    while let Ok(notice) = rx.recv_timeout(Duration::from_secs(5)) {
        kinds.push(notice.kind().to_string());
        if notice.kind() == "closed" {
            break;
        }
    }
    assert_eq!(
        kinds,
        vec!["failed".to_string(), "closed".to_string()],
        "建连失败必须以 Failed 上报并补一条 Closed，UI 才不会永远等"
    );
    handle.cancel();
    handle.join();
}

#[test]
fn cancel_is_idempotent_and_observable() {
    let db = fresh_db();
    let account = Account::new(ACC, "user@example.com", ProtocolKind::Jmap);
    let (tx, _rx) = mpsc::channel::<PushNotice>();
    let handle = crossmail_app::subscribe_push(db, account, String::new(), move |n| {
        let _ = tx.send(n);
    })
    .expect("订阅启动成功");

    assert!(!handle.is_cancelled());
    assert!(handle.cancel(), "首次取消返回 true");
    assert!(!handle.cancel(), "重复取消返回 false");
    assert!(handle.is_cancelled());
    handle.join();
}
