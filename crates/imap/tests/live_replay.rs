//! 端到端联调：**业务层写操作 → 离线队列 → 回放层 → 真实服务器**。
//!
//! 这是交付口径里最后一条没串起来的链路。此前的 live 用例都直接调 Provider，
//! 相当于只验证了「协议能通」；而产品实际走的是
//! `MailService::move_to`（只改本地 + 入队）→ `OfflineReplayer::replay`（翻译成协议调用）。
//! 中间任何一环错位（队列顺序、id 与文件夹的对应关系、from/to 语义），
//! 用户看到的就是「界面上动了，服务器上没动」——最典型的静默失败。
//!
//! 与 `live_server.rs` 一样：**没设环境变量就静默跳过**，不影响离线开发与 CI。
//!
//! ```text
//! CROSSMAIL_LIVE_IMAP_HOST / PORT / TLS_NAME / USER / PASS
//! ```

use crossmail_core::model::{Account, Flag, Mailbox, MailboxRole, ProtocolKind};
use crossmail_core::provider::{CancellationToken, MailProvider, MailboxDraft, SyncRequest};
use crossmail_imap::{Endpoint, ImapProvider, ImapProviderConfig};
use crossmail_service::MailService;
use crossmail_storage::Database;
use crossmail_sync::OfflineReplayer;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ACC: &str = "live-replay";

struct LiveConfig {
    endpoint: Endpoint,
    user: String,
    pass: String,
}

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

async fn connect(cfg: &LiveConfig) -> ImapProvider {
    let provider_cfg =
        ImapProviderConfig::new(ACC, cfg.user.clone(), cfg.user.clone(), cfg.pass.clone());
    ImapProvider::connect(&cfg.endpoint, &cfg.user, &cfg.pass, provider_cfg)
        .await
        .expect("建连 + 认证失败")
}

async fn sync_subject(p: &ImapProvider, mailbox_id: &str, subject: &str) -> usize {
    let mut req = SyncRequest::new(ACC);
    req.mailbox_id = Some(mailbox_id.to_string());
    req.limit = 200;
    p.sync_emails(req, CancellationToken::new(), None)
        .await
        .unwrap_or_else(|e| panic!("同步 {mailbox_id} 失败: {e:?}"))
        .emails
        .iter()
        .filter(|e| e.subject.contains(subject))
        .count()
}

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

/// 建一个只在本测试里有意义的本地库（内存库 + 全量迁移）。
fn local_db(cfg: &LiveConfig) -> Arc<Mutex<Database>> {
    let mut db = Database::open_in_memory().expect("打开内存库失败");
    db.migrate().expect("迁移失败");
    db.upsert_account(&Account::new(ACC, cfg.user.clone(), ProtocolKind::Imap))
        .expect("写账号失败");
    Arc::new(Mutex::new(db))
}

#[tokio::test]
async fn live_service_queue_replay_end_to_end() {
    let cfg = live_or_skip!();
    let provider = Arc::new(connect(&cfg).await);
    purge_leftovers(&provider).await;

    let pid = std::process::id();
    let tag = format!("CrossMail-it-{pid} replay probe");
    let src = provider
        .create_mailbox(MailboxDraft {
            name: format!("CrossMail-it-{pid}-rsrc"),
            parent_id: None,
            role: MailboxRole::Other,
        })
        .await
        .expect("CREATE src 失败");
    let dst = provider
        .create_mailbox(MailboxDraft {
            name: format!("CrossMail-it-{pid}-rdst"),
            parent_id: None,
            role: MailboxRole::Other,
        })
        .await
        .expect("CREATE dst 失败");

    // ---- setup：造一封真实邮件放进 src（这步用 Provider 直连，属于"准备真实数据"，不是被测链路）
    let drafts = provider
        .list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .find(|m| m.role == MailboxRole::Drafts)
        .expect("服务端缺少 Drafts");
    let mut msg = crossmail_core::model::OutgoingMessage::new(
        ACC,
        crossmail_core::model::Address::new(&cfg.user),
        vec![crossmail_core::model::Address::new(&cfg.user)],
        tag.clone(),
    );
    msg.text_body = Some("replay body".into());
    let draft_id = provider.save_draft(&msg).await.expect("APPEND 草稿失败");
    provider
        .move_emails(std::slice::from_ref(&draft_id), &drafts.id, &src.id)
        .await
        .expect("把草稿移进 src 失败");

    // ---- 本地镜像：业务层只认本地库
    let db = local_db(&cfg);
    {
        let g = db.lock().unwrap();
        g.upsert_mailbox(&Mailbox::new(src.id.clone(), ACC, src.name.clone()))
            .unwrap();
        g.upsert_mailbox(&Mailbox::new(dst.id.clone(), ACC, dst.name.clone()))
            .unwrap();
    }
    let mut req = SyncRequest::new(ACC);
    req.mailbox_id = Some(src.id.clone());
    req.limit = 200;
    let served = provider
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .expect("同步 src 失败")
        .emails
        .into_iter()
        .find(|e| e.subject.contains(&tag))
        .expect("APPEND 的邮件没出现在 src");
    // 直接把服务器返回的那条落进本地库：id 就是 Provider 生成的 `账号/文件夹/UID`
    db.lock().unwrap().upsert_email(&served).unwrap();

    let svc = MailService::new(db.clone());
    let replayer = OfflineReplayer::new(provider.clone(), db.clone());

    // ---- 第一跳：业务层移动（只改本地 + 入队，绝不碰网络）
    svc.move_to(&served.id, &dst.id).expect("业务层移动失败");
    assert_eq!(
        svc.pending_count(ACC).expect("读队列失败"),
        1,
        "移动应入队一条离线操作"
    );
    assert_eq!(
        sync_subject(&provider, &src.id, &tag).await,
        1,
        "业务层动作不该立刻改服务器（否则离线优先就是假的）"
    );

    // ---- 回放：队列 → 真服务器
    let report = replayer.replay(ACC).await.expect("回放失败");
    println!("第一跳回放: {report:?}");
    assert_eq!(report.applied, 1, "应成功回放 1 条，实际: {report:?}");
    assert!(report.is_drained(), "队列应清空，实际: {report:?}");
    assert_eq!(svc.pending_count(ACC).expect("读队列失败"), 0);
    assert_eq!(report.remaps.len(), 1, "IMAP MOVE 会换 UID，应回传身份映射");
    // 契约：移动后本地主键变了，调用方必须用映射里的新 id
    let new_id = report.remaps[0].to.clone();
    assert_eq!(report.remaps[0].from, served.id);
    assert!(
        db.lock().unwrap().get_email(&served.id).unwrap().is_none(),
        "旧主键的记录应已迁移"
    );

    assert_eq!(
        sync_subject(&provider, &dst.id, &tag).await,
        1,
        "回放后服务器 dst 里应有这封邮件"
    );
    assert_eq!(
        sync_subject(&provider, &src.id, &tag).await,
        0,
        "回放后服务器 src 里不该还有"
    );

    // ---- 第二跳：再移回去，用映射返回的新 id。
    // 这一跳同时验证「源文件夹以 from 为准」：本地记录此刻在 dst，服务器上这封邮件也在 dst，
    // 而它最初那个 id 里写的是 src——拿旧 id 当源就会变成同箱移动，被服务器直接拒。
    svc.move_to(&new_id, &src.id)
        .expect("第二次移动失败（本地记录应用新主键）");
    let report2 = replayer.replay(ACC).await.expect("第二次回放失败");
    println!("第二跳回放: {report2:?}");
    assert_eq!(report2.applied, 1, "第二跳应成功，实际: {report2:?}");
    assert_eq!(
        sync_subject(&provider, &src.id, &tag).await,
        1,
        "第二跳后服务器 src 里应有这封邮件"
    );

    // ---- 清理
    for mb in [&src.id, &dst.id] {
        let _ = provider.delete_mailbox(mb).await;
    }
}

/// 移动之后立刻改标记：队列里是两条操作，第二条的 id 仍指向旧文件夹。
///
/// 这条用例盯的是「身份漂移」：IMAP 的邮件身份是 `(文件夹, UID)`，移动之后 UID 会变
/// （COPYUID 新分配），而本地主键是移动前算出来的。
/// 修复前真机实测：回放报告 `applied: 2`，服务器上却没标星——标记打到了旧文件夹的旧 UID
/// （服务端对不存在的 UID 静默回 OK），零报错、队列清空，属最隐蔽的一类失败。
/// 现在靠 `MailProvider::move_emails` 回传的 `COPYUID` 映射把队列里的后续操作一起改写。
#[tokio::test]
async fn live_flag_after_move_reaches_the_message() {
    let cfg = live_or_skip!();
    let provider = Arc::new(connect(&cfg).await);
    purge_leftovers(&provider).await;

    let pid = std::process::id();
    let tag = format!("CrossMail-it-{pid} flag-after-move");
    let src = provider
        .create_mailbox(MailboxDraft {
            name: format!("CrossMail-it-{pid}-fsrc"),
            parent_id: None,
            role: MailboxRole::Other,
        })
        .await
        .expect("CREATE src 失败");
    let dst = provider
        .create_mailbox(MailboxDraft {
            name: format!("CrossMail-it-{pid}-fdst"),
            parent_id: None,
            role: MailboxRole::Other,
        })
        .await
        .expect("CREATE dst 失败");
    let drafts = provider
        .list_mailboxes()
        .await
        .expect("LIST 失败")
        .into_iter()
        .find(|m| m.role == MailboxRole::Drafts)
        .expect("服务端缺少 Drafts");
    let mut msg = crossmail_core::model::OutgoingMessage::new(
        ACC,
        crossmail_core::model::Address::new(&cfg.user),
        vec![crossmail_core::model::Address::new(&cfg.user)],
        tag.clone(),
    );
    msg.text_body = Some("flag body".into());
    let draft_id = provider.save_draft(&msg).await.expect("APPEND 草稿失败");
    provider
        .move_emails(std::slice::from_ref(&draft_id), &drafts.id, &src.id)
        .await
        .expect("把草稿移进 src 失败");

    let db = local_db(&cfg);
    {
        let g = db.lock().unwrap();
        g.upsert_mailbox(&Mailbox::new(src.id.clone(), ACC, src.name.clone()))
            .unwrap();
        g.upsert_mailbox(&Mailbox::new(dst.id.clone(), ACC, dst.name.clone()))
            .unwrap();
    }
    let mut req = SyncRequest::new(ACC);
    req.mailbox_id = Some(src.id.clone());
    req.limit = 200;
    let served = provider
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .expect("同步 src 失败")
        .emails
        .into_iter()
        .find(|e| e.subject.contains(&tag))
        .expect("APPEND 的邮件没出现在 src");
    db.lock().unwrap().upsert_email(&served).unwrap();

    let svc = MailService::new(db.clone());
    let replayer = OfflineReplayer::new(provider.clone(), db.clone());

    // 排队：先移动，再标星。两条都还没发出去。
    svc.move_to(&served.id, &dst.id).expect("移动失败");
    svc.toggle_star(&served.id, true).expect("标星失败");
    assert_eq!(svc.pending_count(ACC).expect("读队列失败"), 2);

    let report = replayer.replay(ACC).await.expect("回放失败");
    println!("移动+标星回放: {report:?}");
    assert_eq!(report.applied, 2, "两条都应回放成功: {report:?}");

    // 服务器事实：邮件在 dst 里，且真的带上了 \Flagged
    let mut req = SyncRequest::new(ACC);
    req.mailbox_id = Some(dst.id.clone());
    req.limit = 200;
    let in_dst = provider
        .sync_emails(req, CancellationToken::new(), None)
        .await
        .expect("同步 dst 失败")
        .emails
        .into_iter()
        .find(|e| e.subject.contains(&tag))
        .expect("dst 里没有这封邮件");
    assert!(
        in_dst.flags.contains(&Flag::Flagged),
        "服务器上这封邮件没被标星，说明标记打到了别处（旧文件夹/旧 UID）。实际标记: {:?}",
        in_dst.flags
    );

    for mb in [&src.id, &dst.id] {
        let _ = provider.delete_mailbox(mb).await;
    }
}
