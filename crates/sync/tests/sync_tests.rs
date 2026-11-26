//! S3 引擎/回放一致性验证（S3-08）。

use chrono::DateTime;
use crossmail_core::error::MailResult;
use crossmail_core::model::{Account, Email, Flag, Id, Mailbox, MailboxRole, ProtocolKind};
use crossmail_core::provider::{CancellationToken, OfflineOperation, OperationKind, SyncPage};
use crossmail_storage::{Database, SyncCursor};
use crossmail_sync::engine::ApplyStats;
use crossmail_sync::testing::{MockFailure, MockProvider};
use crossmail_sync::{OfflineReplayer, ReplayConfig, SyncEngine};
use std::sync::{Arc, Mutex};

const ACC: &str = "acc";

/// 测试用数据库句柄：引擎要求 `Arc<Mutex<Database>>`（SQLite 连接非 Sync），
/// 这里把加锁细节收在一处，测试正文仍按同步风格调用。
struct TestDb {
    inner: Arc<Mutex<Database>>,
}

impl TestDb {
    fn shared(&self) -> Arc<Mutex<Database>> {
        Arc::clone(&self.inner)
    }

    fn with<T>(&self, f: impl FnOnce(&Database) -> T) -> T {
        let guard = self.inner.lock().expect("db lock");
        f(&guard)
    }

    fn upsert_email(&self, e: &Email) -> MailResult<()> {
        self.with(|d| d.upsert_email(e))
    }
    fn enqueue(&self, op: &OfflineOperation) -> MailResult<()> {
        self.with(|d| d.enqueue(op))
    }
    fn get_email(&self, id: &str) -> MailResult<Option<Email>> {
        self.with(|d| d.get_email(id))
    }
    fn list_emails(&self, account_id: &str, limit: u32) -> MailResult<Vec<Email>> {
        self.with(|d| d.list_emails(account_id, limit))
    }
    fn list_emails_in_mailbox(
        &self,
        account_id: &str,
        mailbox_id: &str,
        limit: u32,
    ) -> MailResult<Vec<Email>> {
        self.with(|d| d.list_emails_in_mailbox(account_id, mailbox_id, limit))
    }
    fn pending_operations(&self, account_id: &str) -> MailResult<Vec<OfflineOperation>> {
        self.with(|d| d.pending_operations(account_id))
    }
    fn get_sync_cursor(&self, account_id: &str, mailbox_id: &str) -> MailResult<SyncCursor> {
        self.with(|d| d.get_sync_cursor(account_id, mailbox_id))
    }
}

/// 建库 + 账号 + 指定文件夹（email_mailboxes 有 mailbox 外键，必须先落库）。
fn db_with(mailboxes: &[(&str, MailboxRole)]) -> TestDb {
    let mut db = Database::open_in_memory().expect("open db");
    db.migrate().expect("migrate");
    db.upsert_account(&Account::new(ACC, "user@example.com", ProtocolKind::Jmap))
        .expect("seed account");
    for (id, role) in mailboxes {
        let mut mb = Mailbox::new(*id, ACC, *id);
        mb.role = *role;
        db.upsert_mailbox(&mb).expect("seed mailbox");
    }
    TestDb {
        inner: Arc::new(Mutex::new(db)),
    }
}

fn fresh_db() -> TestDb {
    db_with(&[
        ("inbox", MailboxRole::Inbox),
        ("drafts", MailboxRole::Drafts),
        ("archive", MailboxRole::Archive),
    ])
}

/// 固定接收时间：幂等性断言要求同一封邮件两次解析出的字段完全一致。
fn mail(id: &str, mailbox: &str) -> Email {
    let mut e = Email::new(id, ACC, mailbox);
    e.subject = format!("subject-{id}");
    e.received_at = DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    e
}

fn page(emails: Vec<Email>) -> SyncPage {
    SyncPage {
        emails,
        new_state: Some("s1".into()),
        ..SyncPage::default()
    }
}

fn engine(provider: &Arc<MockProvider>, db: &TestDb) -> SyncEngine<MockProvider> {
    SyncEngine::new(Arc::clone(provider), db.shared())
}

#[tokio::test]
async fn repeat_sync_is_idempotent() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    provider.queue_page(page(vec![mail("a", "inbox"), mail("b", "inbox")]));
    provider.queue_page(page(vec![mail("a", "inbox"), mail("b", "inbox")]));

    let eng = engine(&provider, &db);
    let first = eng
        .sync(ACC, None, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(first.stats.inserted, 2);
    assert_eq!(first.stats.updated, 0);

    let second = eng
        .sync(ACC, None, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(
        second.stats,
        ApplyStats {
            inserted: 0,
            updated: 0,
            unchanged: 2,
            ..ApplyStats::default()
        },
        "重复同步必须零新增零更新"
    );
    assert_eq!(db.list_emails(ACC, 100).unwrap().len(), 2);
}

#[tokio::test]
async fn deleted_ids_are_removed_once() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    provider.queue_page(page(vec![mail("a", "inbox"), mail("b", "inbox")]));
    provider.queue_page(SyncPage {
        emails: vec![],
        deleted_ids: vec!["b".into()],
        new_state: Some("s2".into()),
        ..SyncPage::default()
    });
    provider.queue_page(SyncPage {
        emails: vec![],
        deleted_ids: vec!["b".into()],
        new_state: Some("s3".into()),
        ..SyncPage::default()
    });

    let eng = engine(&provider, &db);
    eng.sync(ACC, None, CancellationToken::new(), None)
        .await
        .unwrap();
    let second = eng
        .sync(ACC, None, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(second.stats.deleted, 1);
    let third = eng
        .sync(ACC, None, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(third.stats.deleted, 0, "重复删除不该重复计数");
    assert!(db.get_email("b").unwrap().is_none());
}

#[tokio::test]
async fn remote_change_overrides_local_when_no_pending_op() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    let mut local = mail("a", "inbox");
    local.subject = "旧标题".into();
    db.upsert_email(&local).unwrap();

    let mut remote = mail("a", "inbox");
    remote.subject = "新标题".into();
    remote.flags = vec![Flag::Seen];
    provider.queue_page(page(vec![remote]));

    let eng = engine(&provider, &db);
    let (outcome, report) = eng
        .sync_with_report(ACC, None, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(outcome.stats.updated, 1);
    assert_eq!(report.local_overrides, 0);
    let stored = db.get_email("a").unwrap().unwrap();
    assert_eq!(stored.subject, "新标题");
    assert!(stored.flags.contains(&Flag::Seen));
}

#[tokio::test]
async fn pending_move_keeps_local_mailbox() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    // 本地已乐观移动：archive
    let mut local = mail("a", "inbox");
    local.mailbox_ids = vec!["archive".into()];
    db.upsert_email(&local).unwrap();

    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Move {
            email_ids: vec!["a".into()],
            from: "inbox".into(),
            to: "archive".into(),
        },
    ))
    .unwrap();

    // 服务端还没收到移动，仍报 inbox
    provider.queue_page(page(vec![mail("a", "inbox")]));

    let eng = engine(&provider, &db);
    let (_, report) = eng
        .sync_with_report(ACC, None, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(report.move_wins, 1);
    let stored = db.get_email("a").unwrap().unwrap();
    assert_eq!(
        stored.mailbox_ids,
        vec!["archive".to_string()],
        "未上传的移动不能被服务端覆盖"
    );
}

#[tokio::test]
async fn pending_delete_beats_pending_move() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    let mut local = mail("a", "inbox");
    local.mailbox_ids = vec!["archive".into()];
    db.upsert_email(&local).unwrap();

    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Move {
            email_ids: vec!["a".into()],
            from: "inbox".into(),
            to: "archive".into(),
        },
    ))
    .unwrap();
    db.enqueue(&OfflineOperation::new(
        "op2",
        ACC,
        OperationKind::Delete {
            email_ids: vec!["a".into()],
            from: String::new(),
        },
    ))
    .unwrap();

    provider.queue_page(page(vec![mail("a", "inbox")]));

    let eng = engine(&provider, &db);
    let (_, report) = eng
        .sync_with_report(ACC, None, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(report.delete_wins, 1);
    let stored = db.get_email("a").unwrap().unwrap();
    assert!(stored.flags.contains(&Flag::Deleted));
    assert_eq!(
        stored.mailbox_ids,
        vec!["inbox".to_string()],
        "删除优先级高于移动：已删除邮件不再被移动"
    );
}

#[tokio::test]
async fn pending_delete_blocks_server_deletion() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    db.upsert_email(&mail("a", "inbox")).unwrap();
    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Delete {
            email_ids: vec!["a".into()],
            from: String::new(),
        },
    ))
    .unwrap();

    provider.queue_page(SyncPage {
        deleted_ids: vec!["a".into()],
        new_state: Some("s2".into()),
        ..SyncPage::default()
    });

    let eng = engine(&provider, &db);
    let outcome = eng
        .sync(ACC, None, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(outcome.stats.deleted, 0);
    assert!(
        db.get_email("a").unwrap().is_some(),
        "有未上传操作时不得删除本地行"
    );
}

#[tokio::test]
async fn full_resync_calibrates_stale_rows() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    for id in ["a", "b", "c"] {
        db.upsert_email(&mail(id, "inbox")).unwrap();
    }

    provider.queue_page(SyncPage {
        emails: vec![mail("a", "inbox")],
        new_state: Some("s9".into()),
        full_resync: true,
        ..SyncPage::default()
    });

    let eng = engine(&provider, &db);
    let outcome = eng
        .sync(ACC, Some("inbox".into()), CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(outcome.stats.calibrated, 2);
    assert_eq!(
        db.list_emails_in_mailbox(ACC, "inbox", 100).unwrap().len(),
        1
    );
}

#[tokio::test]
async fn cursor_advances_and_is_persisted() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    provider.queue_page(page(vec![mail("a", "inbox")]));

    let eng = engine(&provider, &db);
    let outcome = eng
        .sync(ACC, Some("inbox".into()), CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(outcome.new_state.as_deref(), Some("s1"));
    let cursor = db.get_sync_cursor(ACC, "inbox").unwrap();
    assert_eq!(cursor.state.as_deref(), Some("s1"));
}

#[tokio::test]
async fn sync_failure_does_not_advance_cursor() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    provider.fail_next(MockFailure::Network);

    let eng = engine(&provider, &db);
    let err = eng
        .sync(ACC, Some("inbox".into()), CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), crossmail_core::ErrorKind::Network);
    assert_eq!(db.get_sync_cursor(ACC, "inbox").unwrap().state, None);
}

// ---------------------------------------------------------------- 离线回放

fn replayer(provider: &Arc<MockProvider>, db: &TestDb) -> OfflineReplayer<MockProvider> {
    OfflineReplayer::new(Arc::clone(provider), db.shared())
}

#[tokio::test]
async fn move_rewrites_identity_for_later_queued_ops() {
    let provider = Arc::new(MockProvider::new().with_remap_on_move());
    let db = fresh_db();
    db.upsert_email(&mail("inbox/5", "inbox")).unwrap();

    // 队列：先移动，再标星——第二条用的是**移动前**的主键
    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Move {
            email_ids: vec!["inbox/5".into()],
            from: "inbox".into(),
            to: "archive".into(),
        },
    ))
    .unwrap();
    db.enqueue(&OfflineOperation::new(
        "op2",
        ACC,
        OperationKind::SetFlag {
            email_id: "inbox/5".into(),
            flag: Flag::Flagged,
            enabled: true,
        },
    ))
    .unwrap();

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.applied, 2, "两条都应回放成功: {report:?}");
    assert!(report.is_drained());
    assert_eq!(
        report.remaps,
        vec![crossmail_core::provider::IdRemap::new(
            "inbox/5",
            "archive/1005"
        )],
        "身份变更必须上报给调用方，否则 UI 手里的旧 id 立即失效却无从得知"
    );

    // 关键断言：第二条必须打向服务器分配的新身份，而不是已经不存在的旧 UID。
    // 打旧 UID 时服务器会静默回 OK——这正是「报告全绿、邮件没变」的成因。
    let calls = provider.calls();
    let flag_call = calls
        .iter()
        .find(|c| c.starts_with("set_flags"))
        .unwrap_or_else(|| panic!("没有发出标星调用: {calls:?}"));
    assert!(
        flag_call.contains("archive/1005"),
        "标星应打向移动后的新主键，实际: {flag_call}"
    );

    // 本地记录跟着换主键，旧记录消失
    assert!(
        db.get_email("inbox/5").unwrap().is_none(),
        "旧主键的记录应已迁移"
    );
    let moved = db
        .get_email("archive/1005")
        .unwrap()
        .expect("新主键应有记录");
    assert_eq!(moved.mailbox_ids, vec!["archive".to_string()]);
}

#[tokio::test]
async fn offline_ops_replay_in_fifo_order() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    db.upsert_email(&mail("a", "inbox")).unwrap();

    for (idx, flag) in [Flag::Seen, Flag::Flagged, Flag::Answered]
        .iter()
        .enumerate()
    {
        db.enqueue(&OfflineOperation::new(
            format!("op{idx}"),
            ACC,
            OperationKind::SetFlag {
                email_id: "a".into(),
                flag: flag.clone(),
                enabled: true,
            },
        ))
        .unwrap();
    }

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.applied, 3);
    assert!(report.is_drained());
    assert!(db.pending_operations(ACC).unwrap().is_empty());

    let calls = provider.calls();
    assert!(calls[0].contains("Seen"));
    assert!(calls[1].contains("Flagged"));
    assert!(calls[2].contains("Answered"));
}

#[tokio::test]
async fn network_error_defers_and_stops_replay() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    for idx in 0..3 {
        db.enqueue(&OfflineOperation::new(
            format!("op{idx}"),
            ACC,
            OperationKind::Delete {
                email_ids: vec![format!("e{idx}")],
                from: String::new(),
            },
        ))
        .unwrap();
    }
    provider.fail_next(MockFailure::Network);

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.deferred, 1);
    assert!(!report.is_drained());
    let pending = db.pending_operations(ACC).unwrap();
    assert_eq!(pending.len(), 3, "网络错误不得丢操作");
    assert_eq!(pending[0].attempts, 1);
    assert_eq!(
        provider.call_count("delete:"),
        1,
        "网络错误后必须中止本轮，不能越序继续"
    );
}

#[tokio::test]
async fn protocol_error_terminates_replay() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Delete {
            email_ids: vec!["e1".into()],
            from: String::new(),
        },
    ))
    .unwrap();
    db.enqueue(&OfflineOperation::new(
        "op2",
        ACC,
        OperationKind::Delete {
            email_ids: vec!["e2".into()],
            from: String::new(),
        },
    ))
    .unwrap();
    provider.fail_next(MockFailure::Protocol);

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.failed, 1);
    assert!(!report.errors[0].retryable);
    assert_eq!(db.pending_operations(ACC).unwrap().len(), 2);
}

#[tokio::test]
async fn dead_letter_is_skipped_after_max_attempts() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    let mut op = OfflineOperation::new(
        "dead",
        ACC,
        OperationKind::Delete {
            email_ids: vec!["e1".into()],
            from: String::new(),
        },
    );
    op.attempts = 5;
    db.enqueue(&op).unwrap();
    db.enqueue(&OfflineOperation::new(
        "alive",
        ACC,
        OperationKind::Delete {
            email_ids: vec!["e2".into()],
            from: String::new(),
        },
    ))
    .unwrap();

    let replayer = OfflineReplayer::with_config(
        Arc::clone(&provider),
        db.shared(),
        ReplayConfig { max_attempts: 5 },
    );
    let report = replayer.replay(ACC).await.unwrap();
    assert_eq!(report.skipped, 1, "死信不再重试");
    assert_eq!(report.applied, 1, "死信不得阻塞后续操作");
    assert_eq!(db.pending_operations(ACC).unwrap().len(), 1);
}

#[tokio::test]
async fn archive_op_uses_local_archive_folder() {
    let provider = Arc::new(MockProvider::new());
    let db = db_with(&[
        ("inbox", MailboxRole::Inbox),
        ("Archive", MailboxRole::Archive),
    ]);
    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Archive {
            email_ids: vec!["a".into()],
            from: "inbox".into(),
        },
    ))
    .unwrap();

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.applied, 1);
    assert!(provider.calls()[0].contains("inbox->Archive"));
}

#[tokio::test]
async fn archive_without_folder_is_unsupported() {
    let provider = Arc::new(MockProvider::new());
    let db = db_with(&[("inbox", MailboxRole::Inbox)]);
    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Archive {
            email_ids: vec!["a".into()],
            from: "inbox".into(),
        },
    ))
    .unwrap();

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.failed, 1);
    assert!(!report.errors[0].retryable);
}

#[tokio::test]
async fn draft_send_replays_from_cache() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    let mut draft = mail("draft1", "drafts");
    draft.subject = "草稿主题".into();
    draft.text_body = Some("正文".into());
    draft.flags = vec![Flag::Draft];
    db.upsert_email(&draft).unwrap();
    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Send {
            draft_id: "draft1".into(),
        },
    ))
    .unwrap();

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.applied, 1);
    let sent = provider.sent_messages();
    assert_eq!(sent[0].subject, "草稿主题");
    assert_eq!(sent[0].draft_id.as_deref(), Some("draft1"));
}

#[tokio::test]
async fn draft_with_attachments_is_refused_not_silently_lossy() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    let mut draft = mail("draft1", "drafts");
    let mut att = crossmail_core::Attachment::new("att1", "application/pdf");
    att.name = Some("报价单.pdf".into());
    att.size = 1024;
    draft.attachments = vec![att];
    db.upsert_email(&draft).unwrap();
    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Send {
            draft_id: "draft1".into(),
        },
    ))
    .unwrap();

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.failed, 1);
    assert!(report.errors[0].message.contains("附件"));
}

// ---------------------------------------------------------------- 一致性闭环（S3-08）

#[tokio::test]
async fn local_and_remote_converge_after_replay() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();

    // 1) 首次同步：远端两封邮件
    provider.queue_page(page(vec![mail("a", "inbox"), mail("b", "inbox")]));
    let eng = engine(&provider, &db);
    eng.sync(ACC, Some("inbox".into()), CancellationToken::new(), None)
        .await
        .unwrap();

    // 2) 离线标记已读（写队列）
    let rp = replayer(&provider, &db);
    rp.queue_flag(ACC, &"a".to_string(), Flag::Seen, true)
        .unwrap();
    let report = rp.replay(ACC).await.unwrap();
    assert_eq!(report.applied, 1);

    // 3) 下一次同步，远端已带上 $seen
    let mut a = mail("a", "inbox");
    a.flags = vec![Flag::Seen];
    provider.queue_page(page(vec![a, mail("b", "inbox")]));

    let (outcome, conflicts) = eng
        .sync_with_report(ACC, Some("inbox".into()), CancellationToken::new(), None)
        .await
        .unwrap();
    // 被回放过标记的那封会同步过来（updated=1），另一封无变化（unchanged=1）
    assert_eq!(outcome.stats.updated, 1);
    assert_eq!(outcome.stats.unchanged, 1);
    assert_eq!(
        conflicts.local_overrides, 0,
        "无待上传操作时不得再覆盖服务端"
    );
    assert!(db
        .get_email("a")
        .unwrap()
        .unwrap()
        .flags
        .contains(&Flag::Seen));
}

#[tokio::test]
async fn replay_is_idempotent_after_success() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    let rp = replayer(&provider, &db);
    let id: Id = "a".to_string();
    rp.queue_flag(ACC, &id, Flag::Flagged, true).unwrap();

    let first = rp.replay(ACC).await.unwrap();
    let second = rp.replay(ACC).await.unwrap();
    assert_eq!(first.applied, 1);
    assert_eq!(second, Default::default(), "队列已空时回放不该重复执行");
    assert_eq!(provider.call_count("set_flags:"), 1);
}

// ------------------------------------------------- 失败注入（真联调后补齐）

/// 排队 N 条删除操作，返回它们的 id。
fn queue_deletes(db: &TestDb, n: usize) -> Vec<Id> {
    let mut ids = Vec::new();
    for idx in 0..n {
        let id = format!("op{idx}");
        db.enqueue(&OfflineOperation::new(
            id.clone(),
            ACC,
            OperationKind::Delete {
                email_ids: vec![format!("e{idx}")],
                from: String::new(),
            },
        ))
        .unwrap();
        ids.push(id);
    }
    ids
}

#[tokio::test]
async fn partial_success_then_network_error_keeps_remaining_queue() {
    // 真实路径：一批操作跑了一半断网。要验证三件事：
    //   1. 已经成功的绝不重复执行（否则收件箱会被删两遍）
    //   2. 失败那条留在队列里，网络恢复后重试
    //   3. 还没轮到的操作不许丢
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    let rp = replayer(&provider, &db);

    // 第一轮：2 条正常回放完
    for idx in 0..2 {
        db.enqueue(&OfflineOperation::new(
            format!("batch1-{idx}"),
            ACC,
            OperationKind::Delete {
                email_ids: vec![format!("e{idx}")],
                from: String::new(),
            },
        ))
        .unwrap();
    }
    let first = rp.replay(ACC).await.unwrap();
    assert_eq!(first.applied, 2);
    assert_eq!(provider.call_count("delete:"), 2);

    // 第二轮：再排 2 条，第一条就撞网络错
    for idx in 0..2 {
        db.enqueue(&OfflineOperation::new(
            format!("batch2-{idx}"),
            ACC,
            OperationKind::Delete {
                email_ids: vec![format!("f{idx}")],
                from: String::new(),
            },
        ))
        .unwrap();
    }
    provider.fail_next(MockFailure::Network);
    let second = rp.replay(ACC).await.unwrap();
    assert_eq!(second.applied, 0);
    assert_eq!(second.deferred, 1);
    assert_eq!(
        provider.call_count("delete:"),
        3,
        "网络错之后必须中止本轮，未轮到的操作不许执行"
    );
    assert_eq!(
        db.pending_operations(ACC).unwrap().len(),
        2,
        "两条都必须留着"
    );

    // 第三轮：网络恢复，剩余 2 条补发；第一批的 2 条绝不重复执行
    let third = rp.replay(ACC).await.unwrap();
    assert_eq!(third.applied, 2);
    assert!(third.is_drained());
    assert!(db.pending_operations(ACC).unwrap().is_empty());
    assert_eq!(
        provider.call_count("delete:"),
        5,
        "累计应为 2(首批) + 1(失败) + 2(补发)，重复执行会把数字顶高"
    );
}

#[tokio::test]
async fn authentication_failure_is_flagged_for_reauth_not_retried() {
    // 凭证过期：不可自动重试，但队列必须保留 —— 否则用户的待发邮件凭空消失
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    queue_deletes(&db, 2);
    provider.fail_next(MockFailure::Authentication);

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(report.deferred, 0);
    assert!(
        report.needs_reauth(),
        "认证失败必须能被 UI 识别为需要重新登录"
    );
    let err = report.last_error().expect("应有错误记录");
    assert_eq!(err.kind, crossmail_core::ErrorKind::Authentication);
    assert!(!err.retryable, "认证失败不能自动重试，否则会无限循环");
    assert!(err.is_reauth_required());

    let pending = db.pending_operations(ACC).unwrap();
    assert_eq!(pending.len(), 2, "认证失败不得丢队列");
    assert_eq!(
        provider.call_count("delete:"),
        1,
        "认证失败后本轮必须立即中止"
    );
}

#[tokio::test]
async fn server_5xx_is_retryable_but_protocol_error_is_not() {
    // 5xx 属临时故障（可重试），与 4xx/协议错误必须区分开：
    // 判错方向会让用户要么白白等下去，要么把能成功的操作永久判死
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    queue_deletes(&db, 1);
    provider.fail_next(MockFailure::ServerError);
    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.deferred, 1);
    let err = report.last_error().unwrap();
    assert!(err.retryable);
    assert_eq!(err.kind, crossmail_core::ErrorKind::Server);
    assert!(!report.needs_reauth());

    // 换成协议错误：同样不可重试，但要能与「需要重新登录」区分
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    queue_deletes(&db, 1);
    provider.fail_next(MockFailure::Protocol);
    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.failed, 1);
    let err = report.last_error().unwrap();
    assert!(!err.retryable);
    assert_eq!(err.kind, crossmail_core::ErrorKind::Protocol);
    assert!(
        !report.needs_reauth(),
        "协议错误不是重新登录能解决的，不能误导用户去改密码"
    );
}

#[tokio::test]
async fn timeout_defers_like_network_error() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    queue_deletes(&db, 2);
    provider.fail_next(MockFailure::Timeout);
    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.deferred, 1, "超时按网络类处理，只推迟不判死");
    assert_eq!(
        report.last_error().unwrap().kind,
        crossmail_core::ErrorKind::Network
    );
    assert_eq!(db.pending_operations(ACC).unwrap().len(), 2);
}

#[tokio::test]
async fn not_found_terminates_without_retry() {
    // 邮件已被别处删除：重试一万次也不会变，必须终止
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    queue_deletes(&db, 2);
    provider.fail_next(MockFailure::NotFound);
    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(
        report.last_error().unwrap().kind,
        crossmail_core::ErrorKind::NotFound
    );
    assert_eq!(provider.call_count("delete:"), 1);
    assert_eq!(db.pending_operations(ACC).unwrap().len(), 2);
}

#[tokio::test]
async fn dead_letter_does_not_block_later_operations() {
    // 死信必须「跳过」而不是「堵住」：否则一条永久失败的操作会把整个账号
    // 的离线队列钉死，用户之后所有操作都发不出去
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    let ids = queue_deletes(&db, 3);
    {
        // 把第一条打到尝试上限，模拟「反复失败后放弃」
        let guard = db.shared();
        let d = guard.lock().unwrap();
        for _ in 0..3 {
            d.record_operation_error(&ids[0], "反复失败").unwrap();
        }
    }
    let rp = OfflineReplayer::with_config(
        Arc::clone(&provider),
        db.shared(),
        ReplayConfig { max_attempts: 3 },
    );
    let report = rp.replay(ACC).await.unwrap();
    assert_eq!(report.skipped, 1, "达上限的应计入死信");
    assert_eq!(report.applied, 2, "死信之后的正常操作必须继续执行");
    assert!(
        !db.pending_operations(ACC).unwrap().is_empty(),
        "死信本身留在队列里等人工处置，不能悄悄删除"
    );
}

#[tokio::test]
async fn queued_operations_survive_reconnect_without_duplicates() {
    // 断链 → 重连 → 继续：这是移动端最高频的真实路径。
    // 用一个全新的 provider 实例模拟重连，验证队列只被消费一次。
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    let rp = replayer(&provider, &db);
    let id: Id = "a".to_string();
    rp.queue_flag(ACC, &id, Flag::Seen, true).unwrap();
    provider.fail_next(MockFailure::Network);
    let first = rp.replay(ACC).await.unwrap();
    assert_eq!(first.deferred, 1);

    // 重连后换一个干净 provider：调用记录归零，队列里仍应只有这一条
    let provider2 = Arc::new(MockProvider::new());
    let rp2 = replayer(&provider2, &db);
    let second = rp2.replay(ACC).await.unwrap();
    assert_eq!(second.applied, 1, "重连后应把积压操作补发出去");
    assert_eq!(provider2.call_count("set_flags:"), 1);
    assert!(db.pending_operations(ACC).unwrap().is_empty());
}

// -------------------------------------- 陈旧主键重定向（S6-15）

/// 回放 `Delete`/`Copy` 时，进 provider 的主键必须指向 `from` 记录的**当前**位置。
///
/// `email_ids` 里的文件夹是上次同步的快照。邮件被移动过（或别的客户端搬走了）之后，
/// 快照就过期了，于是 `SELECT` 的是一个这些 UID 根本不存在的文件夹。
/// 真机对这种写操作回 `OK`——不是报错，是**静默空转**：报告全绿、服务器上什么都没发生。
#[tokio::test]
async fn replay_retargets_stale_ids_to_recorded_folder() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Delete {
            // 主键说它在 INBOX，但实际已经被移走了
            email_ids: vec![format!("{ACC}/INBOX/42")],
            from: "Trash".into(),
        },
    ))
    .unwrap();
    db.enqueue(&OfflineOperation::new(
        "op2",
        ACC,
        OperationKind::Copy {
            email_ids: vec![format!("{ACC}/INBOX/43")],
            from: "Trash".into(),
            to: "Archive".into(),
        },
    ))
    .unwrap();

    let report = replayer(&provider, &db).replay(ACC).await.unwrap();
    assert_eq!(report.applied, 2, "{report:?}");

    let calls = provider.calls();
    assert!(
        calls.contains(&format!("delete:{ACC}/Trash/42")),
        "删除必须打到 from 指定的文件夹: {calls:?}"
    );
    assert!(
        calls.contains(&format!("copy:{ACC}/Trash/43->Archive")),
        "复制必须打到 from 指定的文件夹: {calls:?}"
    );
}

/// 老队列里没有 `from`（`#[serde(default)]` 反序列化成空串）时保持原行为，
/// 不能因为缺这个字段就把主键改坏。
#[tokio::test]
async fn replay_keeps_id_when_from_is_absent() {
    let provider = Arc::new(MockProvider::new());
    let db = fresh_db();
    db.enqueue(&OfflineOperation::new(
        "op1",
        ACC,
        OperationKind::Delete {
            email_ids: vec![format!("{ACC}/INBOX/7")],
            from: String::new(),
        },
    ))
    .unwrap();

    replayer(&provider, &db).replay(ACC).await.unwrap();
    assert!(
        provider.calls().contains(&format!("delete:{ACC}/INBOX/7")),
        "{:?}",
        provider.calls()
    );
}
