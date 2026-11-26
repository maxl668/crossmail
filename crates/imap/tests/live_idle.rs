//! S6-14 真机端到端：**IDLE 推送 → 增量同步 → 真正落库**。
//!
//! 这条用例证明的是 IMAP 侧「不轮询也能实时」：在真实服务器上 APPEND 一封草稿到
//! 被监视的文件夹，然后**没有任何人调 sync()**，邮件自己出现在本地库里。
//!
//! 监视 Drafts 而不是 INBOX 是有意的：本仓库的 APPEND 入口是 `save_draft`，它固定写
//! Drafts；要在 INBOX 上制造变化就得另开一条原始连接手搓 `APPEND`，徒增不确定性。
//! IDLE 的语义是「当前选中文件夹有变化」，监视哪个箱都会被同样地推送。
//!
//! 默认静默跳过（没设 `CROSSMAIL_LIVE_IMAP_HOST`）。会真的写数据（建一个草稿并销毁），
//! 用固定前缀 `CrossMail-idle-` 便于人工识别残留。
//!
//! 与 `live_server.rs` 共用同一个真实账号，必须 `--test-threads=1`。

use chrono::Utc;
use crossmail_core::error::MailResult;
use crossmail_core::model::{Account, Address, MailboxRole, OutgoingMessage, ProtocolKind};
use crossmail_core::provider::{CancellationToken, MailProvider};
use crossmail_imap::idle::IdleConfig;
use crossmail_imap::transport::Endpoint;
use crossmail_imap::{ImapProvider, ImapProviderConfig};
use crossmail_storage::Database;
use crossmail_sync::{run_push_sync, PushSyncConfig, PushSyncEvent};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct LiveConfig {
    host: String,
    port: u16,
    tls_name: String,
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
    Some(LiveConfig {
        host,
        port: std::env::var("CROSSMAIL_LIVE_IMAP_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(993),
        tls_name: std::env::var("CROSSMAIL_LIVE_IMAP_TLS_NAME").unwrap_or_default(),
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

const ACCOUNT: &str = "live-imap";

fn endpoint(cfg: &LiveConfig) -> Endpoint {
    let ep = Endpoint::new(cfg.host.clone(), cfg.port).with_timeout(Duration::from_secs(20));
    if cfg.tls_name.trim().is_empty() {
        ep
    } else {
        ep.with_tls_name(cfg.tls_name.clone())
    }
}

async fn connect(cfg: &LiveConfig) -> ImapProvider {
    let provider_cfg = ImapProviderConfig::new(
        ACCOUNT,
        cfg.user.clone(),
        cfg.user.clone(),
        cfg.pass.clone(),
    );
    ImapProvider::connect(&endpoint(cfg), &cfg.user, &cfg.pass, provider_cfg)
        .await
        .expect("连接真实 IMAP 失败")
}

/// 建库并把服务端的账号/文件夹先落库（邮件表对文件夹有外键）。
async fn seed_db(provider: &ImapProvider) -> MailResult<Arc<Mutex<Database>>> {
    let mut db = Database::open_in_memory()?;
    db.migrate()?;
    db.upsert_account(&Account::new(
        ACCOUNT,
        provider.config().email_address.clone(),
        ProtocolKind::Imap,
    ))?;
    for mailbox in provider.list_mailboxes().await? {
        db.upsert_mailbox(&mailbox)?;
    }
    Ok(Arc::new(Mutex::new(db)))
}

fn local_email_exists(db: &Arc<Mutex<Database>>, email_id: &str) -> bool {
    db.lock()
        .ok()
        .and_then(|guard| guard.get_email(email_id).ok().flatten())
        .is_some()
}

/// IDLE 唤醒驱动 → 驱动自己把新邮件增量同步进本地库。
#[tokio::test]
async fn live_idle_drives_incremental_sync_into_local_db() {
    let cfg = live_or_skip!();
    let provider = Arc::new(connect(&cfg).await);

    // 能力位自检：登录后必须拿到 IDLE，否则后面 subscribe 会直接 Unsupported。
    let caps = provider.capabilities();
    assert!(
        caps.imap_idle && caps.push,
        "登录后能力集应含 IDLE（真机登录前没有、登录后才有）: {:?}",
        caps
    );

    // 监视 Drafts：本仓库唯一能 APPEND 的箱就是它
    let mailboxes = provider.list_mailboxes().await.expect("列文件夹失败");
    let drafts = mailboxes
        .iter()
        .find(|m| m.role == MailboxRole::Drafts)
        .expect("服务端缺少 Drafts 文件夹");

    let db = seed_db(&provider).await.expect("seed 本地库失败");

    // 1) 先建订阅（返回即代表已进入 IDLE），再拉初始快照：
    //    顺序反了会静默丢掉两者之间的变化。
    let cancel = CancellationToken::new();
    let mut watcher = provider
        .watch(IdleConfig::for_mailbox(drafts.id.clone()), cancel.clone())
        .await
        .expect("订阅 IDLE 失败（服务端未声明 IDLE？）");

    let engine = crossmail_sync::SyncEngine::new(Arc::clone(&provider), Arc::clone(&db));
    let initial = engine
        .sync(ACCOUNT, Some(drafts.id.clone()), cancel.clone(), None)
        .await
        .expect("初始快照同步失败");
    println!(
        "初始快照({}): inserted={} updated={} unchanged={}",
        drafts.name, initial.stats.inserted, initial.stats.updated, initial.stats.unchanged
    );

    // 2) 制造一次真实变化：APPEND 一封草稿到被监视的文件夹
    let tag = format!("CrossMail-idle-{}", std::process::id());
    let mut msg = OutgoingMessage::new(
        ACCOUNT,
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} IDLE 驱动同步"),
    );
    msg.text_body = Some(format!("created at {:?}", Utc::now()));
    let draft_id = provider.save_draft(&msg).await.expect("建草稿失败");
    println!("已建草稿 emailId = {draft_id}");

    // 3) 让驱动自己跑：全程没有任何人调 sync()
    let events: Arc<Mutex<Vec<PushSyncEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&events);
    let push_cfg = PushSyncConfig {
        debounce: Duration::from_millis(200),
        sync_on_close: false,
        mailbox_id: Some(drafts.id.clone()),
        ..PushSyncConfig::default()
    };
    let driver = run_push_sync(
        &engine,
        ACCOUNT,
        &mut watcher,
        &push_cfg,
        cancel.clone(),
        move |event| {
            if let Ok(mut guard) = sink.lock() {
                guard.push(event);
            }
        },
    );

    // 4) 一边跑驱动，一边等库里出现这封邮件
    let wait_deadline = Instant::now() + Duration::from_secs(45);
    let found = tokio::select! {
        result = driver => panic!("驱动意外退出（说明连接提前结束或同步失败）: {result:?}"),
        found = async {
            loop {
                if local_email_exists(&db, &draft_id) {
                    break true;
                }
                if Instant::now() >= wait_deadline {
                    break false;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        } => found,
    };

    cancel.cancel();
    assert!(found, "45 秒内 IDLE 没有把新邮件带进本地库——驱动链路断了");

    let synced = events.lock().expect("events").clone();
    let runs = synced
        .iter()
        .filter(|e| matches!(e, PushSyncEvent::Synced { .. }))
        .count();
    assert!(runs >= 1, "至少要跑过一次推送驱动的同步，事件: {synced:?}");

    // 落库内容要真的是这封信，而不是「库里恰好有条记录」
    let stored = db
        .lock()
        .expect("db")
        .get_email(&draft_id)
        .expect("读库失败")
        .expect("库里应该有这封邮件");
    assert_eq!(stored.subject, format!("{tag} IDLE 驱动同步"));
    println!(
        "IDLE 驱动的同步完成：runs={runs}，邮件已落库（文件夹 {:?}）",
        stored.mailbox_ids
    );

    // 5) 清理：销毁这封草稿
    if let Err(e) = provider
        .delete_emails(std::slice::from_ref(&draft_id))
        .await
    {
        eprintln!("清理草稿失败（残留可用前缀 {tag} 人工识别）: {e}");
    }
}
