//! S6-16 真机端到端：**真连服务器触发的 `Synced` 带真实未读角标**。
//!
//! 与 `jmap/tests/live_push_sync.rs` 的分工：那条证明「推送 → 增量同步 → 落库」，
//! 本条证明再往上一层——编排层 `subscribe_push` 收敛出的 `PushNotice::Synced`
//! 把**真实未读角标**带了出来，而不是编造的 0 / 拍脑袋的常量。
//!
//! 真值来源：派发线程回调时，`to_notice` 刚从同一份 `db` 读完角标；我们在回调里
//! **再独立读一次** `Database::unread_counts` 并随事件一起存下来，最后逐项比对。
//! 若将来有人把 `read_unread` 改成恒返回 0，只要库里真有未读邮件，本用例就会挂。
//!
//! 默认静默跳过（没设 `CROSSMAIL_LIVE_JMAP_URL/USER/PASS`）。会真的写数据（建草稿
//! 并销毁），前缀 `CrossMail-live-synced-` 便于人工识别残留。与 jmap live 用例共用
//! 同一真实账号，必须 `--test-threads=1`。
//!
//! 时序：worker 异步建连且未暴露「已就绪」信号，故不猜延迟——**轮询注入草稿直到
//! 收到 Synced**。每次注入都会产生一次 JMAP StateChange，只要 watcher 建连后有一次
//! 注入落在循环里，就必然触发一次推送驱动的同步。

use chrono::Utc;
use crossmail_app::{subscribe_push, PushNotice, PushReason};
use crossmail_core::error::MailResult;
use crossmail_core::model::{
    Account, Address, Credential, JmapConfig, OutgoingMessage, ProtocolKind,
};
use crossmail_core::provider::MailProvider;
use crossmail_jmap::{AuthMethod, JmapProvider};
use crossmail_storage::Database;
use std::sync::{mpsc, Arc, Mutex};
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
    if url.trim().is_empty() || user.trim().is_empty() || pass.trim().is_empty() {
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

fn connect(cfg: &LiveConfig) -> Arc<JmapProvider> {
    let auth = AuthMethod::basic(cfg.user.clone(), cfg.pass.clone());
    Arc::new(JmapProvider::new(cfg.url.clone(), auth, cfg.user.clone()))
}

/// 用服务器返回的真实 accountId / mailbox id 落库（邮件表对文件夹有外键，
/// 且 JMAP 同步时邮件带的 mailboxIds 必须能在本地找到，否则角标读不出来）。
async fn seed_db(
    provider: &JmapProvider,
    account_id: &str,
    user: &str,
) -> MailResult<Arc<Mutex<Database>>> {
    let mut db = Database::open_in_memory()?;
    db.migrate()?;
    db.upsert_account(&Account::new(
        account_id.to_string(),
        user,
        ProtocolKind::Jmap,
    ))?;
    for mailbox in provider.list_mailboxes().await? {
        db.upsert_mailbox(&mailbox)?;
    }
    Ok(Arc::new(Mutex::new(db)))
}

/// `build_backend` 只认 endpoint + email_address + secret，credential 是凭证仓元数据占位。
fn push_account(account_id: &str, cfg: &LiveConfig) -> Account {
    let mut account = Account::new(account_id.to_string(), cfg.user.clone(), ProtocolKind::Jmap);
    account.jmap = Some(JmapConfig {
        endpoint: cfg.url.clone(),
        credential: Credential::Keyring {
            alias: "live-test".into(),
        },
    });
    account
}

type Carried = (PushNotice, u64, Vec<(String, u64)>);

/// 真连服务器触发的 Synced 必须携带与本地库一致的真实未读角标。
#[test]
fn live_real_synced_notice_carries_true_unread_badge() {
    let cfg = live_or_skip!();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建 runtime");

    rt.block_on(async move {
        let provider = connect(&cfg);
        let account_id = provider
            .client()
            .account_id()
            .await
            .expect("取 JMAP accountId 失败");
        let db = seed_db(&provider, &account_id, &cfg.user)
            .await
            .expect("seed 本地库失败");

        // 回调里随事件一起抓一份库真值快照：与 to_notice 的 read_unread 是两次独立读。
        let db_for_cb = Arc::clone(&db);
        let aid_for_cb = account_id.clone();
        let (tx, rx) = mpsc::channel::<Carried>();
        let handle = subscribe_push(
            Arc::clone(&db),
            push_account(&account_id, &cfg),
            cfg.pass.clone(),
            move |notice| {
                let truth = db_for_cb
                    .lock()
                    .expect("db 锁")
                    .unread_counts(&aid_for_cb)
                    .unwrap_or_default();
                let total: u64 = truth.values().map(|n| u64::from(*n)).sum();
                let by: Vec<(String, u64)> = truth
                    .into_iter()
                    .map(|(id, n)| (id, u64::from(n)))
                    .collect();
                let _ = tx.send((notice, total, by));
            },
        )
        .expect("订阅应立即返回句柄");

        // 轮询注入：worker 异步建连，前几次可能落在建连前；只要有一次落在循环里即可。
        let mut created: Vec<String> = Vec::new();
        let mut carried: Option<Carried> = None;
        let deadline = Instant::now() + Duration::from_secs(60);
        for i in 0..8 {
            let tag = format!("CrossMail-live-synced-{}-{i}", std::process::id());
            let mut msg = OutgoingMessage::new(
                account_id.clone(),
                Address::new(&cfg.user),
                vec![Address::new(&cfg.user)],
                tag.clone(),
            );
            msg.text_body = Some(format!("created at {:?}", Utc::now()));
            match provider.save_draft(&msg).await {
                Ok(id) => created.push(id),
                Err(e) => eprintln!("注入草稿失败（{tag}）: {e}"),
            }

            let until = Instant::now() + Duration::from_secs(4);
            while Instant::now() < until {
                if let Ok(item) = rx.try_recv() {
                    if matches!(item.0, PushNotice::Synced { .. }) && carried.is_none() {
                        carried = Some(item);
                    }
                }
                if carried.is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if carried.is_some() || Instant::now() >= deadline {
                break;
            }
        }

        handle.cancel();
        handle.join();
        // 真服务器数据必须清理；清理失败只留告警，前缀固定便于人工识别。
        if !created.is_empty() {
            if let Err(e) = provider.delete_emails(&created).await {
                eprintln!("清理注入草稿失败（前缀 CrossMail-live-synced-）: {e}");
            }
        }

        let (notice, truth_total, truth_by) = carried
            .unwrap_or_else(|| panic!("60 秒内未收到真实 Synced 事件——推送驱动同步链路未触发"));
        let PushNotice::Synced {
            reason,
            unread_total,
            unread_by_mailbox,
            ..
        } = notice
        else {
            unreachable!("已按 Synced 过滤");
        };

        // 前提自检：库里必须真有未读，否则「角标是真值」这件事证明不了。
        assert!(
            truth_total > 0,
            "真机库里应有未读邮件，否则无法证明角标不是编造的 0；truth_by={truth_by:?}"
        );
        assert_eq!(reason, PushReason::Push, "应由真实推送信号触发");
        assert_eq!(
            unread_total, truth_total,
            "事件角标合计必须等于同一时刻库里独立读出的真值"
        );
        assert_eq!(
            unread_total,
            unread_by_mailbox.iter().map(|m| m.unread).sum::<u64>(),
            "角标合计必须与分箱明细自洽"
        );
        assert_eq!(
            unread_by_mailbox.len(),
            truth_by.len(),
            "分箱条目数应与库真值一致: event={unread_by_mailbox:?} truth={truth_by:?}"
        );
        for m in &unread_by_mailbox {
            let expected = truth_by
                .iter()
                .find(|(id, _)| id == &m.mailbox_id)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            assert_eq!(
                m.unread, expected,
                "分箱 {} 的角标应与库真值一致",
                m.mailbox_id
            );
        }
        println!(
            "真连 Synced 角标已核对：total={unread_total}，分箱 {} 个，注入草稿 {} 封",
            unread_by_mailbox.len(),
            created.len()
        );
    });
}
