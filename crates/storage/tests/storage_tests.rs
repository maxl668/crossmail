use crossmail_core::*;
use crossmail_storage::{Database, EmailQuery, SearchFields, SearchQuery};

fn setup() -> Database {
    let mut db = Database::open_in_memory().unwrap();
    db.migrate().unwrap();
    db
}

fn seed_account(db: &Database, id: &str) -> Account {
    let a = Account::new(id, "me@example.com", ProtocolKind::Jmap);
    db.upsert_account(&a).unwrap();
    a
}

fn seed_mailbox(db: &Database, account_id: &str, id: &str) -> Mailbox {
    let mut m = Mailbox::new(id, account_id, "INBOX");
    m.role = MailboxRole::Inbox;
    db.upsert_mailbox(&m).unwrap();
    m
}

#[test]
fn migration_is_versioned_and_idempotent() {
    let mut db = Database::open_in_memory().unwrap();
    let first = db.migrate().unwrap();
    assert_eq!(first.from_version, 0);
    assert_eq!(first.to_version, 6);
    assert_eq!(first.applied.len(), 6);
    assert_eq!(db.schema_version().unwrap(), 6);

    // 重复迁移不重复执行，且不丢数据
    seed_account(&db, "acc1");
    let second = db.migrate().unwrap();
    assert_eq!(second.from_version, 6);
    assert!(second.applied.is_empty());
    assert!(db.get_account("acc1").unwrap().is_some());
}

/// v6 的核心目的：UID 只在**文件夹内**唯一，跨文件夹重号是完全正常的，
/// 库层面不能对 `(account_id, uid)` 加唯一约束。
///
/// 真实触发场景：发信后把副本 APPEND 进「已发送」，Sent 的第一封 uid=1，
/// 而收件箱里的种子邮件也是 uid=1 —— 老约束下整次发送会报「本地存储错误」。
#[test]
fn migration_v6_allows_same_uid_in_different_mailboxes() {
    let db = setup();
    seed_account(&db, "acc1");
    seed_mailbox(&db, "acc1", "INBOX");
    seed_mailbox(&db, "acc1", "Sent");

    let mut inbox = Email::new("acc1/INBOX/1", "acc1", "INBOX");
    inbox.uid = Some(1);
    db.upsert_email(&inbox).unwrap();

    let mut sent = Email::new("acc1/Sent/1", "acc1", "Sent");
    sent.uid = Some(1);
    db.upsert_email(&sent)
        .unwrap_or_else(|e| panic!("不同文件夹同 UID 必须允许写入，实测失败：{e}"));

    let ids: Vec<_> = db
        .list_emails("acc1", 50)
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    assert_eq!(ids.len(), 2, "{ids:?}");
}

/// 重建表期间会临时关外键，迁移结束后必须恢复——否则后续写入不再受约束，
/// 一旦代码路径出错就是静默的数据错乱，比直接报错难查得多。
#[test]
fn migration_restores_foreign_key_enforcement() {
    let db = setup();
    let on: i64 = db
        .connection()
        .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(on, 1, "迁移结束后 foreign_keys 必须回到 ON");
}

#[test]
fn account_crud_and_cascade_delete() {
    let db = setup();
    let a = seed_account(&db, "acc1");
    let got = db.get_account("acc1").unwrap().expect("account exists");
    assert_eq!(got.email_address, a.email_address);
    assert_eq!(got.protocol, ProtocolKind::Jmap);
    assert_eq!(got.sync_days, 30);
    assert_eq!(db.list_accounts().unwrap(), vec!["acc1".to_string()]);

    seed_mailbox(&db, "acc1", "mb1");
    let mut e = Email::new("e1", "acc1", "mb1");
    e.subject = "hello".into();
    db.upsert_email(&e).unwrap();

    assert_eq!(db.delete_account("acc1").unwrap(), 1);
    assert!(db.get_account("acc1").unwrap().is_none());
    assert!(db.list_mailboxes("acc1").unwrap().is_empty());
    assert!(db.get_email("e1").unwrap().is_none());
}

#[test]
fn email_upsert_is_idempotent_and_normalizes_flags() {
    let db = setup();
    seed_account(&db, "acc1");
    seed_mailbox(&db, "acc1", "mb1");

    let mut e = Email::new("e1", "acc1", "mb1");
    e.subject = "周报".into();
    e.from.push(Address::with_name("张三", "zhang@example.com"));
    e.set_flag(Flag::Seen, true);
    e.set_flag(Flag::Seen, true); // 重复写入
    e.set_flag(Flag::Flagged, true);
    e.attachments
        .push(Attachment::new("at1", "application/pdf"));
    e.uid = Some(42);
    db.upsert_email(&e).unwrap();
    db.upsert_email(&e).unwrap(); // 幂等

    assert_eq!(db.list_emails("acc1", 10).unwrap().len(), 1);
    let got = db.get_email("e1").unwrap().unwrap();
    assert_eq!(got.subject, "周报");
    assert_eq!(got.from, e.from);
    assert_eq!(got.flags.len(), 2);
    assert!(got.flags.contains(&Flag::Seen));
    assert_eq!(got.mailbox_ids, vec!["mb1".to_string()]);
    assert_eq!(got.attachments.len(), 1);
    assert_eq!(got.uid, Some(42));
}

#[test]
fn basic_search_is_parameterized_and_escaped() {
    let db = setup();
    seed_account(&db, "acc1");
    seed_mailbox(&db, "acc1", "mb1");
    for (id, subject) in [("e1", "100%_季度报告"), ("e2", "普通周报")] {
        let mut e = Email::new(id, "acc1", "mb1");
        e.subject = subject.into();
        db.upsert_email(&e).unwrap();
    }
    assert_eq!(db.search_emails("acc1", "100%").unwrap().len(), 1);
    assert_eq!(db.search_emails("acc1", "周报").unwrap().len(), 1);
    // '%' 被转义为字面量，不会退化成全表匹配
    assert!(db
        .search_emails("acc1", "%")
        .unwrap()
        .iter()
        .all(|e| e.id == "e1"));
    assert!(db
        .search_emails("acc1", "'; DROP TABLE emails; --")
        .unwrap()
        .is_empty());
}

#[test]
fn offline_queue_is_fifo_and_error_is_recorded() {
    let db = setup();
    seed_account(&db, "acc1");
    let ops = vec![
        OfflineOperation::new(
            "op1",
            "acc1",
            OperationKind::SetFlag {
                email_id: "e1".into(),
                flag: Flag::Seen,
                enabled: true,
            },
        ),
        OfflineOperation::new(
            "op2",
            "acc1",
            OperationKind::Delete {
                email_ids: vec!["e2".into()],
                from: String::new(),
            },
        ),
    ];
    for op in &ops {
        db.enqueue(op).unwrap();
    }

    let pending = db.pending_operations("acc1").unwrap();
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0].id, "op1");
    assert_eq!(pending[1].id, "op2");
    assert_eq!(conflict_priority(&pending[1].kind), 30);

    db.record_operation_error("op1", "network down").unwrap();
    let pending = db.pending_operations("acc1").unwrap();
    assert_eq!(pending[0].attempts, 1);
    assert_eq!(pending[0].last_error.as_deref(), Some("network down"));

    db.mark_operation_done("op1").unwrap();
    assert_eq!(db.pending_operations("acc1").unwrap().len(), 1);
}

#[test]
fn sync_cursor_roundtrip() {
    let db = setup();
    seed_account(&db, "acc1");
    assert_eq!(db.get_sync_cursor("acc1", "mb1").unwrap().state, None);
    let cursor = crossmail_storage::SyncCursor {
        state: Some("state-abc".into()),
        modseq: Some(9876),
    };
    db.set_sync_cursor("acc1", "mb1", &cursor).unwrap();
    assert_eq!(db.get_sync_cursor("acc1", "mb1").unwrap(), cursor);
}

#[test]
fn cache_lru_eviction_respects_quota() {
    let db = setup();
    db.record_cache_file("old", 400).unwrap();
    db.record_cache_file("new", 400).unwrap();
    assert_eq!(db.cache_usage().unwrap(), 800);
    let removed = db.evict_lru(500).unwrap();
    assert_eq!(removed.len(), 1);
    assert!(db.cache_usage().unwrap() <= 500);
    assert!(db.evict_lru(500).unwrap().is_empty());
}

// ---------------------------------------------------------------- S4-01 / S4-02

/// S4-01：Email 模型里的收件人/抄送/回复地址/Message-ID/线程等字段必须能完整往返，
/// 此前只落 subject/from/to，读回来的 cc/thread 永远是空，是真实的数据丢失缺陷。
#[test]
fn full_metadata_roundtrips() {
    let db = setup();
    seed_account(&db, "acc1");
    seed_mailbox(&db, "acc1", "mb1");
    let mut e = Email::new("e1", "acc1", "mb1");
    e.subject = "季度复盘".into();
    e.from = vec![Address::new("boss@example.com")];
    e.to = vec![Address::new("me@example.com")];
    e.cc = vec![Address::new("cc@example.com")];
    e.bcc = vec![Address::new("secret@example.com")];
    e.reply_to = vec![Address::new("noreply@example.com")];
    e.message_id = Some("<abc@example.com>".into());
    e.thread_id = Some("thread-1".into());
    e.sent_at = Some(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap());
    db.upsert_email(&e).unwrap();

    let got = db.get_email("e1").unwrap().unwrap();
    assert_eq!(got.cc, e.cc);
    assert_eq!(got.bcc, e.bcc);
    assert_eq!(got.reply_to, e.reply_to);
    assert_eq!(got.message_id, e.message_id);
    assert_eq!(got.thread_id, e.thread_id);
    assert_eq!(got.sent_at, e.sent_at);
}

/// S4-02：分页 offset/limit、total、next_offset 与实际数据一致。
#[test]
fn pagination_and_counts_are_consistent() {
    let db = setup();
    seed_account(&db, "acc1");
    seed_mailbox(&db, "acc1", "mb1");
    seed_mailbox(&db, "acc1", "mb2");
    for i in 0..25u32 {
        let mb = if i % 5 == 0 { "mb2" } else { "mb1" };
        let mut e = Email::new(format!("e{i:02}"), "acc1", mb);
        e.received_at = chrono::DateTime::from_timestamp(1_700_000_000 + i as i64, 0).unwrap();
        if i % 3 == 0 {
            e.flags.push(Flag::Seen);
        }
        db.upsert_email(&e).unwrap();
    }

    let p1 = db
        .page_emails("acc1", &EmailQuery::new().with_limit(10))
        .unwrap();
    assert_eq!(p1.total, 25);
    assert_eq!(p1.items.len(), 10);
    assert_eq!(p1.next_offset, Some(10));
    // 按接收时间倒序：第一条应是最大的 received_at
    assert_eq!(p1.items[0].id, "e24");

    let p3 = db
        .page_emails("acc1", &EmailQuery::new().with_limit(10).with_offset(20))
        .unwrap();
    assert_eq!(p3.items.len(), 5);
    assert_eq!(p3.next_offset, None, "末页不应再给下一页偏移");

    let in_mb2 = db
        .page_emails("acc1", &EmailQuery::new().in_mailbox("mb2"))
        .unwrap();
    assert_eq!(in_mb2.total, 5);

    let unread = db
        .page_emails("acc1", &EmailQuery::new().in_mailbox("mb1").unread_only())
        .unwrap();
    // mb1 共 20 封，其中 i%3==0 的 7 封已读 → 未读 13；mb2 共 5 封，已读 2 → 未读 3
    assert_eq!(unread.total, 13);

    let counts = db.unread_counts("acc1").unwrap();
    assert_eq!(counts.get("mb1"), Some(&13));
    assert_eq!(counts.get("mb2"), Some(&3));
    assert_eq!(db.count_emails("acc1").unwrap(), 25);
}

/// S4-02 验收：万级数据查询性能达标，且确实走了索引而不是全表扫。
#[test]
fn ten_thousand_rows_query_uses_index_and_stays_fast() {
    let db = setup();
    seed_account(&db, "acc1");
    seed_mailbox(&db, "acc1", "mb1");
    let started = std::time::Instant::now();
    for i in 0..10_000u32 {
        let mut e = Email::new(format!("e{i:05}"), "acc1", "mb1");
        e.subject = format!("subject {i}");
        e.received_at = chrono::DateTime::from_timestamp(1_700_000_000 + i as i64, 0).unwrap();
        e.flags.push(Flag::Seen);
        db.upsert_email(&e).unwrap();
    }
    let insert_ms = started.elapsed().as_millis();

    let started = std::time::Instant::now();
    let page = db
        .page_emails("acc1", &EmailQuery::new().in_mailbox("mb1").with_limit(50))
        .unwrap();
    let page_ms = started.elapsed().as_millis();
    assert_eq!(page.total, 10_000);
    assert_eq!(page.items.len(), 50);
    // 深翻页也不能退化成全表扫描式卡顿
    let started = std::time::Instant::now();
    let deep = db
        .page_emails(
            "acc1",
            &EmailQuery::new()
                .in_mailbox("mb1")
                .with_limit(50)
                .with_offset(9_900),
        )
        .unwrap();
    let deep_ms = started.elapsed().as_millis();
    assert_eq!(deep.items.len(), 50);

    let plan = db
        .explain(
            "SELECT e.id FROM emails e \
             WHERE e.account_id='acc1' AND EXISTS (SELECT 1 FROM email_mailboxes m \
             WHERE m.email_id=e.id AND m.mailbox_id='mb1') \
             ORDER BY e.received_at DESC LIMIT 50",
        )
        .unwrap()
        .join(" | ");
    assert!(
        plan.contains("idx_emails_account_received"),
        "列表分页应命中账号+时间索引：{plan}"
    );
    assert!(
        !plan.contains("SCAN e") && !plan.contains("SCAN m"),
        "分页查询不得退化为全表扫描：{plan}"
    );
    assert!(
        plan.contains("email_mailboxes"),
        "文件夹过滤应走 email_mailboxes 索引：{plan}"
    );

    // 阈值放得很宽，只拦“明显退化”（本地实测通常是个位数毫秒）
    assert!(insert_ms < 60_000, "万级写入异常慢：{insert_ms}ms");
    assert!(page_ms < 1_000, "首页查询异常慢：{page_ms}ms");
    assert!(deep_ms < 1_000, "深翻页异常慢：{deep_ms}ms");
}

/// S4-06：分字段搜索互不串味，且支持分页。
#[test]
fn search_respects_field_selection() {
    let db = setup();
    seed_account(&db, "acc1");
    seed_mailbox(&db, "acc1", "mb1");
    let mut a = Email::new("e1", "acc1", "mb1");
    a.subject = "月度报告".into();
    a.from = vec![Address::new("finance@example.com")];
    a.to = vec![Address::new("me@example.com")];
    a.received_at = chrono::DateTime::from_timestamp(1_700_000_100, 0).unwrap();
    db.upsert_email(&a).unwrap();

    let mut b = Email::new("e2", "acc1", "mb1");
    b.subject = "闲聊".into();
    b.from = vec![Address::new("friend@example.com")];
    b.to = vec![Address::new("me@example.com")];
    b.received_at = chrono::DateTime::from_timestamp(1_700_000_200, 0).unwrap();
    db.upsert_email(&b).unwrap();

    let mut c = Email::new("e3", "acc1", "mb1");
    c.subject = "会议邀请".into();
    c.from = vec![Address::new("hr@example.com")];
    c.cc = vec![Address::new("finance@example.com")];
    db.upsert_email(&c).unwrap();

    let by_subject = db
        .search_page(
            "acc1",
            &SearchQuery::new("报告").with_fields(SearchFields::subject_only()),
        )
        .unwrap();
    assert_eq!(by_subject.total, 1);
    assert_eq!(by_subject.items[0].id, "e1");

    let by_from = db
        .search_page(
            "acc1",
            &SearchQuery::new("finance").with_fields(SearchFields::from_only()),
        )
        .unwrap();
    assert_eq!(by_from.total, 1, "只按发件人搜，抄送里的 finance 不算");
    assert_eq!(by_from.items[0].id, "e1");

    let by_to = db
        .search_page(
            "acc1",
            &SearchQuery::new("finance").with_fields(SearchFields::to_only()),
        )
        .unwrap();
    assert_eq!(by_to.total, 1, "收件人搜索应覆盖抄送");
    assert_eq!(by_to.items[0].id, "e3");

    let all = db
        .search_page("acc1", &SearchQuery::new("example.com"))
        .unwrap();
    assert_eq!(all.total, 3, "全字段搜索应命中全部三封");

    let paged = db
        .search_page("acc1", &SearchQuery::new("example.com").with_limit(2))
        .unwrap();
    assert_eq!(paged.items.len(), 2);
    assert_eq!(paged.next_offset, Some(2));
}

/// v5 → v6 的真实升级路径：重建 `emails` 必须**一行不丢**，且子表
/// （`email_mailboxes` / `email_flags` / `attachments`）不能被级联清空。
///
/// 这是本次迁移最大的风险点：SQLite 开着外键 `DROP TABLE emails` 会顺着
/// `ON DELETE CASCADE` 把孩子一起删掉。测试特意在迁移前塞了带文件夹、带标星
/// 的邮件，迁移后逐个核对。
#[test]
fn migration_v5_to_v6_keeps_rows_and_children() {
    let path = std::env::temp_dir().join(format!(
        "crossmail-v6-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut db = Database::open(&path).unwrap();
    // 手工停在 v5（模拟老用户），不能用 migrate()——它会直接跑到 v6
    {
        let conn = db.connection();
        for m in crossmail_storage::migrations::all()
            .into_iter()
            .filter(|m| m.version <= 5)
        {
            conn.execute_batch(m.sql).unwrap();
        }
        conn.execute_batch("PRAGMA user_version = 5;").unwrap();
    }
    assert_eq!(db.schema_version().unwrap(), 5);

    seed_account(&db, "acc1");
    seed_mailbox(&db, "acc1", "INBOX");
    let mut e = Email::new("acc1/INBOX/7", "acc1", "INBOX");
    e.uid = Some(7);
    e.subject = "升级前就有的邮件".into();
    e.flags = vec![Flag::Flagged];
    db.upsert_email(&e).unwrap();
    assert_eq!(db.list_emails("acc1", 10).unwrap().len(), 1);

    let report = db.migrate().unwrap();
    assert_eq!(report.from_version, 5);
    assert_eq!(report.to_version, 6);

    // 1) 邮件本身还在
    let kept = db.get_email("acc1/INBOX/7").unwrap().expect("邮件不能丢");
    assert_eq!(kept.subject, "升级前就有的邮件");
    assert_eq!(kept.uid, Some(7));
    // 2) 子表没被级联清空
    assert_eq!(kept.mailbox_ids, vec!["INBOX".to_string()], "email_mailboxes 被清空了");
    assert!(kept.flags.contains(&Flag::Flagged), "email_flags 被清空了");
    // 3) 新约束生效：另一个文件夹里同 UID 现在可以写入
    seed_mailbox(&db, "acc1", "Sent");
    let mut s = Email::new("acc1/Sent/7", "acc1", "Sent");
    s.uid = Some(7);
    db.upsert_email(&s).expect("v6 后跨文件夹同 UID 必须允许");

    let _ = std::fs::remove_file(&path);
}
