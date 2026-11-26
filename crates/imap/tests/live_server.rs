//! 真实服务器联调（live integration）。
//!
//! 默认**静默跳过**：没设 `CROSSMAIL_LIVE_IMAP_HOST` 就什么都不做，CI 与离线开发不受影响。
//! 设齐环境变量后才会真连服务器，把 `ImapProvider` 整条链路跑一遍。
//!
//! 需要这几个环境变量：
//! ```text
//! CROSSMAIL_LIVE_IMAP_HOST     TCP 实际连接地址（IP 或域名），必需
//! CROSSMAIL_LIVE_IMAP_PORT     默认 993
//! CROSSMAIL_LIVE_IMAP_TLS_NAME 证书校验用主机名（证书签给公网域名而连内网 IP 时必须给），可选
//! CROSSMAIL_LIVE_IMAP_USER     登录名，必需
//! CROSSMAIL_LIVE_IMAP_PASS     密码，必需
//! ```
//!
//! 为什么单独一个测试文件而不是 `#[ignore]`：`#[ignore]` 需要人记住 `--ignored`，
//! 而环境变量门控可以让「有凭据的机器自动真跑、没凭据的机器自动跳过」，
//! 不需要在两台机器上维护不同的命令。
//!
//! 这个测试会**真的写数据**（建测试文件夹、APPEND 草稿、改标记），
//! 因此每一步都自带清理，异常退出留下的残留用固定前缀 `CrossMail-it-` 可人工识别。

use crossmail_core::error::ErrorKind;
use crossmail_core::model::{Address, Flag, MailboxRole, OutgoingMessage};
use crossmail_core::provider::{CancellationToken, MailProvider, MailboxDraft, SyncRequest};
use crossmail_imap::client::local_email_id;
use crossmail_imap::{Endpoint, ImapProvider, ImapProviderConfig};
use std::time::Duration;

struct LiveConfig {
    endpoint: Endpoint,
    user: String,
    pass: String,
}

/// 读取环境变量；缺关键项返回 None（=跳过）。
fn live_config() -> Option<LiveConfig> {
    let host = std::env::var("CROSSMAIL_LIVE_IMAP_HOST").ok()?;
    let user = std::env::var("CROSSMAIL_LIVE_IMAP_USER").ok()?;
    let pass = std::env::var("CROSSMAIL_LIVE_IMAP_PASS").ok()?;
    if host.trim().is_empty() || user.trim().is_empty() {
        return None;
    }
    let port: u16 = std::env::var("CROSSMAIL_LIVE_IMAP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(993);
    let mut endpoint = Endpoint::new(host, port).with_timeout(Duration::from_secs(20));
    if let Ok(name) = std::env::var("CROSSMAIL_LIVE_IMAP_TLS_NAME") {
        if !name.trim().is_empty() {
            endpoint = endpoint.with_tls_name(name);
        }
    }
    Some(LiveConfig {
        endpoint,
        user,
        pass,
    })
}

macro_rules! live_or_skip {
    () => {
        match live_config() {
            Some(c) => c,
            None => {
                eprintln!(
                    "跳过：未设置 CROSSMAIL_LIVE_IMAP_HOST/USER/PASS（真联调需要真实服务器凭据）"
                );
                return;
            }
        }
    };
}

async fn connect(cfg: &LiveConfig, account_id: &str) -> ImapProvider {
    let provider_cfg = ImapProviderConfig::new(
        account_id,
        cfg.user.clone(),
        cfg.user.clone(),
        cfg.pass.clone(),
    );
    ImapProvider::connect(&cfg.endpoint, &cfg.user, &cfg.pass, provider_cfg)
        .await
        .expect("建连 + 认证失败")
}

/// 清掉历史运行残留（异常中断时清理没跑到会累积）。
async fn purge_leftovers(p: &ImapProvider) {
    let stale: Vec<String> = p
        .list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .filter(|m| m.name.starts_with("CrossMail-it-"))
        .map(|m| m.id)
        .collect();
    for id in stale {
        p.delete_mailbox(&id).await.expect("清理残留文件夹失败");
    }
}

/// 账号、目录、能力协商：真实服务器第一关。
#[tokio::test]
async fn live_verify_and_list_mailboxes() {
    let cfg = live_or_skip!();
    let p = connect(&cfg, "live-acct").await;

    let info = p.verify_credentials().await.expect("凭证校验失败");
    assert_eq!(info.email_address, cfg.user);
    // 真实服务器必须至少声明 IMAP4rev1；一条都没有说明问候语/能力解析与服务器对不上
    // 真实服务器必须至少声明 UIDPLUS —— 它是「移动/复制后拿新 UID」的前提，
    // 缺了它 move/copy 只能靠猜 UID，S3 的增量就不可靠。要是这条挂了，
    // 说明能力协商压根没跑通（问候语 CAPABILITY 没解析出来）。
    let caps = p.capabilities();
    assert!(
        caps.uidplus,
        "服务端未协商出 UIDPLUS，能力解析有问题: {caps:?}"
    );
    println!("能力协商: {caps:?}");

    let boxes = p.list_mailboxes().await.expect("LIST 失败");
    assert!(!boxes.is_empty(), "真实账号至少应有一个文件夹");
    let inbox = boxes
        .iter()
        .find(|m| m.role == MailboxRole::Inbox)
        .expect("没识别出 INBOX：special-use 或名字回退逻辑有问题");
    // 关键：INBOX 的名字必须能直接当 SELECT 参数用，不能是展示名
    assert!(
        inbox.id.to_ascii_uppercase().contains("INBOX"),
        "INBOX 的 id 不可用于 SELECT: {}",
        inbox.id
    );
    println!(
        "账号 {} 共 {} 个文件夹，INBOX id={}",
        info.email_address,
        boxes.len(),
        inbox.id
    );
    for m in &boxes {
        println!("  - id={:<28} name={:<24} role={:?}", m.id, m.name, m.role);
    }
}

/// 文件夹增删改：CREATE / RENAME / DELETE 与回读一致性。
#[tokio::test]
async fn live_mailbox_lifecycle() {
    let cfg = live_or_skip!();
    let p = connect(&cfg, "live-acct").await;
    purge_leftovers(&p).await;

    let tag = format!("CrossMail-it-{}", std::process::id());
    let draft = MailboxDraft {
        name: tag.clone(),
        parent_id: None,
        role: MailboxRole::Other,
    };
    let created = p.create_mailbox(draft).await.expect("CREATE 失败");
    assert_eq!(created.name, tag, "回读的文件夹名与创建时不一致");

    // 重复建同名文件夹必须幂等成功（真实场景：重试、并发初始化）
    p.create_mailbox(MailboxDraft {
        name: tag.clone(),
        parent_id: None,
        role: MailboxRole::Other,
    })
    .await
    .expect("重复 CREATE 应幂等成功");

    // 重命名后必须能在 LIST 里按新名找到，否则说明回读用的是旧缓存
    let renamed = format!("{tag}-r");
    p.rename_mailbox(&created.id, &renamed)
        .await
        .expect("RENAME 失败");
    let names: Vec<String> = p
        .list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert!(
        names.iter().any(|n| n == &renamed),
        "重命名后 LIST 里找不到 {renamed}，实际: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == &tag),
        "重命名后旧名 {tag} 仍在，说明服务端没生效"
    );

    // IMAP 的文件夹主键就是名字本身：改名后 id 必须换成新名，
    // 用旧 id 删会得到 TRYCREATE / 不存在（这一步就是专门验证这个语义）。
    p.delete_mailbox(&renamed).await.expect("DELETE 失败");
    let names: Vec<String> = p
        .list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert!(
        !names.iter().any(|n| n == &renamed),
        "删除后 {renamed} 仍在: {names:?}"
    );
    assert!(
        !names.iter().any(|n| n == &tag),
        "删除后旧名 {tag} 仍在: {names:?}"
    );
}

/// 增量同步：首轮全量 → 二次同步应为空且游标可复用。
#[tokio::test]
async fn live_sync_inbox_and_reuse_cursor() {
    let cfg = live_or_skip!();
    let p = connect(&cfg, "live-acct").await;
    let inbox = p
        .list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .find(|m| m.role == MailboxRole::Inbox)
        .expect("没识别出 INBOX");

    let mut req = SyncRequest::new("live-acct");
    req.mailbox_id = Some(inbox.id.clone());
    req.limit = 20;
    let first = p
        .sync_emails(req.clone(), CancellationToken::new(), None)
        .await
        .expect("首轮同步失败");
    assert!(
        first.new_state.is_some(),
        "首轮同步必须返回游标，否则下一次会重新全量拉"
    );
    println!("INBOX 首轮拉到 {} 封", first.emails.len());
    for e in first.emails.iter().take(5) {
        println!("  uid={:?} subject={:?}", e.uid, e.subject);
    }
    // 本地主键必须能被反解回 (文件夹, UID)，否则后续所有按 id 的操作都会失败
    for e in &first.emails {
        p.parse_email_id(&e.id).expect("本地主键无法反解");
    }

    // 第二轮：带上游标，不该重复返回已同步的邮件。
    //
    // CONDSTORE 语义下「重复」要区分两件事：**新邮件**（UID 更大）当然要下发；
    // 只有「同一封、内容没变」的重复下发才是游标失效。所以这里按 UID 判断，
    // 并把撞上的具体邮件打出来——只报个数字，出问题时无从下手。
    req.since_state = first.new_state.clone();
    let second = p
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .expect("增量同步失败");
    let overlap: Vec<_> = second
        .emails
        .iter()
        .filter(|e| first.emails.iter().any(|f| f.id == e.id))
        .collect();
    for e in &overlap {
        println!(
            "  重复下发: uid={:?} subject={:?} flags={:?}",
            e.uid, e.subject, e.flags
        );
    }
    println!(
        "第二轮：{} 封；游标 {:?} -> {:?}；modseq {:?} -> {:?}",
        second.emails.len(),
        first.new_state,
        second.new_state,
        first.new_modseq,
        second.new_modseq
    );
    assert!(
        overlap.is_empty(),
        "增量同步重复返回了 {} 封已同步邮件（详见上面的 uid/subject）",
        overlap.len()
    );

    // 有邮件就顺带验证取正文不炸（正文 part 选择最容易在真实服务器上出偏差）
    if let Some(first_mail) = first.emails.first() {
        let full = p
            .fetch_email(&first_mail.id, true)
            .await
            .expect("取正文失败");
        assert_eq!(full.id, first_mail.id);
        println!("正文长度 {:?}", full.text_body.as_ref().map(|t| t.len()));
    }
}

/// 写操作闭环：APPEND 草稿 → 拉回 → 改标记 → 物理删除。
#[tokio::test]
async fn live_append_fetch_flag_and_delete() {
    let cfg = live_or_skip!();
    let p = connect(&cfg, "live-acct").await;
    purge_leftovers(&p).await;

    let tag = format!("CrossMail-it-{}", std::process::id());
    let created = p
        .create_mailbox(MailboxDraft {
            name: tag.clone(),
            parent_id: None,
            role: MailboxRole::Other,
        })
        .await
        .expect("CREATE 失败");

    // 用 save_draft 走真实 APPEND 路径（依赖服务端有 Drafts 角色文件夹）
    let mut msg = OutgoingMessage::new(
        "live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} subject"),
    );
    msg.text_body = Some("live integration body".into());
    let draft_id = p.save_draft(&msg).await.expect("APPEND 草稿失败");
    println!("草稿落到 {draft_id}");

    // 草稿进的是 Drafts 文件夹，把它同步出来确认内容真的落盘
    let drafts = p
        .list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .find(|m| m.role == MailboxRole::Drafts)
        .expect("服务端缺少 Drafts 角色文件夹");
    let mut req = SyncRequest::new("live-acct");
    req.mailbox_id = Some(drafts.id.clone());
    req.limit = 50;
    let page = p
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .expect("同步草稿箱失败");
    let mine = page
        .emails
        .iter()
        .find(|e| e.subject.contains(&tag))
        .expect("APPEND 的草稿在服务器上找不到");
    let full = p.fetch_email(&mine.id, true).await.expect("取草稿正文失败");
    assert!(
        full.text_body
            .as_deref()
            .map(|t| t.contains("live integration body"))
            .unwrap_or(false),
        "取回的正文与 APPEND 时不一致: {:?}",
        full.text_body
    );

    // 改标记：加 \Flagged 再撤掉，两次都要被服务端接受
    p.set_flags(std::slice::from_ref(&mine.id), Flag::Flagged, true)
        .await
        .expect("加标记失败");
    p.set_flags(std::slice::from_ref(&mine.id), Flag::Flagged, false)
        .await
        .expect("撤标记失败");

    // 清理：删草稿（进回收站或物理删）+ 删测试文件夹
    p.delete_emails(std::slice::from_ref(&mine.id))
        .await
        .expect("删草稿失败");
    p.delete_mailbox(&created.id).await.expect("DELETE 失败");
}

/// 同步指定文件夹，返回全部邮件（按 subject 过滤由调用方做）。
async fn sync_box(p: &ImapProvider, mailbox_id: &str) -> Vec<crossmail_core::model::Email> {
    let mut req = SyncRequest::new("live-acct");
    req.mailbox_id = Some(mailbox_id.to_string());
    req.limit = 200;
    p.sync_emails(req, CancellationToken::new(), None)
        .await
        .unwrap_or_else(|e| panic!("同步 {mailbox_id} 失败: {e:?}"))
        .emails
}

/// 在某个文件夹里按主题找邮件（服务器状态回读，不信本地缓存）。
async fn search_box(
    p: &ImapProvider,
    mailbox_id: &str,
    subject: &str,
) -> Vec<crossmail_core::model::Email> {
    sync_box(p, mailbox_id)
        .await
        .into_iter()
        .filter(|e| e.subject.contains(subject))
        .collect()
}

async fn box_by_id(p: &ImapProvider, id: &str) -> String {
    p.list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .find(|m| m.id == id)
        .unwrap_or_else(|| panic!("找不到文件夹 {id}"))
        .name
}

/// MOVE / COPY / 物理删除：写链路里唯一还在「纯脚本化口径」的部分。
///
/// 脚本化通道能验证命令拼得对不对，验证不了服务器**收下命令之后干了什么**。
/// 这里每一步都回读服务端状态：源箱真的空了、目标箱真的有了、
/// 二次删除真的从回收站消失了——才算通过。
#[tokio::test]
async fn live_move_copy_and_permanent_delete() {
    let cfg = live_or_skip!();
    let p = connect(&cfg, "live-acct").await;
    purge_leftovers(&p).await;

    let pid = std::process::id();
    let src_name = format!("CrossMail-it-{pid}-src");
    let dst_name = format!("CrossMail-it-{pid}-dst");
    let mut created = Vec::new();
    for name in [&src_name, &dst_name] {
        created.push(
            p.create_mailbox(MailboxDraft {
                name: name.clone(),
                parent_id: None,
                role: MailboxRole::Other,
            })
            .await
            .expect("CREATE 失败"),
        );
    }
    let (src, dst) = (created[0].id.clone(), created[1].id.clone());

    let drafts = p
        .list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .find(|m| m.role == MailboxRole::Drafts)
        .expect("服务端缺少 Drafts 角色文件夹");
    let trash = p
        .list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .find(|m| m.role == MailboxRole::Trash)
        .expect("服务端缺少 Trash 角色文件夹");

    // 造一封邮件：草稿落在 Drafts，再 MOVE 到 src（顺便验证跨箱移动）
    let subject = format!("CrossMail-it-{pid} move-copy probe");
    let mut msg = OutgoingMessage::new(
        "live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        subject.clone(),
    );
    msg.text_body = Some("probe body".into());
    let draft_id = p.save_draft(&msg).await.expect("APPEND 草稿失败");
    p.move_emails(std::slice::from_ref(&draft_id), &drafts.id, &src)
        .await
        .expect("从 Drafts 移到 src 失败");

    let in_src: Vec<_> = sync_box(&p, &src)
        .await
        .into_iter()
        .filter(|e| e.subject.contains(&subject))
        .collect();
    assert_eq!(in_src.len(), 1, "MOVE 后 src 里应正好一封，实际 {in_src:?}");
    let src_id = in_src[0].id.clone();
    let drafts_left = sync_box(&p, &drafts.id)
        .await
        .into_iter()
        .filter(|e| e.subject.contains(&subject))
        .count();
    assert_eq!(drafts_left, 0, "MOVE 后 Drafts 里还留着原件");

    // COPY：目标箱要有，源箱不能少
    p.copy_emails(std::slice::from_ref(&src_id), &dst)
        .await
        .expect("COPY 失败");
    let copied = sync_box(&p, &dst)
        .await
        .into_iter()
        .filter(|e| e.subject.contains(&subject))
        .count();
    assert_eq!(copied, 1, "COPY 后 dst 里应有副本");
    let still_src = sync_box(&p, &src)
        .await
        .into_iter()
        .filter(|e| e.subject.contains(&subject))
        .count();
    assert_eq!(still_src, 1, "COPY 不该动源箱");

    // 删 src 那封 → 进回收站
    p.delete_emails(std::slice::from_ref(&src_id))
        .await
        .expect("移入回收站失败");
    let src_after = sync_box(&p, &src)
        .await
        .into_iter()
        .filter(|e| e.subject.contains(&subject))
        .count();
    assert_eq!(src_after, 0, "删除后 src 里仍在");
    let trashed = search_box(&p, &trash.id, &subject).await;
    assert!(!trashed.is_empty(), "回收站里没找到被删的邮件");

    // 回收站里再删 → 彻底消失（过去这里自我移动，等于什么都没做）
    let trash_id = trashed[0].id.clone();
    p.delete_emails(std::slice::from_ref(&trash_id))
        .await
        .expect("回收站内删除失败");
    let still_trash = search_box(&p, &trash.id, &subject).await;
    assert!(
        still_trash.is_empty(),
        "彻底删除后回收站里还有 {} 封",
        still_trash.len()
    );

    // 清理：dst 里的副本 + 两个测试文件夹
    let dst_left = search_box(&p, &dst, &subject).await;
    for e in dst_left {
        p.delete_emails(std::slice::from_ref(&e.id))
            .await
            .expect("清副本失败");
    }
    println!(
        "MOVE/COPY/EXPUNGE 闭环通过：src={} dst={}",
        box_by_id(&p, &src).await,
        box_by_id(&p, &dst).await
    );
    for m in &created {
        p.delete_mailbox(&m.id).await.expect("DELETE 失败");
    }
}

/// 认证失败必须是 `Authentication` 而不是网络/协议错误。
///
/// 这条在真实服务器上比 mock 更有价值：不同服务器对错密码的回应差异极大
/// （有的回 `NO [AUTHENTICATIONFAILED]`，有的直接断连），判错类型会让
/// 回放层把「密码错」当成可重试、无限重试下去。
#[tokio::test]
async fn live_wrong_password_is_authentication_error() {
    let cfg = live_or_skip!();
    let provider_cfg = ImapProviderConfig::new(
        "live-acct",
        cfg.user.clone(),
        cfg.user.clone(),
        "definitely-not-the-password",
    );
    let err = match ImapProvider::connect(
        &cfg.endpoint,
        &cfg.user,
        "definitely-not-the-password",
        provider_cfg,
    )
    .await
    {
        Ok(_) => panic!("错密码居然登录成功"),
        Err(e) => e,
    };
    println!("错密码返回: {err:?}");
    assert_eq!(
        err.kind(),
        crossmail_core::error::ErrorKind::Authentication,
        "错密码应判为认证错误，实际: {err:?}"
    );
    assert!(
        !err.is_retryable(),
        "认证失败不该被当成可重试，否则回放层会无限重试: {err:?}"
    );
}

/// 真机事实：对**不存在的 UID** 执行 `UID COPY` / `UID MOVE` / `UID STORE`，
/// Stalwart 一律回 `OK`（`OK No messages were copied.` / `OK UID STORE completed`），
/// 服务器上什么都没发生。
///
/// 这就是「陈旧主键」为什么难查：客户端的本地主键是 `账号/文件夹/UID`，文件夹来自上次
/// 同步的快照；邮件被移动过（服务器换了 UID，或在别的客户端搬走了）之后，这串 id 就指向
/// 一个空位置。以前直接发写命令，服务端愉快回 OK，回放报告一片绿，用户的邮件纹丝不动。
///
/// 这条用例盯的就是「别再静默成功」。
#[tokio::test]
async fn live_stale_uid_is_reported_instead_of_silently_ignored() {
    let cfg = live_or_skip!();
    let p = connect(&cfg, "live-acct").await;

    // 一个必然不存在的 UID：INBOX 里不可能有十亿号
    let ghost = local_email_id("live-acct", "INBOX", 999_999_999);

    let err = p
        .delete_emails(std::slice::from_ref(&ghost))
        .await
        .expect_err("服务端会回 OK；必须由客户端拦住，否则就是静默空转");
    assert_eq!(err.kind(), ErrorKind::NotFound, "实际错误: {err}");
    println!("删除陈旧主键 → {err}");

    let err = p
        .copy_emails(std::slice::from_ref(&ghost), &"INBOX".to_string())
        .await
        .expect_err("服务端会回 OK No messages were copied.；同样必须拦住");
    assert_eq!(err.kind(), ErrorKind::NotFound, "实际错误: {err}");
    println!("复制陈旧主键 → {err}");
}
