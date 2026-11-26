//! S2 端到端单测：用脚本化通道跑完整的 IMAP / SMTP 命令-响应流程（需求 S2-09）。

use crossmail_core::error::{ErrorKind, MailError};
use crossmail_core::model::{
    Address, Flag, MailboxRole, OutgoingAttachment, OutgoingMessage, SecurityMode,
};
use crossmail_imap::capability::Capabilities;
use crossmail_imap::client::{ExpungeStrategy, ImapClient, MoveStrategy};
use crossmail_imap::conn::{security_for_port, with_retry, ConnectionPolicy};
use crossmail_imap::draft;
use crossmail_imap::gmail::GmailAdapter;
use crossmail_imap::smtp::{self, AuthMech, SmtpCapabilities, SmtpClient};
use crossmail_imap::sync::{self, SyncState};
use crossmail_imap::transport::testing::MockIo;

fn open(
    script: &[&str],
    caps: &[&str],
) -> (ImapClient, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let mock = MockIo::new(script.iter().copied());
    let log = mock.log_handle();
    let client = ImapClient::with_capabilities(
        Box::new(mock),
        Capabilities::from_items(caps.iter().copied()),
    );
    (client, log)
}

fn commands(log: &std::sync::Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().map(|g| g.clone()).unwrap_or_default()
}

// ---------------------------------------------------------------- 连接与登录

#[test]
fn security_policy_rejects_plaintext_ports() {
    assert_eq!(security_for_port(993).unwrap(), SecurityMode::ImplicitTls);
    assert_eq!(security_for_port(143).unwrap(), SecurityMode::StartTls);
    assert_eq!(
        security_for_port(110).unwrap_err().kind(),
        ErrorKind::Unsupported
    );
}

#[tokio::test]
async fn login_quotes_credentials_and_advances_tag() {
    let (mut c, log) = open(&["A1 OK LOGIN completed"], &["IMAP4rev1"]);
    c.login("alice@example.com", "p@ss\"word").await.unwrap();
    let cmds = commands(&log);
    assert_eq!(cmds[0], "A1 LOGIN \"alice@example.com\" \"p@ss\\\"word\"");
}

#[tokio::test]
async fn login_failure_maps_to_authentication_error() {
    let (mut c, _log) = open(&["A1 NO [AUTHENTICATIONFAILED] invalid credentials"], &[]);
    let err = c.login("a@b.com", "x").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Authentication);
    assert_eq!(err.user_hint(), "登录状态失效，请重新登录账号");
}

#[tokio::test]
async fn greeting_exposes_capabilities() {
    let (mut c, _log) = open(
        &["* OK [CAPABILITY IMAP4rev1 UIDPLUS MOVE IDLE CONDSTORE X-GM-EXT-1] ready"],
        &[],
    );
    c.read_greeting().await.unwrap();
    let pc = c.provider_capabilities();
    assert!(pc.uidplus && pc.imap_move && pc.imap_idle && pc.condstore && pc.gmail_labels);
    assert!(!pc.jmap_mail);
}

#[tokio::test]
async fn bye_on_connect_is_network_error() {
    let (mut c, _log) = open(&["* BYE too many connections"], &[]);
    assert_eq!(
        c.read_greeting().await.unwrap_err().kind(),
        ErrorKind::Network
    );
}

#[tokio::test]
async fn authenticate_plain_sends_sasl_token() {
    let (mut c, log) = open(&["A1 OK authenticated"], &["AUTH=PLAIN"]);
    c.authenticate_plain("alice", "secret").await.unwrap();
    let cmd = &commands(&log)[0];
    assert!(cmd.starts_with("A1 AUTHENTICATE PLAIN "));
    // base64("\0alice\0secret")
    assert!(cmd.contains("AGFsaWNlAHNlY3JldA=="));
}

// ---------------------------------------------------------------- 文件夹

#[tokio::test]
async fn list_folders_decodes_utf7_and_special_use() {
    let (mut c, log) = open(
        &[
            r#"* LIST (\HasNoChildren \Sent) "/" "Sent Items""#,
            r#"* LIST (\HasNoChildren) "/" "&XfJT0ZAB-""#,
            r#"* LIST (\HasChildren) "/" "Work/2026""#,
            "A1 OK LIST completed",
        ],
        &[],
    );
    let folders = c.list_folders("acc1").await.unwrap();
    assert_eq!(folders.len(), 3);
    assert_eq!(folders[0].role, MailboxRole::Sent);
    assert_eq!(folders[1].name, "已发送");
    assert_eq!(folders[2].parent_id.as_deref(), Some("Work"));
    assert_eq!(commands(&log)[0], "A1 LIST \"\" \"*\"");
}

#[tokio::test]
async fn role_guessed_when_server_omits_special_use() {
    // 回归：这条测试原本**自己手动调 fill_missing_roles**，
    // 于是即使生产路径压根没接线也照样通过（正是它掩盖了真缺陷）。
    // 现在必须只走 list_folders，由生产代码自己补角色。
    //
    // 用例照抄 Stalwart 的真实返回：INBOX 是裸 `() "/" "INBOX"`，不带任何 special-use。
    let (mut c, _log) = open(
        &[
            r#"* LIST () "/" "INBOX""#,
            r#"* LIST () "/" "Sent""#,
            r#"* LIST () "/" "Drafts""#,
            r#"* LIST (\Trash) "/" "Deleted Items""#,
            r#"* LIST () "/" "Newsletter""#,
            "A1 OK LIST completed",
        ],
        &[],
    );
    let folders = c.list_folders("acc").await.unwrap();
    assert_eq!(
        folders[0].role,
        MailboxRole::Inbox,
        "服务器不给 INBOX 标 special-use 时必须按名字兜底，否则整条业务链都找不到收件箱"
    );
    assert_eq!(folders[1].role, MailboxRole::Sent);
    assert_eq!(folders[2].role, MailboxRole::Drafts);
    // 服务端明确标了的一律以服务端为准，不能被名字猜测覆盖
    assert_eq!(folders[3].role, MailboxRole::Trash);
    assert_eq!(folders[4].role, MailboxRole::Other);
}

// ---------------------------------------------------------------- SELECT 与增量同步

#[tokio::test]
async fn select_collects_uidvalidity_uidnext_and_modseq() {
    let (mut c, _log) = open(
        &[
            r#"* FLAGS (\Answered \Flagged \Deleted \Seen \Draft)"#,
            "* 12 EXISTS",
            "* 2 RECENT",
            "* OK [UIDVALIDITY 42] UIDs valid",
            "* OK [UIDNEXT 40] Predicted next UID",
            "* OK [HIGHESTMODSEQ 1024] Highest modseq",
            "A1 OK [READ-WRITE] SELECT completed",
        ],
        &["CONDSTORE"],
    );
    let st = c.select("INBOX").await.unwrap();
    assert_eq!(st.uid_validity, Some(42));
    assert_eq!(st.uid_next, Some(40));
    assert_eq!(st.highest_modseq, Some(1024));
    assert_eq!(st.exists, 12);
    assert!(!st.read_only);
    assert_eq!(c.state().mailbox.as_deref(), Some("INBOX"));
}

#[tokio::test]
async fn select_read_only_mailbox_flagged() {
    let (mut c, _log) = open(
        &[
            "* 0 EXISTS",
            "* OK [UIDVALIDITY 1] ok",
            "A1 OK [READ-ONLY] done",
        ],
        &[],
    );
    let st = c.select("Archive").await.unwrap();
    assert!(st.read_only);
}

#[tokio::test]
async fn sync_plan_feeds_fetch_command() {
    let caps = Capabilities::from_items(["IMAP4rev1", "UIDPLUS"]);
    let server = sync::ServerState {
        uid_validity: 9,
        uid_next: 120,
        highest_modseq: None,
        exists: 100,
    };
    let plan = sync::plan_sync(
        &caps,
        &server,
        &SyncState {
            uid_validity: Some(9),
            last_uid: Some(60),
            highest_modseq: None,
        },
    );
    assert_eq!(plan.uid_set, "61:*");
    assert!(!plan.is_noop());

    let (mut c, log) = open(&["A1 OK FETCH completed"], &["UIDPLUS"]);
    c.fetch_metadata(&plan.uid_set, "acc", "inbox")
        .await
        .unwrap();
    assert!(commands(&log)[0].contains("UID FETCH 61:*"));
}

// ---------------------------------------------------------------- 邮件元数据

#[tokio::test]
async fn fetch_metadata_maps_envelope_flags_and_attachments() {
    let fetch = r#"* 1 FETCH (UID 300 FLAGS (\Seen \Flagged) RFC822.SIZE 2048 MODSEQ (77) INTERNALDATE "13-Nov-2026 10:00:00 +0800" ENVELOPE ("Fri, 13 Nov 2026 10:00:00 +0800" "季度报告" (("Alice" NIL "alice" "example.com")) NIL NIL (("Bob" NIL "bob" "example.com")) (("Carol" NIL "carol" "example.com")) NIL NIL "<m1@example.com>") BODYSTRUCTURE (("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" 100 3 NIL NIL NIL)("APPLICATION" "PDF" ("NAME" "a.pdf") NIL NIL "BASE64" 900 NIL ("ATTACHMENT" ("FILENAME" "a.pdf")) NIL) "MIXED" ("BOUNDARY" "b1") NIL NIL NIL))"#;
    let (mut c, _log) = open(&[fetch, "A1 OK FETCH completed"], &[]);
    let emails = c.fetch_metadata("1:*", "acc", "inbox").await.unwrap();
    assert_eq!(emails.len(), 1);
    let e = &emails[0];
    assert_eq!(e.uid, Some(300));
    assert_eq!(e.subject, "季度报告");
    assert_eq!(e.from[0].email, "alice@example.com");
    assert_eq!(e.to[0].email, "bob@example.com");
    assert_eq!(e.cc[0].email, "carol@example.com");
    assert_eq!(e.message_id.as_deref(), Some("<m1@example.com>"));
    assert_eq!(e.size, 2048);
    assert_eq!(e.modseq, Some(77));
    assert!(e.is_read());
    assert!(e.flags.contains(&Flag::Flagged));
    assert!(e.sent_at.is_some());
    // 正文未下载，只记录部件位置（I-005 按需拉取）
    assert!(e.text_body.is_none());
    assert_eq!(e.attachments.len(), 1);
    assert_eq!(e.attachments[0].name.as_deref(), Some("a.pdf"));
    // 描述符要带上传传输编码与字符集，否则上层取正文后无法正确解码（base64 正文会变乱码）。
    // 按字段断言而不是整串比对：格式以后还会加字段，整串比对会把每次扩展都变成"改测试"。
    let desc = e
        .raw_headers
        .get("X-Body-Part")
        .map(|s| s.as_str())
        .expect("缺少正文部件描述符");
    let fields: Vec<&str> = desc.split(';').collect();
    assert_eq!(fields[0], "1", "部件号");
    assert!(fields.contains(&"size=100"), "尺寸丢失: {desc}");
    assert!(
        fields.iter().any(|f| f.starts_with("cte=")),
        "必须带上传输编码: {desc}"
    );
    assert!(
        fields.contains(&"cte=7bit"),
        "服务端未给编码时按 7bit 兜底: {desc}"
    );
    assert!(
        fields.iter().any(|f| f.starts_with("charset=")),
        "必须带上字符集: {desc}"
    );
}

#[tokio::test]
async fn fetch_part_reads_literal_body() {
    let mut mock = MockIo::new(Vec::<String>::new());
    mock.push_line("* 1 FETCH (UID 300 BODY[TEXT] {11}");
    mock.push_bytes(b"Hello world".to_vec());
    mock.push_line(")");
    mock.push_line("A1 OK FETCH completed");
    let mut c = ImapClient::with_capabilities(Box::new(mock), Capabilities::default());
    let data = c.fetch_part(300, "1.1").await.unwrap();
    assert_eq!(data, b"Hello world");
}

#[tokio::test]
async fn missing_part_is_not_found() {
    let (mut c, _log) = open(&["A1 OK FETCH completed"], &[]);
    let err = c.fetch_part(9, "2").await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

// ---------------------------------------------------------------- 标记与移动

#[tokio::test]
async fn store_flags_sends_add_and_remove() {
    let (mut c, log) = open(&["A1 OK STORE completed", "A2 OK STORE completed"], &[]);
    c.store_flags("1:3", &[Flag::Seen], &[Flag::Flagged])
        .await
        .unwrap();
    let cmds = commands(&log);
    assert_eq!(cmds[0], "A1 UID STORE 1:3 +FLAGS.SILENT (\\Seen)");
    assert_eq!(cmds[1], "A2 UID STORE 1:3 -FLAGS.SILENT (\\Flagged)");
}

#[tokio::test]
async fn store_flags_noop_when_empty() {
    let (mut c, log) = open(&[], &[]);
    c.store_flags("1:3", &[], &[]).await.unwrap();
    assert!(commands(&log).is_empty());
}

#[tokio::test]
async fn move_prefers_move_extension() {
    let (mut c, log) = open(&["A1 OK MOVE completed"], &["MOVE"]);
    let out = c.move_messages(&[1, 2, 3], "Archive").await.unwrap();
    assert_eq!(out.strategy, MoveStrategy::Move);
    assert_eq!(commands(&log)[0], "A1 UID MOVE 1:3 \"Archive\"");
}

#[tokio::test]
async fn move_falls_back_to_copy_store_expunge() {
    let (mut c, log) = open(
        &[
            "A1 OK COPY completed",
            "A2 OK STORE completed",
            "A3 OK EXPUNGE completed",
        ],
        &["IMAP4rev1", "UIDPLUS"],
    );
    let out = c.move_messages(&[7], "[Gmail]/All Mail").await.unwrap();
    assert_eq!(out.strategy, MoveStrategy::CopyStoreExpunge);
    let cmds = commands(&log);
    assert_eq!(cmds[0], "A1 UID COPY 7 \"[Gmail]/All Mail\"");
    assert!(cmds[1].contains("+FLAGS.SILENT (\\Deleted)"));
    assert_eq!(cmds[2], "A3 UID EXPUNGE 7");
}

/// 无 UIDPLUS 时的降级：只 COPY + \Deleted，**不发 EXPUNGE**。
///
/// 早期实现无条件发 `UID EXPUNGE`，但那是 UIDPLUS 才有的命令，
/// 在不支持的服务器上会被拒（整次移动失败）；退化成裸 `EXPUNGE`
/// 又会连带清掉本箱其他 \Deleted 邮件。所以这里选择延迟删除。
#[tokio::test]
async fn move_without_uidplus_never_expunges_whole_mailbox() {
    let (mut c, log) = open(
        &["A1 OK COPY completed", "A2 OK STORE completed"],
        &["IMAP4rev1"],
    );
    let out = c.move_messages(&[9], "Archive").await.unwrap();
    assert_eq!(out.strategy, MoveStrategy::CopyStore);
    let cmds = commands(&log);
    assert_eq!(cmds.len(), 2, "不该多发命令: {cmds:?}");
    assert!(
        !cmds.iter().any(|c| c.contains("EXPUNGE")),
        "无 UIDPLUS 时不得发 EXPUNGE/UID EXPUNGE: {cmds:?}"
    );
}

/// rev2 把 UIDPLUS 吸收进核心，服务器不会单独列出 `UIDPLUS`；
/// 不按版本推断就会把「支持」判成「不支持」，退到延迟删除。
#[test]
fn rev2_implies_uidplus() {
    assert!(Capabilities::from_items(["IMAP4rev2"]).supports_uidplus());
    assert!(!Capabilities::from_items(["IMAP4rev1"]).supports_uidplus());
    assert!(Capabilities::from_items(["IMAP4rev1", "UIDPLUS"]).supports_uidplus());
    // rev2 同时隐含 MOVE，所以真实 rev2 服务器走不到 COPY+EXPUNGE 降级路径
    assert!(Capabilities::from_items(["IMAP4rev2"]).supports_move());
}

#[tokio::test]
async fn expunge_uids_is_precise_with_uidplus() {
    let (mut c, log) = open(
        &["A1 OK STORE completed", "A2 OK EXPUNGE completed"],
        &["IMAP4rev1", "UIDPLUS"],
    );
    let got = c.expunge_uids(&[11, 12]).await.unwrap();
    assert_eq!(got, ExpungeStrategy::Precise);
    let cmds = commands(&log);
    assert!(cmds[0].contains("+FLAGS.SILENT (\\Deleted)"), "{}", cmds[0]);
    assert_eq!(cmds[1], "A2 UID EXPUNGE 11:12");
}

#[tokio::test]
async fn expunge_uids_degrades_to_mailbox_wide_and_says_so() {
    let (mut c, log) = open(
        &["A1 OK STORE completed", "A2 OK EXPUNGE completed"],
        &["IMAP4rev1"],
    );
    let got = c.expunge_uids(&[5]).await.unwrap();
    assert_eq!(got, ExpungeStrategy::MailboxWide);
    assert_eq!(commands(&log)[1], "A2 EXPUNGE");
}

#[tokio::test]
async fn expunge_uids_empty_is_noop() {
    let (mut c, log) = open(&[], &["IMAP4rev1", "UIDPLUS"]);
    assert_eq!(c.expunge_uids(&[]).await.unwrap(), ExpungeStrategy::Precise);
    assert!(commands(&log).is_empty());
}

#[tokio::test]
async fn gmail_move_never_expunges() {
    let (mut c, log) = open(
        &["A1 OK COPY completed", "A2 OK STORE completed"],
        &["IMAP4rev1", "X-GM-EXT-1"],
    );
    let out = c.move_messages(&[4, 5], "[Gmail]/All Mail").await.unwrap();
    assert_eq!(out.strategy, MoveStrategy::CopyStore);
    assert!(!commands(&log).iter().any(|c| c.contains("EXPUNGE")));
}

#[tokio::test]
async fn empty_move_rejected() {
    let (mut c, _log) = open(&[], &["MOVE"]);
    assert_eq!(
        c.move_messages(&[], "Archive").await.unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn gmail_archive_removes_inbox_label() {
    let g = GmailAdapter::default();
    let cmds = g.archive_commands(&[10, 11]);
    assert_eq!(cmds, vec!["UID STORE 10:11 -X-GM-LABELS.SILENT (\\Inbox)"]);
    assert!(!g.allow_expunge());
}

// ---------------------------------------------------------------- APPEND / 草稿

#[tokio::test]
async fn append_message_waits_continuation_and_returns_uid() {
    let mut mock = MockIo::new(Vec::<String>::new());
    mock.push_line("+ Ready for literal data");
    mock.push_line("A1 OK [APPENDUID 42 100] append completed");
    let mut c =
        ImapClient::with_capabilities(Box::new(mock), Capabilities::from_items(["UIDPLUS"]));
    let body = b"Subject: draft\r\n\r\nhello";
    let uid = c
        .append_message("Drafts", &[Flag::Draft], body)
        .await
        .unwrap();
    assert_eq!(uid, Some(100));
}

#[tokio::test]
async fn append_rejection_is_propagated() {
    let mut mock = MockIo::new(Vec::<String>::new());
    mock.push_line("A1 NO [TRYCREATE] no such mailbox");
    let mut c = ImapClient::new(Box::new(mock));
    let err = c
        .append_message("Nope", &[Flag::Draft], b"x")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}

#[test]
fn draft_merge_conflict_keeps_both_versions() {
    let mut local = crossmail_core::model::Email::new("L1", "acc", "drafts");
    local.subject = "周报".into();
    local.message_id = Some("<d1@x>".into());
    local.text_body = Some("本地修改".into());
    local.to = vec![Address::new("boss@example.com")];

    let mut remote = local.clone();
    remote.id = "R1".into();
    remote.text_body = Some("另一台设备修改".into());

    let report = draft::merge_drafts(&[local], &[remote]);
    assert_eq!(report.conflicts.len(), 1);
    assert_eq!(report.to_upload.len(), 1);
    assert_eq!(report.to_adopt.len(), 0);
}

// ---------------------------------------------------------------- 回退重连

#[tokio::test]
async fn reconnect_backoff_retries_network_errors_only() {
    let policy = ConnectionPolicy::quick_for_test();
    let mut attempts = 0;
    let out: Result<(), MailError> = with_retry(&policy, |_| {
        attempts += 1;
        async { Err(MailError::Network("reset".into())) }
    })
    .await;
    assert!(out.is_err());
    assert_eq!(attempts, 4);

    let mut fatal_attempts = 0;
    let _: Result<(), MailError> = with_retry(&policy, |_| {
        fatal_attempts += 1;
        async { Err(MailError::Protocol("bad".into())) }
    })
    .await;
    assert_eq!(fatal_attempts, 1);
}

// ---------------------------------------------------------------- SMTP

fn smtp_caps() -> SmtpCapabilities {
    SmtpCapabilities::parse_ehlo(&[
        "mx.example.com".to_string(),
        "AUTH PLAIN LOGIN".to_string(),
        "SIZE 10485760".to_string(),
        "8BITMIME".to_string(),
    ])
}

#[test]
fn ehlo_capabilities_parsed() {
    let caps = smtp_caps();
    assert_eq!(caps.auth, vec!["PLAIN".to_string(), "LOGIN".to_string()]);
    assert_eq!(caps.size, Some(10_485_760));
    assert!(caps.eight_bit);
    assert!(!caps.starttls);
}

#[test]
fn auth_mechanism_priority_follows_spec() {
    // S6-18 修订：PLAIN 优先（授权码服务器广告 XOAUTH2 但只收授权码；
    // OAuth token 兼容 PLAIN），XOAUTH2 仅在唯一提供时使用。
    let all = SmtpCapabilities::parse_ehlo(&["AUTH LOGIN PLAIN XOAUTH2".into()]);
    assert_eq!(AuthMech::select(&all), Some(AuthMech::Plain));

    let plain_login = SmtpCapabilities::parse_ehlo(&["AUTH LOGIN PLAIN".into()]);
    assert_eq!(AuthMech::select(&plain_login), Some(AuthMech::Plain));

    let login_only = SmtpCapabilities::parse_ehlo(&["AUTH LOGIN".into()]);
    assert_eq!(AuthMech::select(&login_only), Some(AuthMech::Login));

    let xoauth2_only = SmtpCapabilities::parse_ehlo(&["AUTH XOAUTH2".into()]);
    assert_eq!(AuthMech::select(&xoauth2_only), Some(AuthMech::XOAuth2));

    let none = SmtpCapabilities::parse_ehlo(&["SIZE 100".into()]);
    assert_eq!(AuthMech::select(&none), None);
}

#[tokio::test]
async fn smtp_send_happy_path_plain_auth() {
    let mock = smtp::testing::MockSmtpIo::new(vec![
        "220 mx.example.com ESMTP ready",
        "250-mx.example.com greets you",
        "250-AUTH PLAIN LOGIN",
        "250-SIZE 10485760",
        "250 8BITMIME",
        "235 2.7.0 Authentication successful",
        "250 2.1.0 Sender ok",
        "250 2.1.5 Recipient ok",
        "250 2.1.5 Recipient ok",
        "354 End data with <CR><LF>.<CR><LF>",
        "250 2.0.0 Ok: queued as ABC123",
    ]);
    let log = mock.log_handle();
    let mut client = SmtpClient::new(Box::new(mock));
    client.greet().await.unwrap();
    assert_eq!(
        client
            .authenticate("alice@example.com", "pass")
            .await
            .unwrap(),
        AuthMech::Plain
    );

    let mut msg = OutgoingMessage::new(
        "acc",
        Address::with_name("Alice", "alice@example.com"),
        vec![Address::new("bob@example.com")],
        "月度报告",
    );
    msg.text_body = Some("hello bob".into());
    msg.cc = vec![Address::new("carol@example.com")];
    client.send_message(&msg).await.unwrap();

    let cmds = log.lock().unwrap().clone();
    assert!(cmds[0].starts_with("EHLO "));
    assert!(cmds[1].starts_with("AUTH PLAIN "));
    assert_eq!(cmds[2], "MAIL FROM:<alice@example.com>");
    assert!(cmds[3].starts_with("RCPT TO:<bob@example.com>"));
    assert!(cmds[4].starts_with("RCPT TO:<carol@example.com>"));
    assert_eq!(cmds[5], "DATA");
    assert!(cmds.contains(&".".to_string()));
}

/// 服务端唯一提供 XOAUTH2（OAuth-only）时：走 XOAUTH2。
#[tokio::test]
async fn smtp_xoauth2_only_server_uses_xoauth2() {
    let mock = smtp::testing::MockSmtpIo::new(vec![
        "220 ready",
        "250-AUTH XOAUTH2",
        "250 OK",
        "235 accepted",
    ]);
    let log = mock.log_handle();
    let mut client = SmtpClient::new(Box::new(mock));
    client.greet().await.unwrap();
    let mech = client
        .authenticate("alice@example.com", "token-123")
        .await
        .unwrap();
    assert_eq!(mech, AuthMech::XOAuth2);
    let cmds = log.lock().unwrap().clone();
    assert!(cmds[1].starts_with("AUTH XOAUTH2 "));
}

/// XOAUTH2-only 服务器收到凭据被拒的 334 挑战：归类为认证错误。
#[tokio::test]
async fn smtp_xoauth2_rejected_maps_to_auth_error() {
    let mock = smtp::testing::MockSmtpIo::new(vec![
        "220 ready",
        "250-AUTH XOAUTH2",
        "250 OK",
        "334 eyJzdGF0dXMiOiI0MDAifQ",
    ]);
    let mut client = SmtpClient::new(Box::new(mock));
    client.greet().await.unwrap();
    let err = client
        .authenticate("alice@example.com", "auth-code")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Authentication);
}

#[tokio::test]
async fn smtp_rejected_recipient_is_reported() {
    let mock = smtp::testing::MockSmtpIo::new(vec![
        "220 ready",
        "250-AUTH PLAIN",
        "250 OK",
        "235 accepted",
        "250 Sender ok",
        "550 5.1.1 No such user",
    ]);
    let mut client = SmtpClient::new(Box::new(mock));
    client.greet().await.unwrap();
    client.authenticate("a@example.com", "p").await.unwrap();
    let msg = OutgoingMessage::new(
        "acc",
        Address::new("a@example.com"),
        vec![Address::new("ghost@example.com")],
        "hi",
    );
    let err = client.send_message(&msg).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Protocol);
}

#[tokio::test]
async fn smtp_size_limit_enforced_locally() {
    let mock = smtp::testing::MockSmtpIo::new(vec![
        "220 ready",
        "250-AUTH PLAIN",
        "250-SIZE 10",
        "250 OK",
    ]);
    let mut client = SmtpClient::new(Box::new(mock));
    client.greet().await.unwrap();
    let mut msg = OutgoingMessage::new(
        "acc",
        Address::new("a@example.com"),
        vec![Address::new("b@example.com")],
        "hi",
    );
    msg.text_body = Some("x".repeat(200));
    let err = client.send_message(&msg).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Quota);
}

#[tokio::test]
async fn smtp_auth_failure_maps_to_authentication() {
    let mock = smtp::testing::MockSmtpIo::new(vec![
        "220 ready",
        "250-AUTH PLAIN",
        "250 OK",
        "535 5.7.8 Username and Password not accepted",
    ]);
    let mut client = SmtpClient::new(Box::new(mock));
    client.greet().await.unwrap();
    let err = client
        .authenticate("a@example.com", "wrong")
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Authentication);
}

#[test]
fn message_build_encodes_headers_and_mime() {
    let mut msg = OutgoingMessage::new(
        "acc",
        Address::with_name("Alice\nBcc: evil@example.com", "alice@example.com"),
        vec![Address::new("bob@example.com")],
        "季度总结",
    );
    msg.text_body = Some("正文".into());
    msg.html_body = Some("<p>正文</p>".into());
    msg.attachments.push(OutgoingAttachment {
        name: "report.pdf".into(),
        content_type: "application/pdf".into(),
        data: b"%PDF-1.4".to_vec(),
    });
    let raw = String::from_utf8(smtp::build_message(&msg).unwrap()).unwrap();

    // 头部注入无效：换行被剥离，不会产生独立的 Bcc 头行
    assert!(!raw.lines().any(|l| l.trim_start().starts_with("Bcc:")));
    // 全部使用 CRLF，不存在裸 LF
    assert!(!raw.replace("\r\n", "").contains('\n'));
    // 中文标题按 RFC 2047 编码
    assert!(raw.contains("Subject: =?UTF-8?B?"));
    assert!(raw.contains("multipart/mixed"));
    assert!(raw.contains("multipart/alternative"));
    assert!(raw.contains("Content-Disposition: attachment; filename=\"report.pdf\""));
    assert!(raw.contains("MIME-Version: 1.0"));
}

#[test]
fn dot_stuffing_prefixes_leading_dots() {
    let lines = smtp::dot_stuffed_lines(b".hidden\r\nnormal line\r\n");
    assert_eq!(
        lines,
        vec![".hidden".replace('.', ".."), "normal line".to_string()]
    );
}

#[test]
fn smtp_port_follows_security_mode() {
    assert_eq!(smtp::port_for(SecurityMode::ImplicitTls), 465);
    assert_eq!(smtp::port_for(SecurityMode::StartTls), 587);
}

// ------------------------------------------- 登录后能力刷新（真机倒逼的缺陷）

/// 真机事实：问候语只带 9 项能力，`IDLE` / `CONDSTORE` / `UIDPLUS` 只在**登录后**出现。
/// 漏掉登录响应里的 `[CAPABILITY ...]`，高级特性会被全部误判为不支持并静默降级。
#[tokio::test]
async fn login_absorbs_capabilities_from_ok_response() {
    let (mut c, _log) = open(
        &[
            // 登录前：够认证用，但没有 IDLE / CONDSTORE / UIDPLUS
            "* OK [CAPABILITY IMAP4rev1 SASL-IR AUTH=PLAIN AUTH=LOGIN] ready",
            // 登录成功响应携带完整能力
            "A1 OK [CAPABILITY IMAP4rev1 IDLE CONDSTORE UIDPLUS MOVE] Authentication successful",
        ],
        &[],
    );
    c.read_greeting().await.unwrap();
    let before = c.provider_capabilities();
    assert!(
        !before.imap_idle && !before.condstore && !before.uidplus,
        "登录前不该声称支持这些能力"
    );

    c.login("alice@example.com", "secret").await.unwrap();

    let after = c.provider_capabilities();
    assert!(after.imap_idle, "登录后必须拿到 IDLE");
    assert!(after.push, "push 与 idle 同源");
    assert!(after.condstore, "登录后必须拿到 CONDSTORE");
    assert!(after.uidplus, "登录后必须拿到 UIDPLUS");
    assert!(after.imap_move);
}

/// 同一路由：任何命令的 OK 响应都可能带 `[CAPABILITY ...]`（RFC 3501 §7.1），
/// 不只登录——服务器中途升级能力也要能被吸收。
#[tokio::test]
async fn any_ok_response_can_refresh_capabilities() {
    let (mut c, _log) = open(
        &[
            "* OK [CAPABILITY IMAP4rev1] ready",
            "A1 OK [CAPABILITY IMAP4rev1 IDLE] ENABLE completed",
        ],
        &[],
    );
    c.read_greeting().await.unwrap();
    assert!(!c.provider_capabilities().imap_idle);
    c.cmd("ENABLE CONDSTORE").await.unwrap();
    assert!(
        c.provider_capabilities().imap_idle,
        "非登录命令的 OK 响应同样要吸收"
    );
}

/// 反面：没有能力码的 OK 响应不能凭空造出能力。
#[tokio::test]
async fn ok_response_without_capability_code_changes_nothing() {
    let (mut c, _log) = open(&["A1 OK LOGIN completed"], &["IMAP4rev1"]);
    c.login("a@b.com", "x").await.unwrap();
    let pc = c.provider_capabilities();
    assert!(!pc.imap_idle && !pc.condstore);
}
