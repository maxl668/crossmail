//! S6-17 真机端到端：**RFC 8887 WebSocket 推送（wss://…/jmap/ws）**。
//!
//! SSE 侧的推送（`live_push_sync`）已真机验证过；本用例验证的是第二条推送通道：
//!
//! > 真实 Stalwart 声明 `urn:ietf:params:jmap:websocket`（supportsPush: true），
//! > watcher 按能力优先选 WS → 握手带 `Sec-WebSocket-Protocol: jmap` + Basic 认证
//! > → 发 `WebSocketPushEnable` 注册 → 服务器上真实新建一封邮件 →
//! > StateChange 从 **WS 文本帧**送达 → 翻译成 `RemoteSignal::Changed`。
//!
//! 任何一步不支持（握手拒/注册拒/无事件），用例都会失败而不是静默降级——
//! 通道是否真的在用，由 `using_websocket()` 与信号送达共同证明。
//!
//! 默认静默跳过（没设 `CROSSMAIL_LIVE_JMAP_URL`）。会真的写数据（建一个草稿并销毁），
//! 用固定前缀 `CrossMail-ws-` 便于人工识别残留。
//!
//! 注意：与 `live_push_sync.rs` 共用同一个真实账号，必须 `--test-threads=1`。

use chrono::Utc;
use crossmail_core::error::MailResult;
use crossmail_core::model::{Account, Address, OutgoingMessage, ProtocolKind};
use crossmail_core::provider::{CancellationToken, MailProvider};
use crossmail_core::watch::{RemoteSignal, RemoteWatcher, DATA_TYPE_EMAIL};
use crossmail_jmap::push::PushConfig;
use crossmail_jmap::watcher::JmapWatcher;
use crossmail_jmap::{AuthMethod, JmapProvider};
use std::sync::Arc;
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

/// 真机 WS 推送：注册后服务器上的真实变更必须从 WebSocket 帧送达。
#[tokio::test]
async fn live_websocket_push_delivers_real_state_change() {
    let cfg = live_or_skip!();
    let provider = Arc::new(JmapProvider::new(
        cfg.url.clone(),
        AuthMethod::basic(cfg.user.clone(), cfg.pass.clone()),
        cfg.user.clone(),
    ));

    // 1) 订阅：watcher 按能力协商选通道。真机声明了 WS 能力，必须选 WS。
    let cancel = CancellationToken::new();
    let mut watcher = JmapWatcher::subscribe(
        &provider,
        PushConfig::for_types(&["Email", "Mailbox"]),
        cancel.clone(),
    )
    .await
    .expect("订阅推送失败");
    assert!(
        watcher.using_websocket(),
        "真机声明了 urn:ietf:params:jmap:websocket，订阅应走 WebSocket 通道"
    );

    // 2) 建库落服务器文件夹（邮件表对文件夹有外键；真实变更要落库的前提）。
    let account = provider.client().account_id().await.expect("取账号失败");
    let mut db = crossmail_storage::Database::open_in_memory().expect("建库失败");
    db.migrate().expect("迁移失败");
    db.upsert_account(&Account::new(&account, &cfg.user, ProtocolKind::Jmap))
        .expect("落账号失败");
    for mailbox in provider.list_mailboxes().await.expect("列文件夹失败") {
        db.upsert_mailbox(&mailbox).expect("落文件夹失败");
    }

    // 3) 制造一次真实变化：新建草稿（与 SSE 版用例相同的触发手段）。
    let tag = format!("CrossMail-ws-{}", std::process::id());
    let mut msg = OutgoingMessage::new(
        "live-acct",
        Address::new(&cfg.user),
        vec![Address::new(&cfg.user)],
        format!("{tag} WS 推送"),
    );
    msg.text_body = Some(format!("created at {:?}", Utc::now()));
    let draft_id = provider.save_draft(&msg).await.expect("建草稿失败");
    println!("已建草稿 emailId = {draft_id}");

    // 4) 等 StateChange 从 WS 帧送达（45 秒上限：与 SSE 版同一量级）。
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut saw_email_change = false;
    while Instant::now() < deadline {
        let signal = tokio::select! {
            signal = watcher.next_signal() => signal.expect("读信号失败"),
            _ = tokio::time::sleep(Duration::from_millis(500)) => None,
        };
        match signal {
            Some(RemoteSignal::Changed { data_type, .. }) if data_type == DATA_TYPE_EMAIL => {
                println!("WS 通道收到 {data_type} 变更信号");
                saw_email_change = true;
                break;
            }
            Some(RemoteSignal::Resync) => {
                // 订阅已建成才建草稿，不该有断线空档；出现即说明通道异常。
                panic!("订阅建成后的首条信号不该是 Resync（说明连接断过）");
            }
            Some(RemoteSignal::Closed) => {
                panic!("WS 推送流意外结束（被服务器关闭？）");
            }
            Some(other) => {
                println!("忽略非 Email 信号: {other:?}");
            }
            None => {
                // select 超时分支继续轮询
            }
        }
    }
    cancel.cancel();

    assert!(
        saw_email_change,
        "45 秒内 WebSocket 通道没有收到 Email 变更信号"
    );

    // 5) 清理：销毁这封草稿。
    if let Err(e) = provider
        .delete_emails(std::slice::from_ref(&draft_id))
        .await
    {
        eprintln!("清理草稿失败（残留可用前缀 {tag} 人工识别）: {e}");
    } else {
        println!("草稿已清理");
    }
}

/// 会话能读到 WS 端点与能力声明（排障入口），且真机确实声明 supportsPush。
#[tokio::test]
async fn live_session_declares_websocket_capability() {
    let cfg = live_or_skip!();
    let provider = JmapProvider::new(
        cfg.url.clone(),
        AuthMethod::basic(cfg.user.clone(), cfg.pass.clone()),
        cfg.user.clone(),
    );
    let (es, ws) = provider.push_endpoints().await.expect("取推送端点失败");
    println!("eventSource = {es:?}");
    println!("websocket  = {ws:?}");
    let ws_url = ws.expect("真机应声明 WebSocket 推送端点");
    assert!(
        ws_url.starts_with("wss://") || ws_url.starts_with("ws://"),
        "{ws_url}"
    );
    let session = provider.client().session().await.expect("session 失败");
    assert!(
        session.has_capability(crossmail_jmap::session::WEBSOCKET_CAPABILITY),
        "session capabilities 应含 urn:ietf:params:jmap:websocket"
    );
}

/// 静默编译期锚点：确保 MailResult 未被误删（防止无用 import 告警漂移）。
#[allow(dead_code)]
fn _mail_result_anchor(_: MailResult<()>) {}
