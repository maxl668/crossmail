//! 业务层集成测试（S4-07）：账号、凭证、缓存、邮件流程、搜索。

use crossmail_core::model::SecurityMode;
use crossmail_core::provider::OperationKind;
use crossmail_core::{
    Address, Attachment, Email, Flag, MailError, Mailbox, MailboxRole, ProtocolKind,
};
use crossmail_service::*;
use crossmail_storage::{Database, EmailQuery};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const ACC: &str = "acc-1";

struct Harness {
    db: Arc<Mutex<Database>>,
    vault: Arc<SecretVault>,
    accounts: AccountService,
    mail: MailService,
    search: SearchService,
    cache_root: PathBuf,
    cache: Arc<BodyCache>,
}

impl Harness {
    /// 每例一个独立临时目录 + 内存库，互不干扰。
    fn new(quota_bytes: u64) -> Self {
        let mut db = Database::open_in_memory().expect("open db");
        db.migrate().expect("migrate");
        let db = Arc::new(Mutex::new(db));

        let codec = Arc::new(ChaChaSecretCodec::new([7u8; 32]));
        let vault = Arc::new(SecretVault::new(Arc::clone(&db), codec));
        let accounts = AccountService::new(Arc::clone(&db), Arc::clone(&vault));

        let cache_root =
            std::env::temp_dir().join(format!("crossmail-cache-{}", uuid::Uuid::new_v4()));
        let cache = Arc::new(
            BodyCache::new(Arc::clone(&db), CacheConfig::new(&cache_root, quota_bytes))
                .expect("cache init"),
        );
        let mail = MailService::with_cache(Arc::clone(&db), Arc::clone(&cache));
        let search = SearchService::new(Arc::clone(&db));
        Self {
            db,
            vault,
            accounts,
            mail,
            search,
            cache_root,
            cache,
        }
    }

    /// 直接建一个已落库的账号（跳过凭证，测试态用）。
    fn seed_account(&self) {
        let a = crossmail_core::Account::new(ACC, "me@example.com", ProtocolKind::Jmap);
        self.db.lock().unwrap().upsert_account(&a).unwrap();
    }

    fn seed_mailbox(&self, id: &str, role: MailboxRole, name: &str) {
        let mut mb = Mailbox::new(id, ACC, name);
        mb.role = role;
        self.db.lock().unwrap().upsert_mailbox(&mb).unwrap();
    }

    fn seed_email(&self, id: &str, mailbox: &str) -> Email {
        let mut e = Email::new(id, ACC, mailbox);
        e.subject = format!("主题 {id}");
        e.from = vec![Address::new("boss@example.com")];
        e.to = vec![Address::new("me@example.com")];
        e.received_at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        self.db.lock().unwrap().upsert_email(&e).unwrap();
        e
    }

    fn pending_kinds(&self) -> Vec<OperationKind> {
        self.db
            .lock()
            .unwrap()
            .pending_operations(ACC)
            .unwrap()
            .into_iter()
            .map(|op| op.kind)
            .collect()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.cache_root);
    }
}

fn imap_setup() -> ProtocolSetup {
    ProtocolSetup::Imap {
        imap: Endpoint::new("imap.example.com", 993, SecurityMode::ImplicitTls),
        smtp: Some(Endpoint::new(
            "smtp.example.com",
            465,
            SecurityMode::ImplicitTls,
        )),
    }
}

// ---------------------------------------------------------------- S4-04 账号

#[test]
fn account_create_encrypts_secret_and_resolves_it() {
    let h = Harness::new(8 * 1024 * 1024);
    let account = h
        .accounts
        .create(NewAccount::new(
            "  Me@Example.COM ",
            imap_setup(),
            "super-secret-password",
        ))
        .expect("create");

    // 地址被规范化为小写
    assert_eq!(account.email_address, "me@example.com");
    assert_eq!(account.protocol, ProtocolKind::Imap);
    assert!(account.imap.is_some() && account.smtp.is_some());

    // 明文必须能从 vault 取回
    let secret = h.accounts.resolve_secret(&account).unwrap();
    assert_eq!(secret, "super-secret-password");

    // 落盘的必须是密文：既不等于明文，也不包含明文子串
    let alias = h.accounts.credential_alias(&account).unwrap().unwrap();
    let raw = h.vault.raw_stored(&alias).unwrap().unwrap();
    assert!(is_sealed(&raw), "落盘应为带版本前缀的密文: {raw}");
    assert_ne!(raw, "super-secret-password");
    assert!(!raw.contains("super-secret-password"));

    // 账号配置里也不能出现明文
    let stored = h.accounts.get(&account.id).unwrap();
    let cfg = serde_json::json!({
        "imap": stored.imap,
        "smtp": stored.smtp,
    })
    .to_string();
    assert!(!cfg.contains("super-secret-password"), "{cfg}");
}

#[test]
fn account_create_rejects_bad_input_and_duplicates() {
    let h = Harness::new(8 * 1024 * 1024);
    let bad_email = h
        .accounts
        .create(NewAccount::new("not-an-email", imap_setup(), "pw"));
    assert!(matches!(bad_email, Err(MailError::InvalidInput(_))));

    let bad_host = h.accounts.create(NewAccount::new(
        "a@example.com",
        ProtocolSetup::Imap {
            imap: Endpoint::new("  ", 993, SecurityMode::ImplicitTls),
            smtp: None,
        },
        "pw",
    ));
    assert!(matches!(bad_host, Err(MailError::InvalidInput(_))));

    let bad_port = h.accounts.create(NewAccount::new(
        "a@example.com",
        ProtocolSetup::Imap {
            imap: Endpoint::new("imap.example.com", 0, SecurityMode::ImplicitTls),
            smtp: None,
        },
        "pw",
    ));
    assert!(matches!(bad_port, Err(MailError::InvalidInput(_))));

    let bad_jmap = h.accounts.create(NewAccount::new(
        "a@example.com",
        ProtocolSetup::Jmap {
            endpoint: "ftp://x".into(),
        },
        "pw",
    ));
    assert!(matches!(bad_jmap, Err(MailError::InvalidInput(_))));

    let empty_secret = h
        .accounts
        .create(NewAccount::new("a@example.com", imap_setup(), ""));
    assert!(matches!(empty_secret, Err(MailError::InvalidInput(_))));

    let mut too_short_poll = NewAccount::new("a@example.com", imap_setup(), "pw");
    too_short_poll.poll_interval_secs = Some(5);
    assert!(matches!(
        h.accounts.create(too_short_poll),
        Err(MailError::InvalidInput(_))
    ));

    let mut bad_days = NewAccount::new("a@example.com", imap_setup(), "pw");
    bad_days.sync_days = Some(0);
    assert!(matches!(
        h.accounts.create(bad_days),
        Err(MailError::InvalidInput(_))
    ));

    // 同协议同地址第二次创建应冲突
    h.accounts
        .create(NewAccount::new("dup@example.com", imap_setup(), "pw"))
        .unwrap();
    let dup = h
        .accounts
        .create(NewAccount::new("DUP@example.com", imap_setup(), "pw"));
    assert!(matches!(dup, Err(MailError::Conflict(_))), "{dup:?}");

    // 校验失败不应留下孤儿密钥
    assert_eq!(h.vault.aliases().unwrap().len(), 1);
}

#[test]
fn account_update_and_rotate_secret() {
    let h = Harness::new(8 * 1024 * 1024);
    let account = h
        .accounts
        .create(NewAccount::new(
            "me@example.com",
            imap_setup(),
            "old-password",
        ))
        .unwrap();
    let alias = h.accounts.credential_alias(&account).unwrap().unwrap();
    let before = h.vault.raw_stored(&alias).unwrap().unwrap();

    let patched = h
        .accounts
        .update(
            &account.id,
            AccountPatch {
                display_name: Some(Some("Test User".into())),
                sync_days: Some(90),
                poll_interval_secs: Some(600),
                smtp: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(patched.display_name.as_deref(), Some("Test User"));
    assert_eq!(patched.sync_days, 90);
    assert_eq!(patched.poll_interval_secs, 600);
    assert!(patched.smtp.is_none(), "smtp 应被显式移除");

    // 清空显示名
    let cleared = h
        .accounts
        .update(
            &account.id,
            AccountPatch {
                display_name: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(cleared.display_name.is_none());

    // 轮换凭证：密文必须变，旧密码再也解不出来
    h.accounts
        .rotate_secret(&account.id, "new-password")
        .unwrap();
    let after = h.vault.raw_stored(&alias).unwrap().unwrap();
    assert_ne!(before, after, "轮换后密文应变化");
    assert_eq!(h.accounts.resolve_secret(&account).unwrap(), "new-password");

    // 非法 sync_days 不接受
    assert!(matches!(
        h.accounts.update(
            &account.id,
            AccountPatch {
                sync_days: Some(9999),
                ..Default::default()
            }
        ),
        Err(MailError::InvalidInput(_))
    ));
}

#[test]
fn account_delete_cascades_data_and_secrets() {
    let h = Harness::new(8 * 1024 * 1024);
    let account = h
        .accounts
        .create(NewAccount::new("me@example.com", imap_setup(), "pw"))
        .unwrap();
    let alias = h.accounts.credential_alias(&account).unwrap().unwrap();

    let mut mb = Mailbox::new("mb1", &account.id, "INBOX");
    mb.role = MailboxRole::Inbox;
    h.db.lock().unwrap().upsert_mailbox(&mb).unwrap();
    let mut e = Email::new("e1", &account.id, "mb1");
    e.subject = "x".into();
    h.db.lock().unwrap().upsert_email(&e).unwrap();

    h.accounts.delete(&account.id).unwrap();
    assert!(
        h.vault.raw_stored(&alias).unwrap().is_none(),
        "凭证必须清掉"
    );
    assert!(h.accounts.list().unwrap().is_empty());
    assert!(h.db.lock().unwrap().get_email("e1").unwrap().is_none());
    assert!(matches!(
        h.accounts.get(&account.id),
        Err(MailError::NotFound(_))
    ));
    assert!(matches!(
        h.accounts.delete(&account.id),
        Err(MailError::NotFound(_))
    ));
}

#[test]
fn tampered_ciphertext_fails_loudly() {
    let h = Harness::new(8 * 1024 * 1024);
    let account = h
        .accounts
        .create(NewAccount::new("me@example.com", imap_setup(), "pw"))
        .unwrap();
    let alias = h.accounts.credential_alias(&account).unwrap().unwrap();
    let raw = h.vault.raw_stored(&alias).unwrap().unwrap();

    // 翻转最后一个字符，制造密文篡改
    let mut chars: Vec<char> = raw.chars().collect();
    let last = chars.len() - 1;
    chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
    let tampered: String = chars.into_iter().collect();
    h.db.lock()
        .unwrap()
        .put_secret(&alias, &tampered, chrono::Utc::now())
        .unwrap();

    let err = h.accounts.resolve_secret(&account).unwrap_err();
    assert!(
        matches!(err, MailError::Storage(_)),
        "篡改必须解密失败而不是返回垃圾: {err:?}"
    );
}

#[test]
fn codec_rejects_double_seal_and_bad_key() {
    let codec = ChaChaSecretCodec::new([1u8; 32]);
    let sealed = codec.seal("plain").unwrap();
    assert!(codec.seal(&sealed).is_err(), "密文不得再被封一次");
    assert_eq!(codec.open(&sealed).unwrap(), "plain");

    // 换密钥必须解不开
    let other = ChaChaSecretCodec::new([2u8; 32]);
    assert!(other.open(&sealed).is_err());

    // 缺前缀要报错而不是瞎猜
    assert!(codec.open("plain-text").is_err());

    // 导出的主密钥可恢复同一把密钥
    let exported = codec.export_key();
    let restored = ChaChaSecretCodec::from_exported(&exported).unwrap();
    assert_eq!(restored.open(&sealed).unwrap(), "plain");
    assert!(ChaChaSecretCodec::from_exported("short").is_err());
}

// ---------------------------------------------------------------- S4-03 缓存

#[test]
fn cache_roundtrip_touch_and_quota_eviction() {
    let h = Harness::new(300);
    let big = vec![b'x'; 200];

    h.cache
        .store("e1", CacheKind::TextBody, None, &big)
        .unwrap();
    h.cache
        .store("e1", CacheKind::HtmlBody, None, &big)
        .unwrap();
    assert_eq!(h.cache.usage().unwrap(), 400);
    assert!(h.cache.is_over_quota().unwrap());

    // 读回内容一致（这一步同时把 text 的 last_access 刷新，成为“最近使用”）
    let got = h.cache.load("e1", CacheKind::TextBody, None).unwrap();
    assert_eq!(got.as_deref(), Some(big.as_slice()));

    // 配额 300、占用 400：只需淘汰最旧的一条（html）就回到配额内
    let report = h.cache.enforce_quota().unwrap();
    assert_eq!(report.removed.len(), 1, "只淘汰到配额内为止");
    assert!(report.removed[0].ends_with(".html"), "{:?}", report.removed);
    assert!(report.usage_after <= 300, "淘汰后应回到配额内");
    assert_eq!(report.freed_bytes, 200);

    // 被淘汰的文件必须从磁盘消失
    for path in &report.removed {
        assert!(!std::path::Path::new(path).exists(), "文件应已删除: {path}");
    }
    // 最近用过的正文仍在，HTML 已不可读
    assert!(h
        .cache
        .load("e1", CacheKind::HtmlBody, None)
        .unwrap()
        .is_none());
    assert!(h
        .cache
        .load("e1", CacheKind::TextBody, None)
        .unwrap()
        .is_some());
}

#[test]
fn cache_lru_keeps_recently_used_entry() {
    // 配额只够放一条
    let h = Harness::new(120);
    h.cache
        .store("old", CacheKind::TextBody, None, &[b'a'; 100])
        .unwrap();
    // 命中一次 old，把它的 last_access 刷新到最新
    assert!(h
        .cache
        .load("old", CacheKind::TextBody, None)
        .unwrap()
        .is_some());
    std::thread::sleep(std::time::Duration::from_millis(5));
    h.cache
        .store("new", CacheKind::TextBody, None, &[b'b'; 100])
        .unwrap();
    h.cache.enforce_quota().unwrap();

    // 最近用过的 new 必须还在，旧的 old 被淘汰
    assert!(
        h.cache
            .load("new", CacheKind::TextBody, None)
            .unwrap()
            .is_some(),
        "新写入的应保留"
    );
    assert!(
        h.cache
            .load("old", CacheKind::TextBody, None)
            .unwrap()
            .is_none(),
        "久未访问的应被淘汰"
    );
}

#[test]
fn cache_never_deletes_files_outside_root() {
    let h = Harness::new(0);
    // 手工登记一个缓存目录之外的文件（模拟脏账本）
    let outside =
        std::env::temp_dir().join(format!("crossmail-outside-{}.txt", uuid::Uuid::new_v4()));
    std::fs::write(&outside, b"do not delete me").unwrap();
    h.db.lock()
        .unwrap()
        .record_cache_file(&outside.to_string_lossy(), 14)
        .unwrap();

    let report = h.cache.enforce_quota().unwrap();
    assert_eq!(report.foreign_entries, 1, "外部路径只清登记");
    assert_eq!(report.freed_bytes, 0);
    assert!(outside.exists(), "缓存目录外的文件绝不能被删");
    assert!(h
        .db
        .lock()
        .unwrap()
        .cache_file_size(&outside.to_string_lossy())
        .unwrap()
        .is_none());

    std::fs::remove_file(&outside).ok();
}

#[test]
fn cache_missing_file_is_dropped_from_ledger() {
    let h = Harness::new(8 * 1024 * 1024);
    let path = h
        .cache
        .store("e9", CacheKind::Attachment, Some("att0"), b"payload")
        .unwrap();
    std::fs::remove_file(&path).unwrap();

    assert!(h
        .cache
        .load("e9", CacheKind::Attachment, Some("att0"))
        .unwrap()
        .is_none());
    assert_eq!(
        h.db.lock()
            .unwrap()
            .cache_file_size(&path.to_string_lossy())
            .unwrap(),
        None,
        "账本应清掉已不存在的文件"
    );
}

// ---------------------------------------------------------------- S4-05 邮件业务

#[test]
fn mail_list_read_and_flag_flow() {
    let h = Harness::new(8 * 1024 * 1024);
    h.seed_account();
    h.seed_mailbox("inbox", MailboxRole::Inbox, "INBOX");
    for i in 0..3 {
        h.seed_email(&format!("e{i}"), "inbox");
    }

    let page = h
        .mail
        .list(ACC, &EmailQuery::new().in_mailbox("inbox").with_limit(2))
        .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.next_offset, Some(2));

    let counts = h.mail.unread_counts(ACC).unwrap();
    assert_eq!(counts.get("inbox"), Some(&3));

    // 读未读邮件并标记已读 → 未读数下降 + 动作入队
    let email = h.mail.read("e0").unwrap();
    assert!(!email.is_read());
    let updated = h.mail.set_read("e0", true).unwrap();
    assert!(updated.is_read());
    assert_eq!(h.mail.unread_counts(ACC).unwrap().get("inbox"), Some(&2));

    let kinds = h.pending_kinds();
    assert_eq!(kinds.len(), 1);
    assert!(matches!(
        &kinds[0],
        OperationKind::SetFlag { email_id, flag: Flag::Seen, enabled: true } if email_id == "e0"
    ));

    assert!(matches!(h.mail.read("nope"), Err(MailError::NotFound(_))));
}

#[test]
fn mail_move_archive_and_delete_respect_mailboxes() {
    let h = Harness::new(8 * 1024 * 1024);
    h.seed_account();
    h.seed_mailbox("inbox", MailboxRole::Inbox, "INBOX");
    h.seed_mailbox("work", MailboxRole::Other, "Work");
    h.seed_email("e1", "inbox");
    h.seed_email("e2", "inbox");
    h.seed_email("e3", "inbox");

    // 移到不存在的文件夹必须报错，不能静默成功
    assert!(matches!(
        h.mail.move_to("e1", "ghost"),
        Err(MailError::NotFound(_))
    ));
    let moved = h.mail.move_to("e1", "work").unwrap();
    assert_eq!(moved.mailbox_ids, vec!["work".to_string()]);
    assert_eq!(h.pending_kinds().len(), 1);

    // 没有归档文件夹时应明确不支持
    assert!(matches!(
        h.mail.archive("e2"),
        Err(MailError::Unsupported(_))
    ));
    h.seed_mailbox("archive", MailboxRole::Archive, "Archive");
    let archived = h.mail.archive("e2").unwrap();
    assert_eq!(archived.mailbox_ids, vec!["archive".to_string()]);

    // 没有回收站 → 直接删本地
    let deleted = h.mail.delete("e3").unwrap();
    assert!(!deleted.trashed);
    assert!(matches!(h.mail.read("e3"), Err(MailError::NotFound(_))));

    // 有回收站 → 进回收站并可恢复
    h.seed_mailbox("trash", MailboxRole::Trash, "Trash");
    h.seed_email("e4", "inbox");
    let trashed = h.mail.delete("e4").unwrap();
    assert!(trashed.trashed);
    let in_trash = h.mail.read("e4").unwrap();
    assert_eq!(in_trash.mailbox_ids, vec!["trash".to_string()]);
    assert!(in_trash.flags.contains(&Flag::Deleted));
}

#[test]
fn draft_flow_saves_lists_and_prepares_send() {
    let h = Harness::new(8 * 1024 * 1024);
    h.seed_account();

    // 空草稿拒绝保存
    assert!(matches!(
        h.mail.save_draft(ACC, &Draft::new("")),
        Err(MailError::InvalidInput(_))
    ));

    // 非法收件人地址在草稿阶段就要拦
    let mut bad = Draft::new("标题").text("正文");
    bad.to = vec![Address::new("bad-address")];
    assert!(matches!(
        h.mail.save_draft(ACC, &bad),
        Err(MailError::InvalidInput(_))
    ));

    let mut draft = Draft::new("季度复盘")
        .to(vec![Address::new("boss@example.com")])
        .text("正文内容")
        .html("<p>正文内容</p>");
    draft.attachments = vec![crossmail_core::OutgoingAttachment {
        name: "report.txt".into(),
        content_type: "text/plain".into(),
        data: b"attachment-bytes".to_vec(),
    }];
    let saved = h.mail.save_draft(ACC, &draft).unwrap();
    assert!(saved.flags.contains(&Flag::Draft));
    assert!(saved.attachments.len() == 1);

    // 草稿列表可见，且正文已能读到
    let drafts = h.mail.list_drafts(ACC, 20).unwrap();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].id, saved.id);
    assert_eq!(
        h.mail.read_body(&saved.id, false).unwrap().as_deref(),
        Some("正文内容")
    );
    assert_eq!(
        h.mail.read_body(&saved.id, true).unwrap().as_deref(),
        Some("<p>正文内容</p>")
    );

    // 组装待发送消息：附件从缓存读回
    let outgoing = h.mail.prepare_send(&saved.id).unwrap();
    assert_eq!(outgoing.subject, "季度复盘");
    assert_eq!(outgoing.to.len(), 1);
    assert_eq!(outgoing.attachments.len(), 1);
    assert_eq!(outgoing.attachments[0].data, b"attachment-bytes");
    assert_eq!(outgoing.draft_id.as_deref(), Some(saved.id.as_str()));

    // 发送成功后草稿出箱进已发送
    h.seed_mailbox("sent", MailboxRole::Sent, "Sent");
    let sent = h.mail.mark_sent(&saved.id, Some("sent")).unwrap();
    assert!(!sent.flags.contains(&Flag::Draft));
    assert_eq!(sent.mailbox_ids, vec!["sent".to_string()]);
    assert!(h.mail.list_drafts(ACC, 20).unwrap().is_empty());
}

#[test]
fn body_cache_backs_metadata_lookup() {
    let h = Harness::new(8 * 1024 * 1024);
    h.seed_account();
    h.seed_mailbox("inbox", MailboxRole::Inbox, "INBOX");
    h.seed_email("e1", "inbox");

    h.mail
        .save_body("e1", Some("缓存里的正文"), Some("<p>x</p>"))
        .unwrap();
    let email = h.mail.read("e1").unwrap();
    assert_eq!(email.text_body.as_deref(), Some("缓存里的正文"));
    assert_eq!(
        h.mail.read_body("e1", false).unwrap().as_deref(),
        Some("缓存里的正文")
    );

    // 没有缓存的邮件应返回 None 而不是报错（由上层决定回源拉取）
    assert!(h.mail.read_body("e1", true).unwrap().is_some());
    h.seed_email("e2", "inbox");
    assert!(h.mail.read_body("e2", false).unwrap().is_none());
}

// ---------------------------------------------------------------- S4-06 搜索

#[test]
fn search_scopes_and_paging() {
    let h = Harness::new(8 * 1024 * 1024);
    h.seed_account();
    h.seed_mailbox("inbox", MailboxRole::Inbox, "INBOX");

    let mut a = Email::new("e1", ACC, "inbox");
    a.subject = "月度报告".into();
    a.from = vec![Address::new("finance@example.com")];
    a.to = vec![Address::new("me@example.com")];
    h.db.lock().unwrap().upsert_email(&a).unwrap();

    let mut b = Email::new("e2", ACC, "inbox");
    b.subject = "闲聊".into();
    b.from = vec![Address::new("friend@example.com")];
    b.to = vec![Address::new("finance@example.com")];
    h.db.lock().unwrap().upsert_email(&b).unwrap();

    let by_subject = h
        .search
        .search(ACC, "报告", SearchScope::Subject, 10, 0)
        .unwrap();
    assert_eq!(by_subject.total, 1);
    assert_eq!(by_subject.items[0].id, "e1");

    let by_from = h
        .search
        .search(ACC, "finance", SearchScope::From, 10, 0)
        .unwrap();
    assert_eq!(by_from.total, 1, "只按发件人搜");
    assert_eq!(by_from.items[0].id, "e1");

    let by_to = h
        .search
        .search(ACC, "finance", SearchScope::To, 10, 0)
        .unwrap();
    assert_eq!(by_to.total, 1, "只按收件人搜");
    assert_eq!(by_to.items[0].id, "e2");

    let all = h
        .search
        .search(ACC, "example.com", SearchScope::All, 10, 0)
        .unwrap();
    assert_eq!(all.total, 2);

    let paged = h
        .search
        .search(ACC, "example.com", SearchScope::All, 1, 0)
        .unwrap();
    assert_eq!(paged.items.len(), 1);
    assert_eq!(paged.next_offset, Some(1));

    // 空关键词不返回全库
    let empty = h
        .search
        .search(ACC, "   ", SearchScope::All, 10, 0)
        .unwrap();
    assert_eq!(empty.total, 0);
    assert!(empty.items.is_empty());
}

// ---------------------------------------------------------------- 附件缓存 + 草稿清理

#[test]
fn attachment_cache_roundtrip() {
    let h = Harness::new(8 * 1024 * 1024);
    let path = h
        .cache
        .store("e1", CacheKind::Attachment, Some("att0"), b"PDF-BYTES")
        .unwrap();
    assert!(path.exists());
    assert_eq!(h.cache.usage().unwrap(), 9);
    let loaded = h
        .cache
        .load("e1", CacheKind::Attachment, Some("att0"))
        .unwrap();
    assert_eq!(loaded.as_deref(), Some(b"PDF-BYTES".as_ref()));

    // 不同 part 不互相覆盖
    h.cache
        .store("e1", CacheKind::Attachment, Some("att1"), b"OTHER")
        .unwrap();
    assert_eq!(
        h.cache
            .load("e1", CacheKind::Attachment, Some("att1"))
            .unwrap()
            .as_deref(),
        Some(b"OTHER".as_ref())
    );

    // 清空缓存：文件全删、账本归零
    let n = h.cache.clear().unwrap();
    assert_eq!(n, 2);
    assert_eq!(h.cache.usage().unwrap(), 0);
    assert!(!path.exists());
}

#[test]
fn attachment_metadata_does_not_break_mail_model() {
    // 回归护栏：Attachment 结构体参与 upsert 往返，字段不能丢。
    let h = Harness::new(8 * 1024 * 1024);
    h.seed_account();
    h.seed_mailbox("inbox", MailboxRole::Inbox, "INBOX");
    let mut e = h.seed_email("e1", "inbox");
    e.attachments = vec![Attachment {
        id: "a1".into(),
        name: Some("报表.pdf".into()),
        content_type: "application/pdf".into(),
        size: 1234,
        blob_id: Some("blob-1".into()),
        part_id: Some("2".into()),
        inline: false,
        cid: None,
    }];
    h.db.lock().unwrap().upsert_email(&e).unwrap();

    let got = h.mail.read("e1").unwrap();
    assert_eq!(got.attachments.len(), 1);
    assert_eq!(got.attachments[0].name.as_deref(), Some("报表.pdf"));
    assert_eq!(got.attachments[0].part_id.as_deref(), Some("2"));
    assert!(got.has_attachments());
}

/// 发信成功后的记账必须**对齐服务端那份**，不能另造一行。
///
/// 现状是 SMTP 发出去 + APPEND 进服务端 Sent，下次同步会按 `账号/文件夹/UID`
/// 把那封拉下来。若本地还留着「草稿转正」的那一行（uid 空、主键指向草稿箱），
/// 已发送里就是两封一模一样的邮件，用户会以为自己发了两遍。
#[test]
fn mark_sent_on_server_aligns_local_row_with_server_identity() {
    let h = Harness::new(8 * 1024 * 1024);
    h.seed_account();
    h.seed_mailbox("drafts", MailboxRole::Drafts, "Drafts");
    h.seed_mailbox("sent", MailboxRole::Sent, "Sent Items");

    let mut draft = Draft::new("对齐测试");
    draft.to = vec![Address::new("bob@example.com")];
    draft.text_body = Some("正文".into());
    let saved = h.mail.save_draft(ACC, &draft).unwrap();

    // 服务端 APPEND 回 UID 700：本地主键必须跟着变成 账号/文件夹/UID
    let sent = h
        .mail
        .mark_sent_on_server(&saved.id, "sent", "Sent Items", 700)
        .unwrap();
    assert_eq!(sent.id, format!("{ACC}/sent/700"));
    assert_eq!(sent.uid, Some(700), "UID 必须换成新文件夹里的，否则同步认不出这封");
    assert!(!sent.flags.contains(&Flag::Draft));
    assert!(sent.flags.contains(&Flag::Seen), "自己发的邮件不该显示未读");
    assert_eq!(sent.mailbox_ids, vec!["sent".to_string()]);
    assert!(sent.sent_at.is_some());
    // 旧主键必须消失：留着就是同步时多插一份的来源
    assert!(h.mail.read(&saved.id).is_err(), "旧草稿行应已被替换");
    assert!(h.mail.list_drafts(ACC, 20).unwrap().is_empty());
}

/// 本地库还没有这个文件夹的行（账号刚加、没同步过）时也不能失败：
/// 邮件其实早就发出去了，这里报错会让用户重发一遍。
#[test]
fn mark_sent_on_server_creates_missing_mailbox_row() {
    let h = Harness::new(8 * 1024 * 1024);
    h.seed_account();
    h.seed_mailbox("drafts", MailboxRole::Drafts, "Drafts");
    // 注意：没有 sent 这个文件夹行

    let mut draft = Draft::new("缺文件夹");
    draft.to = vec![Address::new("bob@example.com")];
    let saved = h.mail.save_draft(ACC, &draft).unwrap();

    let sent = h
        .mail
        .mark_sent_on_server(&saved.id, "server-sent", "Sent Items", 12)
        .unwrap();
    assert_eq!(sent.id, format!("{ACC}/server-sent/12"));
    let boxes = h.db.lock().unwrap().list_mailboxes(ACC).unwrap();
    let created = boxes.iter().find(|m| m.id == "server-sent").expect("应补出文件夹行");
    assert_eq!(created.role, MailboxRole::Sent);
    assert_eq!(created.name, "Sent Items");
}
