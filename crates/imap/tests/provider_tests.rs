//! S3-03：IMAP + SMTP Provider 适配层验证（脚本化通道，无网络依赖）。

use async_trait::async_trait;
use crossmail_core::error::{ErrorKind, MailResult};
use crossmail_core::model::{Address, Flag, OutgoingMessage, ProtocolKind};
use crossmail_core::provider::{CancellationToken, MailProvider, SyncRequest};
use crossmail_imap::capability::Capabilities;
use crossmail_imap::client::{local_email_id, ImapClient};
use crossmail_imap::provider::{ImapProvider, ImapProviderConfig, SmtpAuth, SmtpConnector};
use crossmail_imap::smtp::testing::MockSmtpIo;
use crossmail_imap::smtp::SmtpIo;
use crossmail_imap::transport::testing::MockIo;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

const ACC: &str = "acc";

fn config() -> ImapProviderConfig {
    ImapProviderConfig::new(ACC, "user@example.com", "user@example.com", "secret")
}

/// 把脚本里的 tagged 响应按**出现顺序**重新编号成 `A1`、`A2`……
///
/// 客户端会校验响应的 tag 与当前命令是否匹配，所以脚本里的 tag 必须对得上。
/// 但手写 `A1..An` 的代价是：以后往中间插一条命令，后面所有编号都要跟着改，
/// 改漏一个就是「响应错位 → 超时」这种看不懂的失败。统一重编号后，
/// 测试只需关心「这是第几个 tagged 响应」。
fn renumber_tags(script: &[&str]) -> Vec<String> {
    let mut n = 0u32;
    script
        .iter()
        .map(|line| match split_tag(line) {
            Some(rest) => {
                n += 1;
                format!("A{n} {rest}")
            }
            None => (*line).to_string(),
        })
        .collect()
}

/// `A12 OK ...` → `Some("OK ...")`；非 tagged 行（`* ...`、`+ ...`）→ `None`。
fn split_tag(line: &str) -> Option<&str> {
    let (tag, rest) = line.split_once(' ')?;
    let digits = tag.strip_prefix('A')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(rest)
}

fn provider(script: &[&str], caps: &[&str]) -> (ImapProvider, Arc<Mutex<Vec<String>>>) {
    let normalized = renumber_tags(script);
    let mock = MockIo::new(normalized);
    let log = mock.log_handle();
    let client = ImapClient::with_capabilities(
        Box::new(mock),
        Capabilities::from_items(caps.iter().copied()),
    );
    (ImapProvider::new(client, config()), log)
}

fn commands(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().map(|g| g.clone()).unwrap_or_default()
}

const SELECT_BLOCK: [&str; 5] = [
    "* 12 EXISTS",
    "* OK [UIDVALIDITY 42] UIDs valid",
    "* OK [UIDNEXT 40] Predicted next UID",
    "* OK [HIGHESTMODSEQ 1024] Highest modseq",
    "A1 OK [READ-WRITE] SELECT completed",
];

#[test]
fn email_id_roundtrip_and_validation() {
    let (p, _log) = provider(&[], &["IMAP4rev1"]);
    assert_eq!(p.protocol(), ProtocolKind::Imap);

    let id = local_email_id(ACC, "INBOX", 300);
    assert_eq!(p.parse_email_id(&id).unwrap(), ("INBOX".to_string(), 300));

    // 账号不匹配 / UID 非法都要明确报错，不能静默当成 0
    assert_eq!(
        p.parse_email_id("other/INBOX/12").unwrap_err().kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(
        p.parse_email_id(&format!("{ACC}/INBOX/abc"))
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[tokio::test]
async fn first_sync_fetches_all_and_remaps_ids() {
    let fetch = r#"* 1 FETCH (UID 300 FLAGS (\Seen) RFC822.SIZE 2048 INTERNALDATE "13-Nov-2026 10:00:00 +0800" ENVELOPE ("Fri, 13 Nov 2026 10:00:00 +0800" "季度报告" (("Alice" NIL "alice" "example.com")) NIL NIL (("Bob" NIL "bob" "example.com")) NIL NIL NIL "<m1@example.com>") BODYSTRUCTURE (("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" 100 3 NIL NIL NIL) "MIXED" ("BOUNDARY" "b1") NIL NIL NIL))"#;
    let mut script: Vec<&str> = SELECT_BLOCK.to_vec();
    script.push(fetch);
    script.push("A2 OK FETCH completed");

    let (p, log) = provider(&script, &["IMAP4rev1"]);
    let req = SyncRequest {
        account_id: ACC.into(),
        mailbox_id: Some("INBOX".into()),
        since_state: None,
        since_modseq: None,
        limit: 100,
        include_body: false,
    };
    let page = p
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .unwrap();

    assert_eq!(page.emails.len(), 1);
    assert_eq!(page.emails[0].id, local_email_id(ACC, "INBOX", 300));
    assert_eq!(page.emails[0].subject, "季度报告");
    assert!(!page.full_resync, "首次同步不是校准");
    let cursor: serde_json::Value =
        serde_json::from_str(page.new_state.as_deref().unwrap()).unwrap();
    assert_eq!(cursor["uid_validity"], 42);
    assert_eq!(cursor["last_uid"], 300);

    let cmds = commands(&log);
    assert!(
        cmds[1].contains("UID FETCH 1:*"),
        "首次同步应拉全量: {:?}",
        cmds
    );
}

#[tokio::test]
async fn up_to_date_cursor_skips_fetch() {
    let (p, log) = provider(&SELECT_BLOCK, &["IMAP4rev1"]);
    let req = SyncRequest {
        account_id: ACC.into(),
        mailbox_id: Some("INBOX".into()),
        since_state: Some(r#"{"uid_validity":42,"last_uid":39,"highest_modseq":null}"#.into()),
        since_modseq: None,
        limit: 100,
        include_body: false,
    };
    let page = p
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .unwrap();

    assert!(page.emails.is_empty());
    assert!(page.new_state.is_some(), "游标仍要回写，便于下次继续");
    let cmds = commands(&log);
    assert!(
        !cmds.iter().any(|c| c.contains("FETCH")),
        "无新邮件时不得发 FETCH: {cmds:?}"
    );
}

#[tokio::test]
async fn uidvalidity_change_requests_calibration() {
    let fetch = r#"* 1 FETCH (UID 7 FLAGS () RFC822.SIZE 10 INTERNALDATE "13-Nov-2026 10:00:00 +0800" ENVELOPE ("Fri, 13 Nov 2026 10:00:00 +0800" "新库" NIL NIL NIL NIL NIL NIL NIL) BODYSTRUCTURE ("TEXT" "PLAIN" ("CHARSET" "UTF-8") NIL NIL "7BIT" 10 1 NIL NIL NIL))"#;
    let mut script: Vec<&str> = SELECT_BLOCK.to_vec();
    script.push(fetch);
    script.push("A2 OK FETCH completed");

    let (p, _log) = provider(&script, &["IMAP4rev1"]);
    let req = SyncRequest {
        account_id: ACC.into(),
        mailbox_id: Some("INBOX".into()),
        // 旧库 UIDVALIDITY=1，服务端已变成 42
        since_state: Some(r#"{"uid_validity":1,"last_uid":9,"highest_modseq":null}"#.into()),
        since_modseq: None,
        limit: 100,
        include_body: false,
    };
    let page = p
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .unwrap();
    assert!(page.full_resync, "UIDVALIDITY 变化必须触发引擎校准");
    assert_eq!(page.emails.len(), 1);
}

#[tokio::test]
async fn sync_without_mailbox_is_rejected() {
    let (p, _log) = provider(&[], &[]);
    let req = SyncRequest::new(ACC);
    let err = p
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[tokio::test]
async fn set_flags_issues_store_command() {
    let mut script: Vec<&str> = SELECT_BLOCK.to_vec();
    script.push("A2 OK STORE completed");

    let (p, log) = provider(&script, &["IMAP4rev1"]);
    let id = local_email_id(ACC, "INBOX", 300);
    p.set_flags(&[id], Flag::Seen, true).await.unwrap();

    let cmds = commands(&log);
    assert!(cmds[0].contains("SELECT \"INBOX\""));
    assert!(
        cmds[1].contains("UID STORE 300 +FLAGS.SILENT (\\Seen)"),
        "实际命令: {}",
        cmds[1]
    );
}

#[tokio::test]
async fn move_uses_move_extension_when_available() {
    let mut script: Vec<&str> = SELECT_BLOCK.to_vec();
    script.push("* OK [COPYUID 42 300 900] moved");
    script.push("A2 OK MOVE completed");

    let (p, log) = provider(&script, &["IMAP4rev1", "MOVE"]);
    let id = local_email_id(ACC, "INBOX", 300);
    p.move_emails(&[id], &"INBOX".to_string(), &"Archive".to_string())
        .await
        .unwrap();

    let cmds = commands(&log);
    assert!(
        cmds[1].contains("UID MOVE 300 \"Archive\""),
        "实际命令: {}",
        cmds[1]
    );
}

/// 普通文件夹里删除 = 移到回收站（MOVE 优先）。
#[tokio::test]
async fn delete_from_inbox_moves_to_trash() {
    let script = vec![
        r#"* LIST (\Trash) "/" "Trash""#,
        "A1 OK LIST completed",
        // SELECT INBOX
        "* 12 EXISTS",
        "* OK [UIDVALIDITY 42] UIDs valid",
        "* OK [UIDNEXT 40] Predicted next UID",
        "A2 OK [READ-WRITE] SELECT completed",
        // 写操作前的存在性校验（真机对不存在的 UID 回 OK，不校验就是静默空转）
        "* 1 FETCH (UID 300)",
        "A9 OK FETCH completed",
        // UID MOVE
        "* OK [COPYUID 42 300 900] moved",
        "A9 OK MOVE completed",
    ];

    let (p, log) = provider(&script, &["IMAP4rev1", "MOVE", "UIDPLUS"]);
    let id = local_email_id(ACC, "INBOX", 300);
    p.delete_emails(std::slice::from_ref(&id)).await.unwrap();

    let cmds = commands(&log);
    let move_cmd = cmds
        .iter()
        .find(|c| c.contains("MOVE"))
        .expect("没有发 MOVE");
    assert!(
        move_cmd.contains("UID MOVE 300 \"Trash\""),
        "实际命令: {move_cmd}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("EXPUNGE")),
        "移入回收站不该顺手 EXPUNGE: {cmds:?}"
    );
}

/// 已在回收站里再删 = 彻底删除。
///
/// 这里曾经有真实缺陷：代码调用 `move_messages(uids, trash)`——把邮件 MOVE 到
/// 它此刻所在的同一个文件夹。服务端要么回 OK 什么都不做，要么直接报错，
/// 用户侧表现就是「点了彻底删除，邮件还在回收站里」。
#[tokio::test]
async fn delete_inside_trash_expunges_instead_of_self_move() {
    let script = vec![
        r#"* LIST (\Trash) "/" "Trash""#,
        "A1 OK LIST completed",
        "* 12 EXISTS",
        "* OK [UIDVALIDITY 42] UIDs valid",
        "A2 OK [READ-WRITE] SELECT completed",
        "* 1 FETCH (UID 500)",
        "A9 OK FETCH completed",
        "A9 OK STORE completed",
        "* 12 EXPUNGE",
        "A9 OK EXPUNGE completed",
    ];

    let (p, log) = provider(&script, &["IMAP4rev1", "MOVE", "UIDPLUS"]);
    let id = local_email_id(ACC, "Trash", 500);
    p.delete_emails(std::slice::from_ref(&id)).await.unwrap();

    let cmds = commands(&log);
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("MOVE") || c.contains("COPY")),
        "同一文件夹内的「移动」是空操作，必须走物理删除: {cmds:?}"
    );
    assert!(
        cmds.iter()
            .any(|c| c.contains("UID STORE 500 +FLAGS.SILENT (\\Deleted)")),
        "缺 \\Deleted 标记: {cmds:?}"
    );
    assert!(
        cmds.iter().any(|c| c.contains("UID EXPUNGE 500")),
        "缺精确 EXPUNGE: {cmds:?}"
    );
}

/// 无 UIDPLUS 时回收站内删除退化成整箱 EXPUNGE，必须真的发出去而不是静默跳过。
#[tokio::test]
async fn delete_inside_trash_without_uidplus_uses_plain_expunge() {
    let script = vec![
        r#"* LIST (\Trash) "/" "Trash""#,
        "A1 OK LIST completed",
        "* 12 EXISTS",
        "A2 OK [READ-WRITE] SELECT completed",
        "* 1 FETCH (UID 501)",
        "A9 OK FETCH completed",
        "A9 OK STORE completed",
        "* 12 EXPUNGE",
        "A9 OK EXPUNGE completed",
    ];

    let (p, log) = provider(&script, &["IMAP4rev1"]);
    let id = local_email_id(ACC, "Trash", 501);
    p.delete_emails(std::slice::from_ref(&id)).await.unwrap();

    let cmds = commands(&log);
    assert!(
        cmds.iter().any(|c| c.ends_with("EXPUNGE")),
        "应退化为裸 EXPUNGE: {cmds:?}"
    );
    assert!(
        !cmds.iter().any(|c| c.contains("UID EXPUNGE")),
        "无 UIDPLUS 发 UID EXPUNGE 会被服务端拒: {cmds:?}"
    );
}

/// 移动的源文件夹必须以 `from` 参数为准，而不是 id 里内嵌的那个。
///
/// 本地主键 `账号/文件夹/UID` 是「上次同步时」的位置快照；邮件移动过一次之后就过期了。
/// 拿过期快照当源，就会出现「源＝目标」的同箱移动。真服务器（Stalwart）会回
/// `[CANNOT] Source and destination mailboxes are the same.`，被回放层判成协议错误不再重试。
#[tokio::test]
async fn move_uses_from_parameter_when_id_mailbox_is_stale() {
    let script = vec![
        "* 3 EXISTS",
        "A1 OK [READ-WRITE] SELECT completed",
        "* OK [COPYUID 42 300 900] moved",
        "A2 OK MOVE completed",
    ];

    let (p, log) = provider(&script, &["IMAP4rev1", "MOVE", "UIDPLUS"]);
    // id 里写的是 INBOX（旧位置），实际邮件已在 Archive（from）
    let id = local_email_id(ACC, "INBOX", 300);
    p.move_emails(&[id], &"Archive".to_string(), &"Trash".to_string())
        .await
        .unwrap();

    let cmds = commands(&log);
    assert!(
        cmds[0].contains("SELECT \"Archive\""),
        "应 SELECT from 指定的源文件夹，实际: {}",
        cmds[0]
    );
    assert!(
        cmds[1].contains("UID MOVE 300 \"Trash\""),
        "实际命令: {}",
        cmds[1]
    );
}

#[tokio::test]
async fn blob_operations_are_explicitly_unsupported() {
    let (p, _log) = provider(&[], &["IMAP4rev1"]);
    assert_eq!(
        p.upload_blob(b"x", "text/plain").await.unwrap_err().kind(),
        ErrorKind::Unsupported
    );
    assert_eq!(
        p.download_blob("1").await.unwrap_err().kind(),
        ErrorKind::Unsupported
    );
}

// ---------------------------------------------------------------- SMTP 发信

#[derive(Default)]
struct MockSmtpConnector {
    script: Mutex<VecDeque<String>>,
    /// 每次连接的通道命令日志（共享同一个 Arc，连接建立后写入的内容也能读到）
    logs: Mutex<Vec<Arc<Mutex<Vec<String>>>>>,
}

impl MockSmtpConnector {
    fn new(lines: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(lines.iter().map(|s| s.to_string()).collect()),
            logs: Mutex::new(Vec::new()),
        })
    }

    fn commands(&self) -> Vec<String> {
        self.logs
            .lock()
            .map(|handles| {
                handles
                    .iter()
                    .filter_map(|h| h.lock().ok().map(|g| g.clone()))
                    .flatten()
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// 包装：连接时把剧本交给 MockSmtpIo，并共享命令日志。
struct ConnectorHandle(Arc<MockSmtpConnector>);

#[async_trait]
impl SmtpConnector for ConnectorHandle {
    async fn connect(&self) -> MailResult<Box<dyn SmtpIo>> {
        let lines: Vec<String> = self
            .0
            .script
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        let mock = MockSmtpIo::new(lines);
        if let Ok(mut logs) = self.0.logs.lock() {
            logs.push(mock.log_handle());
        }
        Ok(Box::new(mock))
    }
}

fn sent_message() -> OutgoingMessage {
    let mut msg = OutgoingMessage::new(
        ACC,
        Address::new("user@example.com"),
        vec![Address::new("bob@example.com")],
        "季度汇报",
    );
    msg.text_body = Some("正文".into());
    msg
}

#[tokio::test]
async fn send_email_without_smtp_is_unsupported() {
    let (p, _log) = provider(&[], &["IMAP4rev1"]);
    let err = p.send_email(sent_message()).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}

#[tokio::test]
async fn send_email_runs_full_smtp_transaction() {
    let connector = MockSmtpConnector::new(&[
        "220 mock ESMTP ready",
        "250-mock greets you",
        "250-AUTH PLAIN LOGIN",
        "250 SIZE 10485760",
        "235 2.7.0 authentication successful",
        "250 2.1.0 sender ok",
        "250 2.1.5 recipient ok",
        "354 end with <CRLF>.<CRLF>",
        "250 2.0.0 queued as ABC123",
        "221 bye",
    ]);
    let (p, _imap_log) = provider(&[], &["IMAP4rev1"]);
    let p = p.with_smtp(SmtpAuth {
        connector: Box::new(ConnectorHandle(Arc::clone(&connector))),
        username: "user@example.com".into(),
        secret: "secret".into(),
    });

    let receipt = p.send_email(sent_message()).await.unwrap();
    assert!(
        receipt.message_id.starts_with('<') && receipt.message_id.ends_with('>'),
        "回执要带真实报文里的 Message-ID: {}",
        receipt.message_id
    );

    let cmds = connector.commands();
    assert!(cmds.iter().any(|c| c.starts_with("AUTH PLAIN")), "{cmds:?}");
    assert!(cmds
        .iter()
        .any(|c| c.contains("MAIL FROM:<user@example.com>")));
    assert!(cmds.iter().any(|c| c.contains("RCPT TO:<bob@example.com>")));
    assert!(cmds.iter().any(|c| c == "DATA"));
    assert_eq!(cmds.last().map(String::as_str), Some("QUIT"));
}

#[tokio::test]
async fn send_email_rejects_invalid_message_before_connecting() {
    let connector = MockSmtpConnector::new(&["220 mock ESMTP ready"]);
    let (p, _log) = provider(&[], &["IMAP4rev1"]);
    let p = p.with_smtp(SmtpAuth {
        connector: Box::new(ConnectorHandle(Arc::clone(&connector))),
        username: "user@example.com".into(),
        secret: "secret".into(),
    });

    let mut bad = sent_message();
    bad.to.clear();
    let err = p.send_email(bad).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
    assert_eq!(
        connector.script.lock().map(|q| q.len()).unwrap_or(0),
        1,
        "本地校验失败时不该建立 SMTP 连接（剧本未被消费）"
    );
}

#[tokio::test]
async fn capabilities_reflect_imap_extensions() {
    let (p, _log) = provider(
        &["IMAP4rev1"],
        &["IMAP4rev1", "UIDPLUS", "CONDSTORE", "MOVE"],
    );
    let caps = p.capabilities();
    assert!(caps.uidplus);
    assert!(caps.condstore);
    assert!(caps.imap_move);
    assert!(!caps.jmap_mail, "IMAP 账号不得声明 JMAP 能力");
}

// ------------------------------------------------- 陈旧主键导致的「静默空转」

/// 真机事实：对**不存在的 UID** 执行 `UID MOVE` / `UID COPY` / `UID STORE` 一律回
/// `OK`（`OK No messages were moved.` / `OK UID STORE completed`），服务器上什么都没发生。
///
/// 于是「主键里的文件夹已过期」这种常见情况（别的客户端把邮件移走、服务器 MOVE 换了 UID
/// 但没给 COPYUID）会表现为：队列清空、零报错、回放报告一片绿，用户的邮件纹丝不动。
/// 这里锁住的是：一封都不存在时必须显式失败，绝不能返回成功。
#[tokio::test]
async fn delete_refuses_when_none_of_the_uids_exist() {
    let script = vec![
        r#"* LIST (\Trash) "/" "Trash""#,
        "A1 OK LIST completed",
        "* 12 EXISTS",
        "A2 OK [READ-WRITE] SELECT completed",
        // 校验：UID 300 不在这个文件夹里（没有任何 untagged FETCH）
        "A3 OK FETCH completed",
    ];

    let (p, log) = provider(&script, &["IMAP4rev1", "MOVE", "UIDPLUS"]);
    let id = local_email_id(ACC, "INBOX", 300);
    let err = p
        .delete_emails(std::slice::from_ref(&id))
        .await
        .expect_err("一封都不存在时必须报错，不能静默成功");
    assert_eq!(err.kind(), ErrorKind::NotFound);
    assert!(
        err.to_string().contains("INBOX/300"),
        "错误里要能看出是哪封邮件：{err}"
    );

    let cmds = commands(&log);
    assert!(
        !cmds
            .iter()
            .any(|c| c.contains("MOVE") || c.contains("COPY")),
        "既然一封都不存在，就不该再发写命令: {cmds:?}"
    );
}

/// 复制同理：`OK No messages were copied.` 是成功响应，但目标文件夹里什么都没有。
#[tokio::test]
async fn copy_refuses_when_none_of_the_uids_exist() {
    let script = vec![
        "* 12 EXISTS",
        "A1 OK [READ-WRITE] SELECT completed",
        "A2 OK FETCH completed",
    ];

    let (p, log) = provider(&script, &["IMAP4rev1", "UIDPLUS"]);
    let id = local_email_id(ACC, "INBOX", 41);
    let err = p
        .copy_emails(std::slice::from_ref(&id), &"Archive".to_string())
        .await
        .expect_err("一封都没复制成时必须报错");
    assert_eq!(err.kind(), ErrorKind::NotFound);

    let cmds = commands(&log);
    assert!(!cmds.iter().any(|c| c.contains("COPY")), "{cmds:?}");
}

/// 部分存在时的语义：能删的照删（别因为一封失效就整批放弃），
/// 但**只对存在的那些**发写命令——拿不存在的 UID 去 STORE/MOVE 就是在制造假成功。
#[tokio::test]
async fn delete_only_touches_the_uids_that_exist() {
    let script = vec![
        r#"* LIST (\Trash) "/" "Trash""#,
        "A1 OK LIST completed",
        "* 12 EXISTS",
        "* OK [UIDVALIDITY 42] UIDs valid",
        "A2 OK [READ-WRITE] SELECT completed",
        // 300 在、301 不在
        "* 1 FETCH (UID 300)",
        "A3 OK FETCH completed",
        "* OK [COPYUID 42 300 900] moved",
        "A4 OK MOVE completed",
    ];

    let (p, log) = provider(&script, &["IMAP4rev1", "MOVE", "UIDPLUS"]);
    let ids = vec![
        local_email_id(ACC, "INBOX", 300),
        local_email_id(ACC, "INBOX", 301),
    ];
    p.delete_emails(&ids).await.expect("有一封存在就应继续");

    let cmds = commands(&log);
    let move_cmd = cmds.iter().find(|c| c.contains("MOVE")).expect("缺 MOVE");
    assert!(
        move_cmd.contains("UID MOVE 300 ") && !move_cmd.contains("301"),
        "只该移动真实存在的 UID: {move_cmd}"
    );
}

/// 发信成功必须把同一份报文 APPEND 进服务端「已发送」。
///
/// 少了这一步的直接后果：手机端和其他客户端看不到自己发过的邮件，
/// 用户以为没发出去，于是再发一遍。IMAP 的 SMTP 提交与邮箱状态是两条独立链路，
/// 服务器不会替你归档。
#[tokio::test]
async fn send_email_appends_copy_to_server_sent() {
    let script = [
        r#"* LIST (\HasNoChildren \Sent) "/" "Sent Items""#,
        "A1 OK LIST completed",
        "+ Ready for literal data",
        "A2 OK [APPENDUID 42 700] APPEND completed",
    ];
    let (p, imap_log) = provider(&script, &["IMAP4rev1", "UIDPLUS"]);
    let connector = MockSmtpConnector::new(&[
        "220 mock ESMTP ready",
        "250-mock greets you",
        "250-AUTH PLAIN LOGIN",
        "250 SIZE 10485760",
        "235 2.7.0 authentication successful",
        "250 2.1.0 sender ok",
        "250 2.1.5 recipient ok",
        "354 end with <CRLF>.<CRLF>",
        "250 2.0.0 queued as ABC123",
        "221 bye",
    ]);
    let p = p.with_smtp(SmtpAuth {
        connector: Box::new(ConnectorHandle(Arc::clone(&connector))),
        username: "user@example.com".into(),
        secret: "secret".into(),
    });

    let receipt = p.send_email(sent_message()).await.unwrap();
    assert_eq!(
        receipt.sent_mailbox_id.as_deref(),
        Some("Sent Items"),
        "回执必须写明归档到哪个文件夹，编排层要靠它对齐本地主键"
    );
    assert_eq!(
        receipt.server_id.as_deref(),
        Some("700"),
        "APPENDUID 的 UID 要回填，否则本地无法与同步身份对齐"
    );

    let cmds = commands(&imap_log);
    let append = cmds
        .iter()
        .find(|c| c.contains("APPEND"))
        .unwrap_or_else(|| panic!("没有 APPEND：{cmds:?}"));
    assert!(append.contains("\"Sent Items\""), "{append}");
    // 自己发的邮件显示成未读没有意义，必须带 \Seen
    assert!(append.contains("\\Seen"), "{append}");
}

/// 服务端没有「已发送」文件夹 / APPEND 失败时，发信仍然算成功。
///
/// 这条路径的关键不是「能不能归档」，而是**绝不能返回 Err**：SMTP 已经 QUIT，
/// 邮件真的发出去了，报错会让用户重发，对方收到两封。
#[tokio::test]
async fn send_email_survives_missing_sent_folder() {
    let script = [
        r#"* LIST (\HasNoChildren) "/" "INBOX""#,
        "A1 OK LIST completed",
    ];
    let (p, imap_log) = provider(&script, &["IMAP4rev1"]);
    let connector = MockSmtpConnector::new(&[
        "220 mock ESMTP ready",
        "250-mock greets you",
        "250-AUTH PLAIN LOGIN",
        "250 SIZE 10485760",
        "235 2.7.0 authentication successful",
        "250 2.1.0 sender ok",
        "250 2.1.5 recipient ok",
        "354 end with <CRLF>.<CRLF>",
        "250 2.0.0 queued as ABC123",
        "221 bye",
    ]);
    let p = p.with_smtp(SmtpAuth {
        connector: Box::new(ConnectorHandle(Arc::clone(&connector))),
        username: "user@example.com".into(),
        secret: "secret".into(),
    });

    let receipt = p.send_email(sent_message()).await.unwrap();
    assert!(
        receipt.sent_mailbox_id.is_none(),
        "没有已发送文件夹就不该谎报归档成功"
    );
    assert_eq!(receipt.server_id, None);
    // 邮件确实发出去了：SMTP 事务走完才轮到归档
    assert_eq!(
        connector.commands().last().map(String::as_str),
        Some("QUIT")
    );
    assert!(
        !commands(&imap_log).iter().any(|c| c.contains("APPEND")),
        "没有目标文件夹时不该发 APPEND"
    );
}
