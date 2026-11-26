//! S5-01 对外接口的端到端测试。
//!
//! 这些用例直接调用 FFI 层导出的 API（与 Kotlin/Swift 拿到的形状一致），
//! 验证「绑定能生成」之外更关键的一件事：**接口真能用**——账号、草稿、
//! 标记、归档、删除、搜索、缓存这条链路要跑通，错误要能分类抛出来。

use crossmail_ffi::*;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("crossmail-ffi-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).expect("建临时目录");
        Self(p)
    }
    fn path(&self) -> String {
        self.0.to_string_lossy().to_string()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn engine(tag: &str) -> (CrossMailEngineHandle, TempDir) {
    let dir = TempDir::new(tag);
    let cfg = EngineConfig {
        db_path: ":memory:".into(),
        cache_dir: dir.path(),
        cache_quota_bytes: 8 * 1024 * 1024,
        master_key_b64: None,
    };
    let engine = open_engine(cfg).expect("打开引擎");
    (CrossMailEngineHandle { inner: engine }, dir)
}

/// 包一层只是为了让测试里调用更顺手（Arc 解引用），生产侧直接用 Arc。
struct CrossMailEngineHandle {
    inner: std::sync::Arc<CrossMailEngine>,
}

impl std::ops::Deref for CrossMailEngineHandle {
    type Target = CrossMailEngine;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

fn jmap_account(e: &CrossMailEngineHandle, email: &str) -> AccountDto {
    e.create_account(NewAccountDto {
        email_address: email.into(),
        display_name: Some("Test User".into()),
        protocol: ProtocolKindDto::Jmap,
        secret: "app-specific-password".into(),
        jmap_endpoint: Some("https://jmap.example.com".into()),
        imap_host: None,
        imap_port: None,
        imap_security: None,
        smtp_host: None,
        smtp_port: None,
        smtp_security: None,
        sync_days: Some(30),
        poll_interval_secs: Some(300),
    })
    .expect("创建账号")
}

fn draft(subject: &str, to: &str) -> DraftDto {
    DraftDto {
        to: vec![AddressDto {
            name: Some("收件人".into()),
            email: to.into(),
        }],
        cc: vec![],
        bcc: vec![],
        subject: subject.into(),
        text_body: Some("正文内容".into()),
        html_body: None,
        in_reply_to: None,
        references: vec![],
    }
}

// ---------------------------------------------------------------- 基础

#[test]
fn core_status_reports_version_and_protocols() {
    let s = core_status();
    assert!(!s.version.is_empty());
    assert!(s.supported_protocols.contains(&"jmap".to_string()));
    assert_eq!(protocol_display_name("jmap".into()), "JMAP (RFC 8620)");
    assert_eq!(protocol_display_name("imap".into()), "IMAP4rev1 + SMTP");
    // 未知协议回显原文，不编一个假名字
    assert_eq!(protocol_display_name("pop3".into()), "pop3");
}

#[test]
fn engine_opens_and_reports_empty_state() {
    let (e, _d) = engine("open");
    assert!(e.list_accounts().unwrap().is_empty());
    let status = e.cache_status().unwrap();
    assert_eq!(status.usage_bytes, 0);
    assert_eq!(status.quota_bytes, 8 * 1024 * 1024);
    assert!(!status.over_quota);
}

// ---------------------------------------------------------------- 账号

#[test]
fn account_create_list_update_and_delete() {
    let (e, _d) = engine("account");
    let created = jmap_account(&e, "user@example.com");
    assert_eq!(created.email_address, "user@example.com");
    assert_eq!(created.display_name.as_deref(), Some("Test User"));
    assert_eq!(created.protocol, ProtocolKindDto::Jmap);
    assert_eq!(
        created.jmap_endpoint.as_deref(),
        Some("https://jmap.example.com")
    );
    assert_eq!(created.sync_days, 30);

    let got = e.get_account(created.id.clone()).unwrap();
    assert_eq!(got.id, created.id);
    // 凭证绝不出现在 DTO 里：结构上就没有这个字段
    assert_eq!(e.list_accounts().unwrap().len(), 1);

    let updated = e
        .update_account(
            created.id.clone(),
            AccountPatchDto {
                display_name: Some("Test User (Work)".into()),
                clear_display_name: false,
                sync_days: Some(7),
                poll_interval_secs: None,
                jmap_endpoint: None,
                imap_host: None,
                imap_port: None,
                imap_security: None,
                smtp_host: None,
                smtp_port: None,
                smtp_security: None,
                clear_smtp: false,
                secret: None,
            },
        )
        .unwrap();
    assert_eq!(updated.display_name.as_deref(), Some("Test User (Work)"));
    assert_eq!(updated.sync_days, 7);

    // 显式清空昵称
    let cleared = e
        .update_account(
            created.id.clone(),
            AccountPatchDto {
                display_name: None,
                clear_display_name: true,
                sync_days: None,
                poll_interval_secs: None,
                jmap_endpoint: None,
                imap_host: None,
                imap_port: None,
                imap_security: None,
                smtp_host: None,
                smtp_port: None,
                smtp_security: None,
                clear_smtp: false,
                secret: None,
            },
        )
        .unwrap();
    assert_eq!(cleared.display_name, None);

    e.rotate_secret(created.id.clone(), "new-secret".into())
        .unwrap();
    e.delete_account(created.id.clone()).unwrap();
    assert!(e.list_accounts().unwrap().is_empty());

    let err = e.get_account(created.id).unwrap_err();
    assert!(matches!(err, CrossMailError::NotFound { .. }), "{err:?}");
}

#[test]
fn account_rejects_incomplete_or_invalid_input() {
    let (e, _d) = engine("bad-input");

    // 空凭证
    let err = e
        .create_account(NewAccountDto {
            email_address: "a@example.com".into(),
            display_name: None,
            protocol: ProtocolKindDto::Jmap,
            secret: "  ".into(),
            jmap_endpoint: Some("https://jmap.example.com".into()),
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            sync_days: None,
            poll_interval_secs: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, CrossMailError::InvalidInput { .. }),
        "{err:?}"
    );

    // IMAP 缺主机
    let err = e
        .create_account(NewAccountDto {
            email_address: "a@example.com".into(),
            display_name: None,
            protocol: ProtocolKindDto::Imap,
            secret: "pw".into(),
            jmap_endpoint: None,
            imap_host: None,
            imap_port: Some(993),
            imap_security: Some(SecurityModeDto::ImplicitTls),
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            sync_days: None,
            poll_interval_secs: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, CrossMailError::InvalidInput { .. }),
        "{err:?}"
    );

    // JMAP 端点不是 URL
    let err = e
        .create_account(NewAccountDto {
            email_address: "a@example.com".into(),
            display_name: None,
            protocol: ProtocolKindDto::Jmap,
            secret: "pw".into(),
            jmap_endpoint: Some("jmap.example.com".into()),
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            sync_days: None,
            poll_interval_secs: None,
        })
        .unwrap_err();
    assert!(
        matches!(err, CrossMailError::InvalidInput { .. }),
        "{err:?}"
    );

    assert!(
        e.list_accounts().unwrap().is_empty(),
        "失败的创建不得留下半截账号"
    );
}

#[test]
fn imap_account_roundtrips_endpoints() {
    let (e, _d) = engine("imap-account");
    let created = e
        .create_account(NewAccountDto {
            email_address: "user@example.com".into(),
            display_name: None,
            protocol: ProtocolKindDto::Imap,
            secret: "pw".into(),
            jmap_endpoint: None,
            imap_host: Some("imap.example.com".into()),
            imap_port: Some(993),
            imap_security: Some(SecurityModeDto::ImplicitTls),
            smtp_host: Some("smtp.example.com".into()),
            smtp_port: Some(587),
            smtp_security: Some(SecurityModeDto::StartTls),
            sync_days: None,
            poll_interval_secs: None,
        })
        .unwrap();

    assert_eq!(created.protocol, ProtocolKindDto::Imap);
    assert_eq!(created.imap_host.as_deref(), Some("imap.example.com"));
    assert_eq!(created.imap_port, Some(993));
    assert_eq!(created.imap_security, Some(SecurityModeDto::ImplicitTls));
    assert_eq!(created.smtp_host.as_deref(), Some("smtp.example.com"));
    assert_eq!(created.smtp_port, Some(587));
    assert_eq!(created.smtp_security, Some(SecurityModeDto::StartTls));
}

// ---------------------------------------------------------------- 邮件流程

#[test]
fn draft_read_flag_archive_delete_flow() {
    let (e, _d) = engine("mail-flow");
    let account = jmap_account(&e, "user@example.com");
    // 归档/删除依赖 Archive / Trash 角色存在，首次登录后要补齐系统文件夹
    e.ensure_system_mailboxes(account.id.clone())
        .expect("补齐系统文件夹");

    let saved = e
        .save_draft(account.id.clone(), draft("季度复盘", "bob@example.com"))
        .expect("存草稿");
    assert_eq!(saved.subject, "季度复盘");
    assert!(saved.flags.iter().any(|f| matches!(f, FlagDto::Draft)));
    assert_eq!(saved.to.len(), 1);
    assert_eq!(saved.to[0].email, "bob@example.com");

    let drafts = e.list_drafts(account.id.clone(), 10).unwrap();
    assert_eq!(drafts.len(), 1);

    // 列表分页契约
    let page = e
        .list_emails(
            account.id.clone(),
            EmailQueryDto {
                mailbox_id: None,
                unread_only: false,
                limit: 10,
                offset: 0,
            },
        )
        .unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(page.next_offset, None);

    // 已读 / 星标
    let read = e.set_read(saved.id.clone(), true).unwrap();
    assert!(read.flags.iter().any(|f| matches!(f, FlagDto::Seen)));
    let starred = e.toggle_star(saved.id.clone(), true).unwrap();
    assert!(starred.flags.iter().any(|f| matches!(f, FlagDto::Flagged)));
    let unstarred = e.toggle_star(saved.id.clone(), false).unwrap();
    assert!(!unstarred
        .flags
        .iter()
        .any(|f| matches!(f, FlagDto::Flagged)));

    // 归档：自动建 Archive
    let archived = e.archive_email(saved.id.clone()).expect("归档");
    assert_eq!(archived.subject, "季度复盘");
    let mailboxes = e.list_mailboxes(account.id.clone()).unwrap();
    assert!(
        mailboxes.iter().any(|m| m.role == MailboxRoleDto::Archive),
        "归档后应存在 Archive 文件夹: {mailboxes:?}"
    );

    // 删除：已补齐回收站，因此是「移入回收站」——仍可取回，不是真删
    let outcome = e.delete_email(saved.id.clone()).expect("删除");
    assert_eq!(outcome.email_id, saved.id);
    assert!(outcome.trashed, "有回收站时应移入回收站而不是真删");
    let trashed = e
        .get_email(saved.id.clone())
        .expect("回收站里的邮件仍应可读");
    assert!(trashed.flags.iter().any(|f| matches!(f, FlagDto::Deleted)));
    assert!(
        trashed.mailbox_ids.iter().any(|m| m.ends_with("/Trash")),
        "应落在回收站: {:?}",
        trashed.mailbox_ids
    );
}

#[test]
fn body_and_attachment_metadata_are_exposed() {
    let (e, _d) = engine("body");
    let account = jmap_account(&e, "user@example.com");
    let saved = e
        .save_draft(account.id.clone(), draft("带正文", "bob@example.com"))
        .unwrap();

    // 草稿正文应能读回来（落文件缓存，不限格式时必须回退）
    let body = e.read_body(saved.id.clone(), false).unwrap();
    assert_eq!(body.as_deref(), Some("正文内容"));
    let body_html = e.read_body(saved.id.clone(), true).unwrap();
    assert!(
        body_html.is_some(),
        "请求 HTML 时应回退到可用的纯文本，而不是返回空"
    );

    let full = e.get_email(saved.id).unwrap();
    assert!(!full.preview.is_empty(), "预览应由正文生成");
}

#[test]
fn unread_counts_track_flag_changes() {
    let (e, _d) = engine("unread");
    let account = jmap_account(&e, "user@example.com");
    let a = e
        .save_draft(account.id.clone(), draft("一", "bob@example.com"))
        .unwrap();
    e.save_draft(account.id.clone(), draft("二", "bob@example.com"))
        .unwrap();

    let drafts_box = e
        .list_mailboxes(account.id.clone())
        .unwrap()
        .into_iter()
        .find(|m| m.role == MailboxRoleDto::Drafts)
        .expect("应有 Drafts");

    let before = e.unread_counts(account.id.clone()).unwrap();
    let before_n = before
        .iter()
        .find(|c| c.mailbox_id == drafts_box.id)
        .map(|c| c.unread)
        .unwrap_or(0);

    e.set_read(a.id.clone(), true).unwrap();
    let after = e.unread_counts(account.id.clone()).unwrap();
    let after_n = after
        .iter()
        .find(|c| c.mailbox_id == drafts_box.id)
        .map(|c| c.unread)
        .unwrap_or(0);
    assert_eq!(after_n + 1, before_n, "标记已读后未读数应减一");
}

#[test]
fn search_returns_matching_mail_only() {
    let (e, _d) = engine("search");
    let account = jmap_account(&e, "user@example.com");
    e.save_draft(account.id.clone(), draft("季度复盘报告", "bob@example.com"))
        .unwrap();
    e.save_draft(account.id.clone(), draft("午餐约吗", "friend@example.com"))
        .unwrap();

    let hit = e
        .search(
            account.id.clone(),
            "复盘".into(),
            SearchScopeDto::Subject,
            10,
            0,
        )
        .unwrap();
    assert_eq!(hit.total, 1);
    assert_eq!(hit.items[0].subject, "季度复盘报告");

    let miss = e
        .search(
            account.id.clone(),
            "复盘".into(),
            SearchScopeDto::From,
            10,
            0,
        )
        .unwrap();
    assert_eq!(miss.total, 0, "只搜发件人时标题命中不算");

    // 空关键词不得把整库倒出来
    let empty = e
        .search(account.id.clone(), "   ".into(), SearchScopeDto::All, 10, 0)
        .unwrap();
    assert_eq!(empty.total, 0);
}

#[test]
fn mark_sent_moves_draft_out_of_drafts() {
    let (e, _d) = engine("mark-sent");
    let account = jmap_account(&e, "user@example.com");
    let saved = e
        .save_draft(account.id.clone(), draft("要发出去的", "bob@example.com"))
        .unwrap();

    let sent = e.mark_sent(saved.id.clone(), None).unwrap();
    assert!(!sent.flags.iter().any(|f| matches!(f, FlagDto::Draft)));
    assert!(sent.flags.iter().any(|f| matches!(f, FlagDto::Seen)));
    assert!(sent.sent_at.is_some(), "发出后应记录发送时间");
    assert!(
        e.list_drafts(account.id.clone(), 10).unwrap().is_empty(),
        "发出后不应还留在草稿箱"
    );
}

// ---------------------------------------------------------------- 发信

#[test]
fn send_email_rejects_empty_recipients_before_touching_network() {
    let (e, _d) = engine("send-no-rcpt");
    let account = jmap_account(&e, "user@example.com");
    let mut d = draft("没有收件人", "bob@example.com");
    d.to = vec![];

    let err = e.send_email(account.id.clone(), d).unwrap_err();
    assert!(
        matches!(err, CrossMailError::InvalidInput { .. }),
        "空收件人应报 InvalidInput，实际 {err:?}"
    );
    assert!(
        e.list_drafts(account.id, 10).unwrap().is_empty(),
        "参数错误不该先落一封草稿"
    );
}

#[test]
fn send_email_keeps_draft_when_submission_fails() {
    let (e, _d) = engine("send-fail");
    // STARTTLS 账号：编排层在 build_backend 就拒绝，是不依赖网络与外网的稳定失败样本。
    let account = imap_starttls_account(&e, "user@example.com");

    let err = e
        .send_email(account.id.clone(), draft("发不出去", "bob@example.com"))
        .unwrap_err();
    assert!(
        matches!(err, CrossMailError::Unsupported { .. }),
        "缺发信通道应为 Unsupported，实际 {err:?}"
    );
    let drafts = e.list_drafts(account.id, 10).unwrap();
    assert_eq!(drafts.len(), 1, "发送失败必须把草稿留在草稿箱");
    assert!(drafts[0].flags.iter().any(|f| matches!(f, FlagDto::Draft)));
}

#[test]
fn send_email_unknown_account_is_not_found() {
    let (e, _d) = engine("send-noacct");
    let err = e
        .send_email("no-such-account".into(), draft("x", "bob@example.com"))
        .unwrap_err();
    assert!(
        matches!(err, CrossMailError::NotFound { .. }),
        "未知账号应为 NotFound，实际 {err:?}"
    );
}

// ---------------------------------------------------------------- 缓存

#[test]
fn cache_status_and_clear() {
    let (e, _d) = engine("cache");
    let account = jmap_account(&e, "user@example.com");
    e.save_draft(account.id.clone(), draft("占点缓存", "bob@example.com"))
        .unwrap();

    let filled = e.cache_status().unwrap();
    assert!(filled.usage_bytes > 0, "存草稿后缓存应有占用");

    let cleared = e.clear_cache().unwrap();
    assert!(cleared > 0, "应清掉至少一个文件");
    assert_eq!(e.cache_status().unwrap().usage_bytes, 0);
    assert_eq!(e.enforce_cache_quota().unwrap().removed_count, 0);
}

#[test]
fn cache_quota_eviction_is_reported() {
    let dir = TempDir::new("quota");
    let cfg = EngineConfig {
        db_path: ":memory:".into(),
        cache_dir: dir.path(),
        // 配额给到极小，逼出一次淘汰
        cache_quota_bytes: 64,
        master_key_b64: None,
    };
    let e = open_engine(cfg).expect("打开引擎");
    let account = jmap_account(
        &CrossMailEngineHandle {
            inner: std::sync::Arc::clone(&e),
        },
        "user@example.com",
    );
    let mut big = draft("超配额", "bob@example.com");
    // 正文远大于 64 字节配额，逼出一次真实淘汰
    big.text_body = Some("配额测试正文".repeat(200));
    e.save_draft(account.id.clone(), big).unwrap();

    // 保存路径内部就会 enforce，所以「写完立刻回到配额内」才是正确契约；
    // 事后 usage 若仍是 3600 字节，说明淘汰根本没生效。
    let status = e.cache_status().unwrap();
    assert!(
        status.usage_bytes <= 64,
        "超配额必须自动裁到配额内，实际占用 {}",
        status.usage_bytes
    );
    assert!(!status.over_quota);

    // 已经裁干净了，再调一次应是幂等的 0
    let report = e.enforce_cache_quota().unwrap();
    assert_eq!(report.removed_count, 0, "已裁干净后不应再有淘汰");
    assert_eq!(report.freed_bytes, 0);
    assert!(report.usage_after <= 64);
}

#[test]
fn missing_email_reports_not_found_not_panic() {
    let (e, _d) = engine("not-found");
    for res in [
        e.get_email("nope".into()).map(|_| ()),
        e.set_read("nope".into(), true).map(|_| ()),
        e.toggle_star("nope".into(), true).map(|_| ()),
        e.archive_email("nope".into()).map(|_| ()),
        e.delete_email("nope".into()).map(|_| ()),
        e.read_body("nope".into(), false).map(|_| ()),
    ] {
        let err = res.unwrap_err();
        assert!(matches!(err, CrossMailError::NotFound { .. }), "{err:?}");
    }
}

#[test]
fn error_message_carries_user_hint() {
    let (e, _d) = engine("error-msg");
    let err = e.get_email("nope".into()).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("不存在"),
        "错误文本应包含面向用户的提示，实际: {msg}"
    );
}

// ------------------------------------------------ 同步与推送（S6-16）

/// 造一个「合法但 build_backend 必然零网络失败」的账号：IMAP 传输层只支持
/// 隐式 TLS，STARTTLS 在装配阶段就明确报 Unsupported。拿它当推送冒烟的
/// 可控假源——不连真服务器、不受网络时序影响。
fn imap_starttls_account(e: &CrossMailEngineHandle, email: &str) -> AccountDto {
    e.create_account(NewAccountDto {
        email_address: email.into(),
        display_name: None,
        protocol: ProtocolKindDto::Imap,
        secret: "pw".into(),
        jmap_endpoint: None,
        imap_host: Some("imap.example.com".into()),
        imap_port: Some(143),
        imap_security: Some(SecurityModeDto::StartTls),
        smtp_host: None,
        smtp_port: None,
        smtp_security: None,
        sync_days: None,
        poll_interval_secs: None,
    })
    .expect("创建 IMAP/STARTTLS 账号")
}

#[derive(Clone, Default)]
struct RecordingObserver(Arc<Mutex<Vec<PushEventDto>>>);

impl PushObserver for RecordingObserver {
    fn on_event(&self, event: PushEventDto) {
        self.0.lock().expect("observer 锁未中毒").push(event);
    }
}

fn wait_for_events(observer: &RecordingObserver, n: usize) -> Vec<PushEventDto> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        {
            let guard = observer.0.lock().unwrap();
            if guard.len() >= n {
                return guard.clone();
            }
        }
        assert!(Instant::now() < deadline, "等待 {n} 条推送事件超时");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sync_account_maps_unsupported_error_across_ffi() {
    let (e, _d) = engine("sync-unsupported");
    let account = imap_starttls_account(&e, "user@example.com");
    let err = e.sync_account(account.id).unwrap_err();
    assert!(
        matches!(err, CrossMailError::Unsupported { .. }),
        "STARTTLS 应映射成 Unsupported，实际: {err:?}"
    );
}

#[test]
fn sync_account_unknown_id_is_not_found() {
    let (e, _d) = engine("sync-not-found");
    let err = e.sync_account("nope".into()).unwrap_err();
    assert!(matches!(err, CrossMailError::NotFound { .. }), "{err:?}");
}

#[test]
fn subscribe_push_delivers_failed_closed_and_cancels() {
    let (e, _d) = engine("push-fail");
    let account = imap_starttls_account(&e, "user@example.com");
    let observer = RecordingObserver::default();
    let sub = e
        .subscribe_push(account.id, Box::new(observer.clone()))
        .expect("订阅应立即返回句柄");

    assert!(!sub.is_cancelled());
    let events = wait_for_events(&observer, 2);

    let first = &events[0];
    assert_eq!(first.kind, PushEventKindDto::Failed);
    assert_eq!(first.reason, "push");
    assert!(
        first.error.contains("STARTTLS"),
        "错误文本应带上真实原因: {}",
        first.error
    );
    assert_eq!(first.unread_total, 0);
    assert!(first.unread_by_mailbox.is_empty());
    assert!(first.retry_in_ms.is_none());

    let closed = events
        .iter()
        .find(|ev| ev.kind == PushEventKindDto::Closed)
        .expect("流结束必须补 Closed，UI 才不会永远等");
    assert!(closed.reason.is_empty());
    assert!(closed.error.is_empty());

    sub.cancel();
    assert!(sub.is_cancelled());
}
