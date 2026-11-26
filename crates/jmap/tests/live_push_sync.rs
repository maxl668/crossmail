//! S6-13 真机端到端：**推送 → 增量同步 → 真正落库**。
//!
//! 前面几条 live 用例各自证明了一段：推送能收到 state（`live_jmap_push_*`）、
//! 引擎能增量落库（sync 侧测试）。这一条把两端接起来，验证的是「接错就白干」的那件事：
//!
//! > 在真实服务器上新建一封邮件 → 推送把信号送到驱动 → 驱动自己拉增量 →
//! > 邮件出现在本地库里，**全程没有轮询、没有人调 sync()**。
//!
//! 默认静默跳过（没设 `CROSSMAIL_LIVE_JMAP_URL`）。会真的写数据（建一个草稿并销毁），
//! 用固定前缀 `CrossMail-js-` 便于人工识别残留。
//!
//! 注意：本文件与 `live_server.rs` 共用同一个真实账号的 INBOX/Drafts，
//! 必须用 `--test-threads=1` 跑，否则会互相抢文件夹。

use chrono::Utc;
use crossmail_core::error::MailResult;
use crossmail_core::model::{Account, Address, OutgoingMessage, ProtocolKind};
use crossmail_core::provider::{CancellationToken, MailProvider};
use crossmail_jmap::push::PushConfig;
use crossmail_jmap::watcher::JmapWatcher;
use crossmail_jmap::{AuthMethod, JmapProvider};
use crossmail_storage::Database;
use crossmail_sync::{run_push_sync, PushSyncConfig, PushSyncEvent};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct LiveConfig {
    url: String,
    user: String,
    pass: String,
}

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
                eprintln!("跳过：未设置 CROSSMAIL_LIVE_JMAP_URL/USER/PASS（真联调需要真实服务器凭据）");
                return;
            }
        }
    };
}

fn connect(cfg: &LiveConfig) -> JmapProvider {
    let auth = AuthMethod::basic(cfg.user.clone(), cfg.pass.clone());
    JmapProvider::new(cfg.url.clone(), auth, cfg.user.clone())
}

/// 建库并把服务端的账号/文件夹先落库（邮件表对文件夹有外键）。
async fn seed_db(
    provider: &JmapProvider,
    account_id: &str,
    user: &str,
) -> MailResult<Arc<Mutex<Database>>> {
    let mut db = Database::open_in_memory()?;
    db.migrate()?;
    db.upsert_account(&Account::new(account_id, user, ProtocolKind::Jmap))?;
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

/// 推送唤醒驱动 → 驱动自己把新邮件增量同步进本地库。
#[tokio::test]
async fn live_push_drives_incremental_sync_into_local_db() {
    let cfg = live_or_skip!();
    let provider = Arc::new(connect(&cfg));
    let account = provider.client().account_id().await.expect("取账号失败");

    let db = seed_db(&provider, &account, &cfg.user)
        .await
        .expect("seed 本地库失败");

    // 1) 先建订阅（订阅返回即代表连接已建立），再拉初始快照：
    //    顺序反了会静默丢掉两者之间的变化。
    let cancel = CancellationToken::new();
    let mut watcher = JmapWatcher::subscribe(
        &provider,
        PushConfig::for_types(&["Email", "Mailbox"]),
        cancel.clone(),
    )
    .await
    .expect("订阅推送失败（服务端未提供 eventSourceUrl？）");

    let engine = crossmail_sync::SyncEngine::new(Arc::clone(&provider), Arc::clone(&db));
    let initial = engine
        .sync(&account, None, cancel.clone(), None)
        .await
        .expect("初始快照同步失败");
    println!(
        "初始快照: inserted={} updated={} unchanged={} state={:?}",
        initial.stats.inserted, initial.stats.updated, initial.stats.unchanged, initial.new_state
    );

    // 2) 制造一次真实变化：新建草稿
    let tag = format!("CrossMail-js-{}", std::process::id());
    let mut msg = OutgoingMessage::new(
        "live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} 推送驱动同步"),
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
        ..PushSyncConfig::default()
    };
    let driver = run_push_sync(
        &engine,
        &account,
        &mut watcher,
        &push_cfg,
        cancel.clone(),
        move |event| {
            if let Ok(mut guard) = sink.lock() {
                guard.push(event);
            }
        },
    );

    // 4) 一边跑驱动，一边等库里出现这封邮件（45 秒上限：真机推送最迟 30 秒心跳量级）
    let wait_deadline = Instant::now() + Duration::from_secs(45);
    let found = tokio::select! {
        result = driver => panic!("驱动意外退出（说明流提前结束或同步失败）: {result:?}"),
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
    assert!(found, "45 秒内推送没有把新邮件带进本地库——驱动链路断了");

    let synced = events.lock().expect("events").clone();
    let runs = synced
        .iter()
        .filter(|e| matches!(e, PushSyncEvent::Synced { .. }))
        .count();
    assert!(
        runs >= 1,
        "至少要跑过一次推送驱动的同步，实际事件: {synced:?}"
    );

    // 落库内容要真的是这封信，而不是「库里恰好有条记录」
    let stored = db
        .lock()
        .expect("db")
        .get_email(&draft_id)
        .expect("读库失败")
        .expect("库里应该有这封邮件");
    assert_eq!(stored.subject, format!("{tag} 推送驱动同步"));
    assert!(
        stored.mailbox_ids.iter().any(|m| !m.is_empty()),
        "邮件必须挂在某个文件夹上: {:?}",
        stored.mailbox_ids
    );
    println!(
        "推送驱动的同步完成：runs={runs}，邮件已落库（文件夹 {:?}）",
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
