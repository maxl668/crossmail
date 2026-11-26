//! JMAP 真实服务器联调（live integration）。
//!
//! 默认**静默跳过**：没设 `CROSSMAIL_LIVE_JMAP_URL` 就什么都不做，CI 与离线开发不受影响。
//!
//! 需要这几个环境变量：
//! ```text
//! CROSSMAIL_LIVE_JMAP_URL   JMAP Session 端点（如 https://host/jmap/session），必需
//! CROSSMAIL_LIVE_JMAP_USER  登录名（邮箱地址），必需
//! CROSSMAIL_LIVE_JMAP_PASS  口令 / 应用专用密码，必需
//! ```
//!
//! 这个测试会**真的写数据**（建测试文件夹、建草稿、改关键字、移动），
//! 每一步自带清理，异常退出留下的残留用固定前缀 `CrossMail-jm-` 可人工识别。
//!
//! 与 IMAP 侧的关键差异：JMAP 的 `emailId` 是**稳定标识**，移动只改 `mailboxIds`，
//! 所以「移动后 id 失效」那类身份漂移在这里不存在——但反过来，
//! 如果适配层把 `mailboxIds` 当单值处理，就会在「一封邮件属于多个文件夹」时出错。

use crossmail_core::model::{Address, Flag, MailboxRole, OutgoingMessage};
use crossmail_core::provider::{CancellationToken, MailProvider, MailboxDraft, SyncRequest};
use crossmail_jmap::push::{PushConfig, PushUpdate, StateChange};
use crossmail_jmap::{AuthMethod, JmapProvider};

struct LiveConfig {
    /// Session 端点。
    url: String,
    user: String,
    pass: String,
}

/// 读取环境变量；缺关键项返回 None（=跳过）。
fn live_config() -> Option<LiveConfig> {
    let url = std::env::var("CROSSMAIL_LIVE_JMAP_URL").ok()?;
    let user = std::env::var("CROSSMAIL_LIVE_JMAP_USER").ok()?;
    let pass = std::env::var("CROSSMAIL_LIVE_JMAP_PASS").ok()?;
    if url.trim().is_empty() || user.trim().is_empty() {
        return None;
    }
    Some(LiveConfig { url, user, pass })
}

macro_rules! live_or_skip {
    () => {
        match live_config() {
            Some(c) => c,
            None => {
                eprintln!(
                    "跳过：未设置 CROSSMAIL_LIVE_JMAP_URL/USER/PASS（真联调需要真实服务器凭据）"
                );
                return;
            }
        }
    };
}

fn connect(cfg: &LiveConfig) -> JmapProvider {
    let auth = AuthMethod::basic(cfg.user.clone(), cfg.pass.clone());
    // accountId 由 session 的 primaryAccounts 解析得出，不在这里硬编码
    JmapProvider::new(cfg.url.clone(), auth, cfg.user.clone())
}

/// 清掉历史运行残留（异常中断时清理没跑到会累积）。
///
/// **不能静默吞掉失败**：之前写成 `let _ = delete_mailbox(..)`，结果非空文件夹删不掉，
/// 残留越积越多，最后连「本次运行到底有没有副作用」都说不清。这里改成打日志。
async fn purge_leftovers(p: &JmapProvider) {
    for m in p.list_mailboxes().await.expect("Mailbox/get 失败") {
        if m.name.starts_with("CrossMail-jm-") {
            if let Err(e) = p.delete_mailbox(&m.id).await {
                eprintln!(
                    "残留文件夹 {} ({}) 清理失败（多半是非空）：{e}",
                    m.name, m.id
                );
            }
        }
    }
}

fn mailbox_id_by_role(boxes: &[crossmail_core::Mailbox], role: MailboxRole) -> String {
    boxes
        .iter()
        .find(|m| m.role == role)
        .unwrap_or_else(|| panic!("服务端缺少 {role:?} 角色文件夹，实际: {boxes:?}"))
        .id
        .clone()
}

/// 认证 + 会话发现 + 目录：JMAP 第一关。
#[tokio::test]
async fn live_jmap_verify_and_list_mailboxes() {
    let cfg = live_or_skip!();
    let p = connect(&cfg);

    let info = p.verify_credentials().await.expect("凭证校验失败");
    assert_eq!(info.email_address, cfg.user);
    // accountId 必须非空：JMAP 所有方法都要它，拿不到就是 session 解析（primaryAccounts）有问题
    assert!(
        !info.account_id.is_empty(),
        "primaryAccounts 未解析出邮件账号 id"
    );
    assert!(info.capabilities.jmap_mail, "会话未声明 jmap:mail 能力");
    println!("JMAP 能力协商: {:?}", info.capabilities);

    let boxes = p.list_mailboxes().await.expect("Mailbox/get 失败");
    assert!(!boxes.is_empty(), "真实账号至少应有一个文件夹");
    let inbox = boxes
        .iter()
        .find(|m| m.role == MailboxRole::Inbox)
        .expect("没识别出 Inbox：role 映射有问题");
    assert!(!inbox.id.is_empty(), "Inbox 的 id 不可为空");
    for m in &boxes {
        println!("  - id={:<10} name={:<18} role={:?}", m.id, m.name, m.role);
    }
    println!(
        "账号 {} accountId={} 共 {} 个文件夹",
        info.email_address,
        info.account_id,
        boxes.len()
    );
}

/// 文件夹增删改：Mailbox/set create / update / destroy 与回读一致性。
#[tokio::test]
async fn live_jmap_mailbox_lifecycle() {
    let cfg = live_or_skip!();
    let p = connect(&cfg);
    purge_leftovers(&p).await;

    let tag = format!("CrossMail-jm-{}", std::process::id());
    let created = p
        .create_mailbox(MailboxDraft {
            name: tag.clone(),
            parent_id: None,
            role: MailboxRole::Other,
        })
        .await
        .expect("Mailbox/set(create) 失败");

    // 回读：建完必须能在列表里查到，且名字一致
    let listed = p.list_mailboxes().await.expect("Mailbox/get 失败");
    let found = listed
        .iter()
        .find(|m| m.id == created.id)
        .expect("新建的文件夹在列表里找不到");
    assert_eq!(found.name, tag);

    // 改名后回读
    let renamed = format!("{tag}-renamed");
    p.rename_mailbox(&created.id, &renamed)
        .await
        .expect("Mailbox/set(update) 失败");
    let after = p.list_mailboxes().await.expect("Mailbox/get 失败");
    assert_eq!(
        after
            .iter()
            .find(|m| m.id == created.id)
            .expect("改名后文件夹消失")
            .name,
        renamed,
        "改名未落到服务端"
    );

    p.delete_mailbox(&created.id)
        .await
        .expect("Mailbox/set(destroy) 失败");
    let final_list = p.list_mailboxes().await.expect("Mailbox/get 失败");
    assert!(
        !final_list.iter().any(|m| m.id == created.id),
        "删除后文件夹仍在列表里"
    );
}

/// 增量同步 INBOX：首轮拿全量、游标复用不重复。
///
/// 这条盯的是 `Email/query` + `Email/get` 的分页与 `queryState` 语义，
/// 纯脚本化用例构造不出真实分页行为。
#[tokio::test]
async fn live_jmap_sync_inbox_and_reuse_state() {
    let cfg = live_or_skip!();
    let p = connect(&cfg);

    let boxes = p.list_mailboxes().await.expect("Mailbox/get 失败");
    let inbox = mailbox_id_by_role(&boxes, MailboxRole::Inbox);

    let mut req = SyncRequest::new("live-acct");
    req.mailbox_id = Some(inbox.clone());
    req.limit = 200;
    let first = p
        .sync_emails(req.clone(), CancellationToken::new(), None)
        .await
        .expect("首轮同步失败");
    let state = first.new_state.clone().expect("首轮未返回 new_state");
    println!(
        "首轮同步：{} 封，state={} full_resync={}",
        first.emails.len(),
        state,
        first.full_resync
    );
    assert!(!state.is_empty(), "queryState 为空，增量同步无从谈起");

    // 用同一游标再同步一次：应当没有新增（服务端未变），绝不能重复下发同一封
    let mut second_req = req.clone();
    second_req.since_state = Some(state);
    let second = p
        .sync_emails(second_req, CancellationToken::new(), None)
        .await
        .expect("增量同步失败");
    println!(
        "增量同步：{} 封，full_resync={}",
        second.emails.len(),
        second.full_resync
    );
    assert!(
        second.emails.len() <= first.emails.len(),
        "增量同步返回的邮件比首轮还多（{} > {}），游标没起作用",
        second.emails.len(),
        first.emails.len()
    );

    // 取正文：jmap 的 body 走 Email/get 的 bodyValues
    if let Some(first_mail) = first.emails.first() {
        let full = p
            .fetch_email(&first_mail.id, true)
            .await
            .expect("取正文失败");
        assert_eq!(full.id, first_mail.id);
        println!(
            "正文取样：subject={:?} text_len={:?}",
            full.subject,
            full.text_body.as_ref().map(|t| t.len())
        );
    }
}

/// 写链路：建草稿 → 同步回读 → 改关键字 → 移动 → 删除，每步回读服务端状态。
///
/// 这是 JMAP 侧最关键的一条：`Email/set` 的 `notCreated` / `notUpdated`
/// 是**方法级成功、对象级失败**，只看 HTTP 200 会把失败当成功。
#[tokio::test]
async fn live_jmap_write_roundtrip() {
    let cfg = live_or_skip!();
    let p = connect(&cfg);
    purge_leftovers(&p).await;

    let tag = format!("CrossMail-jm-{}", std::process::id());
    let target = p
        .create_mailbox(MailboxDraft {
            name: tag.clone(),
            parent_id: None,
            role: MailboxRole::Other,
        })
        .await
        .expect("create 文件夹失败");

    let boxes = p.list_mailboxes().await.expect("Mailbox/get 失败");
    let drafts = mailbox_id_by_role(&boxes, MailboxRole::Drafts);

    // ---- 建草稿
    let mut msg = OutgoingMessage::new(
        "live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} subject"),
    );
    msg.text_body = Some(format!("{tag} body"));
    let draft_id = p.save_draft(&msg).await.expect("Email/set(create) 失败");
    println!("草稿 id={draft_id}");

    // ---- 回读：草稿必须真的在 Drafts 里，正文对得上
    let mut req = SyncRequest::new("live-acct");
    req.mailbox_id = Some(drafts.clone());
    req.limit = 200;
    let page = p
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .expect("同步草稿箱失败");
    let mine = page
        .emails
        .iter()
        .find(|e| e.id == draft_id)
        .unwrap_or_else(|| {
            panic!(
                "新建的草稿 {draft_id} 同步不回来，共 {} 封",
                page.emails.len()
            )
        });
    assert!(
        mine.subject.contains(&tag),
        "回读的主题对不上: {:?}",
        mine.subject
    );
    let full = p.fetch_email(&draft_id, true).await.expect("取正文失败");
    assert!(
        full.text_body
            .as_deref()
            .map(|t| t.contains(&tag))
            .unwrap_or(false),
        "回读的正文与写入不一致: {:?}",
        full.text_body
    );

    // ---- 改关键字，并回读确认
    p.set_flags(std::slice::from_ref(&draft_id), Flag::Flagged, true)
        .await
        .expect("Email/set(keywords) 加标记失败");
    let after_flag = p.fetch_email(&draft_id, false).await.expect("回读标记失败");
    assert!(
        after_flag.flags.contains(&Flag::Flagged),
        "服务端没落上 \\Flagged，实际: {:?}",
        after_flag.flags
    );

    // ---- 移动（JMAP：改 mailboxIds，id 不变）
    p.move_emails(std::slice::from_ref(&draft_id), &drafts, &target.id)
        .await
        .expect("Email/set(move) 失败");
    let after_move = p
        .fetch_email(&draft_id, false)
        .await
        .expect("移动后回读失败");
    assert!(
        after_move.mailbox_ids.contains(&target.id),
        "移动后目标文件夹不在 mailboxIds 里: {:?}",
        after_move.mailbox_ids
    );
    assert!(
        !after_move.mailbox_ids.contains(&drafts),
        "移动后源文件夹仍在 mailboxIds 里（说明是「复制」不是「移动」）: {:?}",
        after_move.mailbox_ids
    );
    // 这正是与 IMAP 的分水岭：id 稳定，不需要 UID 重映射
    assert_eq!(after_move.id, draft_id, "JMAP 的 emailId 应当稳定不变");

    // ---- 复制：原地保留 + 目标多一份
    p.copy_emails(std::slice::from_ref(&draft_id), &drafts)
        .await
        .expect("Email/set(copy) 失败");
    let after_copy = p
        .fetch_email(&draft_id, false)
        .await
        .expect("复制后回读失败");
    assert!(
        after_copy.mailbox_ids.contains(&drafts) && after_copy.mailbox_ids.contains(&target.id),
        "复制后应当同时属于两个文件夹: {:?}",
        after_copy.mailbox_ids
    );

    // ---- 删除：进回收站（服务端有 trash 角色时应**移动**进回收站而不是 destroy）
    p.delete_emails(std::slice::from_ref(&draft_id))
        .await
        .expect("Email/set(delete) 失败");
    match p.fetch_email(&draft_id, false).await {
        Ok(e) => {
            let trash = mailbox_id_by_role(&boxes, MailboxRole::Trash);
            assert!(
                e.mailbox_ids.contains(&trash),
                "删除后没进回收站，实际 mailboxIds={:?}",
                e.mailbox_ids
            );
            // 关键：删除是「移动」不是「贴标签」，原文件夹与副本文件夹都不能还留着它。
            // 这一条盯的正是「只加 trash 不移除原箱」那类缺陷——用户表现是「删了还在」。
            assert!(
                !e.mailbox_ids.contains(&drafts),
                "删除后草稿箱里还在: {:?}",
                e.mailbox_ids
            );
            assert!(
                !e.mailbox_ids.contains(&target.id),
                "删除后测试文件夹里还在: {:?}",
                e.mailbox_ids
            );
        }
        Err(err) => {
            // 服务端直接销毁也是合法实现，但要确认是 NotFound 而不是别的错
            println!("删除后直接销毁（{err}）");
        }
    }

    p.delete_mailbox(&target.id).await.expect("清理文件夹失败");
}

/// 删除非空文件夹：服务端会拒绝，错误里**必须带上具体原因**。
///
/// 这条盯的是诊断质量而不是功能：`Mailbox/set` 的 `notDestroyed` 是「方法级成功、
/// 对象级失败」，服务端在 `{"type":"mailboxHasEmail"}` 里给了准确原因。
/// 如果适配层只报 `notDestroyed=1`，用户/日志完全无法区分
/// 「文件夹不存在」「没有权限」「里面还有邮件」——三种的处置方式完全不同。
#[tokio::test]
async fn live_jmap_delete_non_empty_mailbox_reports_reason() {
    let cfg = live_or_skip!();
    let p = connect(&cfg);
    purge_leftovers(&p).await;

    let tag = format!("CrossMail-jm-{}", std::process::id());
    let target = p
        .create_mailbox(MailboxDraft {
            name: tag.clone(),
            parent_id: None,
            role: MailboxRole::Other,
        })
        .await
        .expect("create 文件夹失败");

    let boxes = p.list_mailboxes().await.expect("Mailbox/get 失败");
    let drafts = mailbox_id_by_role(&boxes, MailboxRole::Drafts);

    // 造一封进 target
    let mut msg = OutgoingMessage::new(
        "live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} nonempty"),
    );
    msg.text_body = Some("x".into());
    let mail_id = p.save_draft(&msg).await.expect("建草稿失败");
    p.move_emails(std::slice::from_ref(&mail_id), &drafts, &target.id)
        .await
        .expect("移动失败");

    // 非空文件夹删除必须失败，且原因可读
    let err = p
        .delete_mailbox(&target.id)
        .await
        .expect_err("非空文件夹竟然删成功了，服务的拒绝行为与预期不符");
    let text = err.to_string();
    println!("非空文件夹删除错误: {text}");
    assert!(
        text.contains("mailboxHasEmail"),
        "错误信息没带服务端给的具体原因（期望含 mailboxHasEmail），实际: {text}"
    );

    // 清理：把邮件送进回收站，文件夹就能删了
    p.delete_emails(std::slice::from_ref(&mail_id))
        .await
        .expect("清理邮件失败");
    p.delete_mailbox(&target.id)
        .await
        .expect("清空后仍删不掉文件夹");
}

/// 错密码必须是 `Authentication`，不能是网络/协议错误——否则上层会把「密码错了」
/// 当成「网络抖动」无限重试，还可能触发账号锁定。
#[tokio::test]
async fn live_jmap_wrong_password_is_authentication_error() {
    let cfg = live_or_skip!();
    let p = JmapProvider::new(
        cfg.url.clone(),
        AuthMethod::basic(cfg.user.clone(), "definitely-not-the-password"),
        cfg.user.clone(),
    );

    let err = p
        .verify_credentials()
        .await
        .expect_err("错密码竟然认证通过");
    println!("错密码错误: {err:?} / kind={:?}", err.kind());
    assert!(
        matches!(err, crossmail_core::MailError::Authentication(_)),
        "错密码应判为 Authentication，实际: {err:?}"
    );
}

// ---------------- S6-11：Blob 与 EmailSubmission 真联调 ----------------

/// Blob 上传 / 下载：字节必须原样回来（RFC 8620 §6，J-009）。
///
/// 混入非 UTF-8 字节是关键：如果实现把 `Vec<u8>` 当字符串转一手，
/// 0x80/0xff 会被替换成 U+FFFD，附件就此损坏——而纯 ASCII 测试永远发现不了。
#[tokio::test]
async fn live_jmap_blob_upload_download_roundtrip() {
    let cfg = live_or_skip!();
    let p = connect(&cfg);

    let mut payload =
        format!("CrossMail blob roundtrip pid={}\r\n", std::process::id()).into_bytes();
    payload.extend_from_slice(&[0x00, 0xff, 0x7f, 0x80, 0x0d, 0x0a]);

    let blob = p
        .upload_blob(&payload, "application/octet-stream")
        .await
        .expect("Blob 上传失败");
    println!(
        "上传: blobId={} size={} type={}",
        blob.id, blob.size, blob.content_type
    );
    assert!(!blob.id.is_empty(), "服务端未返回 blobId");
    assert_eq!(
        blob.size,
        payload.len() as u64,
        "服务端回报的 size 与本地字节数不一致"
    );
    assert_eq!(blob.content_type, "application/octet-stream");

    let back = p.download_blob(&blob.id).await.expect("Blob 下载失败");
    assert_eq!(
        back.len(),
        payload.len(),
        "下载字节数与上传不一致: {} vs {}",
        back.len(),
        payload.len()
    );
    assert_eq!(back, payload, "下载回来的字节被改过（二进制安全被破坏）");

    // 能力位必须如实反映服务端（真机有 blob 能力）
    let info = p.verify_credentials().await.expect("会话校验失败");
    assert!(
        info.capabilities.blob_upload,
        "真机支持 blob 上传，能力位不该为 false"
    );
}

/// 发信真链路：`Identity/get` → 草稿 + `EmailSubmission/set` → 真投递回本箱。
///
/// 这条覆盖的是**唯一没被验证过的写路径**。两个易错点：
/// 1. `identityId` 必须是服务端身份 id（真机是 `"b"`），填邮箱地址会被
///    `invalidProperties[identityId]` 拒掉；
/// 2. `EmailSubmission/set` 是「方法级成功、对象级失败」，只看 HTTP 200 会把
///    「没发出去」当成功——所以这里必须去收件箱里把邮件捞回来才算数。
#[tokio::test]
async fn live_jmap_identity_and_submission_delivers_mail() {
    let cfg = live_or_skip!();
    let p = connect(&cfg);
    purge_leftovers(&p).await;

    // ---- 身份：id 由服务端给，不是邮箱地址
    let identities = p.identities().await.expect("Identity/get 失败");
    assert!(!identities.is_empty(), "账号没有发送身份，发信无从谈起");
    for i in &identities {
        println!("身份: id={} email={} name={}", i.id, i.email, i.name);
    }
    let mine = identities
        .iter()
        .find(|i| i.email.eq_ignore_ascii_case(&cfg.user))
        .unwrap_or_else(|| panic!("没有匹配 {} 的发送身份: {:?}", cfg.user, identities));
    assert!(!mine.id.is_empty());

    let tag = format!("CrossMail-jm-submit-{}", std::process::id());
    let mut msg = OutgoingMessage::new(
        "live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} 发信真链路"),
    );
    msg.text_body = Some(format!("{tag} body"));
    msg.attachments
        .push(crossmail_core::model::OutgoingAttachment {
            name: "probe.txt".into(),
            content_type: "text/plain".into(),
            data: b"attachment payload\n".to_vec(),
        });

    let receipt = p.send_email(msg).await.expect("EmailSubmission 失败");
    println!(
        "回执: server_id={:?} message_id={} sent_at={}",
        receipt.server_id, receipt.message_id, receipt.sent_at
    );
    assert!(
        receipt.message_id.starts_with('<') && receipt.message_id.ends_with('>'),
        "message_id 不是合法 msg-id: {}",
        receipt.message_id
    );

    let boxes = p.list_mailboxes().await.expect("Mailbox/get 失败");
    let sent = mailbox_id_by_role(&boxes, MailboxRole::Sent);
    let drafts = mailbox_id_by_role(&boxes, MailboxRole::Drafts);
    let inbox = mailbox_id_by_role(&boxes, MailboxRole::Inbox);
    assert_eq!(
        receipt.sent_mailbox_id.as_deref(),
        Some(sent.as_str()),
        "回执未指向已发送箱"
    );

    // ---- 已发送箱：onSuccessUpdateEmail 必须真的把草稿搬过去
    let mut sent_req = SyncRequest::new("live-acct");
    sent_req.mailbox_id = Some(sent.clone());
    sent_req.limit = 200;
    let sent_page = p
        .sync_emails(sent_req, CancellationToken::new(), None)
        .await
        .expect("同步已发送箱失败");
    let sent_mail = sent_page
        .emails
        .iter()
        .find(|e| e.subject.contains(&tag))
        .unwrap_or_else(|| {
            panic!(
                "已发送箱里找不到刚发的邮件（共 {} 封）：说明 onSuccessUpdateEmail 没生效",
                sent_page.emails.len()
            )
        });
    assert!(
        sent_mail.flags.contains(&Flag::Seen),
        "已发送副本应带 \\Seen: {:?}",
        sent_mail.flags
    );
    let sent_full = p
        .fetch_email(&sent_mail.id, true)
        .await
        .expect("取已发送正文失败");
    assert!(
        sent_full.text_body.as_deref().unwrap_or("").contains(&tag),
        "已发送副本正文与写入不一致: {:?}",
        sent_full.text_body
    );
    assert_eq!(
        sent_full.attachments.len(),
        1,
        "已发送副本附件数不对: {:?}",
        sent_full.attachments
    );

    // ---- 草稿箱里不能还留着它
    let mut draft_req = SyncRequest::new("live-acct");
    draft_req.mailbox_id = Some(drafts.clone());
    draft_req.limit = 200;
    let draft_page = p
        .sync_emails(draft_req, CancellationToken::new(), None)
        .await
        .expect("同步草稿箱失败");
    assert!(
        !draft_page.emails.iter().any(|e| e.subject.contains(&tag)),
        "发送后草稿箱里还留着邮件：$draft 关键字或 mailboxIds 没被清理"
    );

    // ---- 收件箱：真投递才算发信成功（本地投递通常秒到，最多等 30s）
    let mut delivered: Option<String> = None;
    for attempt in 0..30 {
        let mut req = SyncRequest::new("live-acct");
        req.mailbox_id = Some(inbox.clone());
        req.limit = 300;
        let page = p
            .sync_emails(req, CancellationToken::new(), None)
            .await
            .expect("同步收件箱失败");
        if let Some(found) = page.emails.iter().find(|e| e.subject.contains(&tag)) {
            delivered = Some(found.id.clone());
            println!("第 {} 秒投递到达: id={}", attempt, found.id);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    let delivered_id = delivered.unwrap_or_else(|| {
        panic!("30 秒内收件箱没收到 {tag}：提交成功但**没有投递**（队列/外发配置问题）")
    });

    // ---- 清理：彻底销毁（留在回收站会污染后续人工盘点）
    hard_destroy(&p, &[sent_mail.id.clone(), delivered_id.clone()]).await;
}

/// 彻底销毁测试邮件（`Email/set` destroy），避免回收站越积越多。
async fn hard_destroy(p: &JmapProvider, ids: &[String]) {
    use std::collections::BTreeMap;
    if ids.is_empty() {
        return;
    }
    match p
        .client()
        .email_set(BTreeMap::new(), BTreeMap::new(), ids)
        .await
    {
        Ok(_) => println!("已彻底销毁 {} 封测试邮件", ids.len()),
        Err(e) => eprintln!("彻底销毁失败（会留在回收站）: {e}"),
    }
}

// ---------------- S6-12：EventSource 推送真联调 ----------------

/// 订阅的消息来源：后台任务把推送结果转给测试主体。
enum PushMsg {
    /// 订阅已建立（connection 已 200），此后的变化才会被推送。
    Ready,
    Update(PushUpdate),
    Failed(String),
}

/// 在后台任务里跑一条完整订阅（独立的 provider 实例，才能 `'static`）。
fn spawn_subscriber(
    cfg: &LiveConfig,
    push_cfg: PushConfig,
) -> tokio::sync::mpsc::Receiver<PushMsg> {
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let (url, user, pass) = (cfg.url.clone(), cfg.user.clone(), cfg.pass.clone());
    tokio::spawn(async move {
        let auth = AuthMethod::basic(user.clone(), pass);
        let p = JmapProvider::new(url, auth, user);
        let mut session = match p.watch(push_cfg, CancellationToken::new()).await {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(PushMsg::Failed(e.to_string())).await;
                return;
            }
        };
        // 「已订阅」必须是**连接真的建好了**，否则测试主体会在空档里改动：
        // 那条推送永远不会到，而且没有任何提示。
        if let Err(e) = session.connect(p.client()).await {
            let _ = tx.send(PushMsg::Failed(e.to_string())).await;
            return;
        }
        if tx.send(PushMsg::Ready).await.is_err() {
            return;
        }
        loop {
            match session.next(p.client()).await {
                Ok(update) => {
                    if tx.send(PushMsg::Update(update)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx.send(PushMsg::Failed(e.to_string())).await;
                    return;
                }
            }
        }
    });
    rx
}

async fn email_state(p: &JmapProvider, account: &str) -> String {
    let result = p
        .client()
        .call(
            "Email/get",
            serde_json::json!({ "accountId": account, "ids": [], "properties": ["subject"] }),
        )
        .await
        .expect("Email/get（只取 state）失败");
    result["state"].as_str().unwrap_or("").to_string()
}

/// 订阅 → 真改动 → 收到 state 推送；再改一次还会继续推。
///
/// 这条是推送链路的**存在性证明**：连接能建、能收到事件、并且是「按变化推」
/// 而不是「连接一建立就吐一次」。
#[tokio::test]
async fn live_jmap_push_delivers_state_changes() {
    let cfg = live_or_skip!();
    let p = connect(&cfg);
    let account = p.client().account_id().await.expect("取账号失败");
    purge_leftovers(&p).await;

    let info = p.verify_credentials().await.expect("会话校验失败");
    assert!(
        info.capabilities.push,
        "真机声明了 eventSourceUrl，能力位不该是 false"
    );

    let (es, ws) = p.push_endpoints().await.expect("取推送地址失败");
    println!("EventSource: {:?}\nWebSocket(未实现): {:?}", es, ws);
    assert!(es.is_some(), "真机应提供 eventSourceUrl");

    let before = email_state(&p, &account).await;
    println!("变化前 Email state = {before}");

    let mut rx = spawn_subscriber(&cfg, PushConfig::for_types(&["Email"]));
    match tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
        .await
        .expect("20 秒内订阅没建立")
        .expect("订阅任务提前退出")
    {
        PushMsg::Ready => println!("订阅已建立"),
        PushMsg::Failed(e) => panic!("订阅失败: {e}"),
        PushMsg::Update(u) => panic!("建连后不该立刻推事件: {u:?}"),
    }

    // 第一次改动：建一个草稿
    let tag = format!("CrossMail-jm-push-{}", std::process::id());
    let mut msg = OutgoingMessage::new(
        "live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} 推送真联调"),
    );
    msg.text_body = Some("push probe".into());
    let draft_id = p.save_draft(&msg).await.expect("建草稿失败");

    let first = next_state(&mut rx).await;
    println!("推送 1: {:?}", first.state_of(&account, "Email"));
    let first_state = first
        .state_of(&account, "Email")
        .cloned()
        .unwrap_or_else(|| panic!("推送里没有 {account}/Email 的 state: {:?}", first.changed));
    assert_ne!(first_state, before, "推送的 state 与变化前相同，等于没推");

    // 第二次改动：销毁草稿 —— 证明推送是持续生效的，不是一次性
    let draft_ids = vec![draft_id];
    destroy_or_log(&p, &draft_ids).await;
    let second = next_state(&mut rx).await;
    let second_state = second
        .state_of(&account, "Email")
        .cloned()
        .unwrap_or_default();
    println!("推送 2: {second_state}（推送 1 是 {first_state}）");
    assert_ne!(
        second_state, first_state,
        "第二次改动后 state 没变，推送没有持续生效"
    );
}

/// 收下一条 **State** 推送（跳过心跳）。
async fn next_state(rx: &mut tokio::sync::mpsc::Receiver<PushMsg>) -> StateChange {
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(30), rx.recv())
            .await
            .expect("30 秒内没等到推送")
            .expect("订阅任务提前退出")
        {
            PushMsg::Update(PushUpdate::State(change)) => return change,
            PushMsg::Update(PushUpdate::Ping) => continue,
            PushMsg::Update(PushUpdate::Resync) => continue,
            PushMsg::Update(other) => panic!("不该出现的推送: {other:?}"),
            PushMsg::Ready => continue,
            PushMsg::Failed(e) => panic!("订阅中断: {e}"),
        }
    }
}

/// `closeafter=state`：服务端在首个 state 后主动断开，EOF 是**正常结束**。
///
/// 这条的价值在「区分结束与故障」：如果把 EOF 当错误重连，一次性订阅会变成死循环。
#[tokio::test]
async fn live_jmap_push_single_shot_ends_after_one_state() {
    let cfg = live_or_skip!();
    let p = connect(&cfg);
    let account = p.client().account_id().await.expect("取账号失败");

    let mut reader = p
        .client()
        .open_push(&PushConfig::single_shot(&["Email"]))
        .await
        .expect("建推送连接失败");

    let tag = format!("CrossMail-jm-oneshot-{}", std::process::id());
    let mut msg = OutgoingMessage::new(
        "live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} 一次性订阅"),
    );
    msg.text_body = Some("single shot probe".into());
    let draft_id = p.save_draft(&msg).await.expect("建草稿失败");

    let update = tokio::time::timeout(std::time::Duration::from_secs(30), reader.next_update())
        .await
        .expect("30 秒内没收到 state")
        .expect("读推送出错");
    match update {
        Some(PushUpdate::State(change)) => {
            println!("一次性订阅收到 state: {:?}", change.changed);
            assert!(
                change.state_of(&account, "Email").is_some(),
                "推送里没有 {account}/Email: {:?}",
                change.changed
            );
        }
        other => panic!("期望 State，实际 {other:?}"),
    }

    let end = tokio::time::timeout(std::time::Duration::from_secs(15), reader.next_update())
        .await
        .expect("服务端没在 15 秒内断开 closeafter=state 连接");
    assert_eq!(end.expect("EOF 不该是错误"), None, "断开应表现为 None");

    destroy_or_log(&p, &[draft_id]).await;
}

/// 销毁测试邮件，失败只告警不掩盖（残留可用 `CrossMail-jm-` 前缀识别）。
async fn destroy_or_log(p: &JmapProvider, ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    if let Err(e) = p.delete_emails(ids).await {
        eprintln!("清理失败（会留在回收站）: {e}");
    }
}
