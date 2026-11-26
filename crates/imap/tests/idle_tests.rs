//! IDLE 推送离线验证（S6-14）：用内存双工流精确复现协议时序。
//!
//! 这里刻意不复用 `MockIo`——IDLE 的核心难点是**读写必须并行**（等推送的同时要能发
//! `DONE`），脚本化通道做不到，只有真正的双工流才能证明「发送 `DONE` 时读路径不被打断」。
//!
//! 每个用例都在验证一条**做错就真出事**的语义，而不是为了凑覆盖率：
//! 服务端静默吞命令、无心跳、断线不补推、`DONE` 之后还有积压变更。

use crossmail_core::error::ErrorKind;
use crossmail_core::provider::CancellationToken;
use crossmail_core::watch::{RemoteSignal, RemoteWatcher, DATA_TYPE_EMAIL};
use crossmail_imap::idle::{testing::MemoryIdleTransport, IdleConfig, ImapWatcher};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

/// 脚本化 IMAP 服务端（对话另一端）。
struct Fake {
    r: BufReader<DuplexStream>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    async fn read(&mut self) -> String {
        let mut line = String::new();
        let n = self.r.read_line(&mut line).await.expect("读取客户端命令");
        assert!(n > 0, "客户端意外关闭连接");
        let line = line.trim_end().to_string();
        if let Ok(mut g) = self.seen.lock() {
            g.push(line.clone());
        }
        line
    }

    async fn say(&mut self, line: &str) {
        let s = self.r.get_mut();
        s.write_all(format!("{line}\r\n").as_bytes())
            .await
            .expect("写响应");
        s.flush().await.expect("刷新");
    }

    /// 问候语**故意不带 IDLE**——真机就是这样（登录前 9 项、登录后 52 项）。
    async fn greet_without_idle(&mut self) {
        self.say("* OK [CAPABILITY IMAP4rev1 SASL-IR AUTH=PLAIN AUTH=LOGIN] ready")
            .await;
    }

    /// 走完 认证 → 取能力 → SELECT → 进入 IDLE，返回 IDLE 用的 tag。
    ///
    /// tag 只校验「是 IDLE 命令」而不写死具体值：watcher 的 tag 计数跨连接累加，
    /// 重连那一轮的 tag 与首轮不同，写死会把「实现正确」误判成失败。
    async fn handshake(&mut self, advertise_idle: bool) -> String {
        // 客户端此时 already 拿到问候语，第一条命令是 LOGIN
        let login = self.read().await;
        let t = login.split_whitespace().next().unwrap_or("A1").to_string();
        let post_login_caps = if advertise_idle {
            "IMAP4rev1 IDLE CONDSTORE UIDPLUS MOVE"
        } else {
            "IMAP4rev1"
        };
        self.say(&format!(
            "{t} OK [CAPABILITY {post_login_caps}] Authentication successful"
        ))
        .await;

        // provider 强制在认证后重取能力
        let cap = self.read().await;
        let cap_tag = cap.split_whitespace().next().unwrap_or("A2").to_string();
        self.say(&format!("* CAPABILITY {post_login_caps}")).await;
        self.say(&format!("{cap_tag} OK CAPABILITY completed"))
            .await;

        // SELECT
        let sel = self.read().await;
        let sel_tag = sel.split_whitespace().next().unwrap_or("A3").to_string();
        self.say("* 5 EXISTS").await;
        self.say("* 0 RECENT").await;
        self.say("* OK [UIDVALIDITY 42] UIDs valid").await;
        self.say("* OK [UIDNEXT 6] Next predicted UID").await;
        self.say(&format!("{sel_tag} OK [READ-WRITE] SELECT completed"))
            .await;

        // IDLE
        let idle = self.read().await;
        let mut parts = idle.split_whitespace();
        let idle_tag = parts.next().unwrap_or("?").to_string();
        assert_eq!(
            parts.next(),
            Some("IDLE"),
            "第 4 条命令应是 IDLE，实际: {idle}"
        );
        self.say("+ Idling, send 'DONE' to stop.").await;
        idle_tag
    }
}

fn cfg_long() -> IdleConfig {
    IdleConfig {
        mailbox: "INBOX".into(),
        idle_window: Duration::from_secs(30),
        reconnect_base: Duration::from_millis(1),
        reconnect_max: Duration::from_millis(5),
        cancel_poll: Duration::from_millis(10),
    }
}

fn cfg_short(window: Duration) -> IdleConfig {
    IdleConfig {
        idle_window: window,
        ..cfg_long()
    }
}

/// 起一个后台脚本服务端；返回 transport 与脚本命令日志。
fn spawn_server<F, Fut>(script: F) -> (Arc<MemoryIdleTransport>, Arc<Mutex<Vec<String>>>)
where
    F: FnOnce(Fake) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    let transport = MemoryIdleTransport::new();
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let shared = Arc::clone(&log);
    let t = Arc::clone(&transport);
    tokio::spawn(async move {
        let s = t.server_side().await;
        let fake = Fake {
            r: BufReader::new(s),
            seen: Arc::clone(&shared),
        };
        script(fake).await;
    });
    (transport, log)
}

async fn watcher(
    transport: Arc<MemoryIdleTransport>,
    cfg: IdleConfig,
    cancel: CancellationToken,
) -> ImapWatcher {
    ImapWatcher::subscribe(
        transport,
        "alice@example.com",
        "secret",
        "acc1",
        cfg,
        cancel,
    )
    .await
    .expect("订阅应成功")
}

/// 取一条信号，带超时护栏：状态机卡住时要**明确失败**，而不是让整个测试进程挂死。
async fn next(w: &mut ImapWatcher) -> RemoteSignal {
    match tokio::time::timeout(Duration::from_secs(5), w.next_signal()).await {
        Ok(Ok(Some(sig))) => sig,
        Ok(Ok(None)) => panic!("IMAP IDLE 不会正常结束，不该返回 None"),
        Ok(Err(e)) => panic!("next_signal 报错: {e}"),
        Err(_) => panic!("next_signal 超时 5s —— 状态机卡住了"),
    }
}

// ------------------------------------------------------------------ 建连与进入 idling

/// **契约**：`subscribe()` 返回时连接必须已经在 idling 状态。
///
/// 反序（先拉快照再进 IDLE）会在两者之间留出空档，期间的变化既不会被推送也不会报错，
/// 表现为「偶发丢邮件」，且日志里看不出任何异常。
#[tokio::test]
async fn subscribe_returns_only_after_entering_idling() {
    let (transport, log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        f.handshake(true).await;
        // 保持连接直到测试结束，别提前关闭
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let _w = watcher(transport, cfg_long(), CancellationToken::new()).await;
    let cmds = log.lock().unwrap().clone();
    assert!(
        cmds.last().is_some_and(|c| c.ends_with(" IDLE")),
        "subscribe 返回前必须已经发出 IDLE：{cmds:?}"
    );
}

/// 真机事实：`IDLE` 只在**登录后**的能力集里出现（登录前 9 项、登录后 52 项）。
/// 少了「认证后重取能力」这一步，这条连接会直接报 Unsupported，推送永远用不上。
#[tokio::test]
async fn idle_detected_only_after_authentication() {
    let (transport, _log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        f.handshake(true).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });
    // 不 panic 即为通过：登录前没有 IDLE，但订阅仍成功
    let _w = watcher(transport, cfg_long(), CancellationToken::new()).await;
}

/// 服务端确实不支持 IDLE 时要在订阅阶段就明确拒绝，而不是返回一个永远静默的 watcher。
#[tokio::test]
async fn unsupported_idle_is_rejected_at_subscribe() {
    let (transport, _log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        // 登录后依然没有 IDLE —— 只走到取能力，不该进入 SELECT/IDLE
        let login = f.read().await;
        let t = login.split_whitespace().next().unwrap_or("A1").to_string();
        f.say(&format!("{t} OK [CAPABILITY IMAP4rev1 UIDPLUS] ok"))
            .await;
        let cap = f.read().await;
        let ct = cap.split_whitespace().next().unwrap_or("A2").to_string();
        f.say("* CAPABILITY IMAP4rev1 UIDPLUS").await;
        f.say(&format!("{ct} OK done")).await;
    });

    let err = ImapWatcher::subscribe(
        transport,
        "a@b.com",
        "x",
        "acc1",
        cfg_long(),
        CancellationToken::new(),
    )
    .await
    .err()
    .expect("缺 IDLE 必须失败");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}

// ------------------------------------------------------------------ 推送内容映射

/// Stalwart 在 SELECTED 状态会推 `* STATUS (...) UIDNEXT n`，这是唯一能拿到准确 state 的行。
#[tokio::test]
async fn status_line_carries_uidnext_as_state() {
    let (transport, _log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        f.handshake(true).await;
        f.say(r#"* STATUS "INBOX" (MESSAGES 6 UNSEEN 1 UIDNEXT 7 UIDVALIDITY 42)"#)
            .await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut w = watcher(transport, cfg_long(), CancellationToken::new()).await;
    let sig = next(&mut w).await;
    assert_eq!(
        sig,
        RemoteSignal::changed("acc1", DATA_TYPE_EMAIL, Some("7".into())),
        "UIDNEXT 要作为 state 上报，供上层诊断与去重"
    );
}

/// `* n EXISTS` 没有 UIDNEXT，沿用已知值；关键是**必须**报变更，不能因为缺 state 就吞掉。
#[tokio::test]
async fn exists_reports_change_using_known_uidnext() {
    let (transport, _log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        f.handshake(true).await;
        f.say("* 6 EXISTS").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut w = watcher(transport, cfg_long(), CancellationToken::new()).await;
    let sig = next(&mut w).await;
    assert_eq!(sig.data_type(), Some(DATA_TYPE_EMAIL));
    assert_eq!(
        sig,
        RemoteSignal::changed("acc1", DATA_TYPE_EMAIL, Some("6".into())),
        "应回落到 SELECT 时记下的 UIDNEXT"
    );
}

/// 删除与标志位变化同样是「邮件变了」，不能只认新邮件。
#[tokio::test]
async fn expunge_and_fetch_also_report_change() {
    let (transport, _log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        f.handshake(true).await;
        f.say("* 5 EXPUNGE").await;
        f.say("* 4 FETCH (FLAGS (\\Seen) UID 4)").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let mut w = watcher(transport, cfg_long(), CancellationToken::new()).await;
    let first = next(&mut w).await;
    assert_eq!(first.data_type(), Some(DATA_TYPE_EMAIL));
    let second = next(&mut w).await;
    assert_eq!(second.data_type(), Some(DATA_TYPE_EMAIL));
}

/// **心跳不是数据**（契约约束 1）。`* OK Still here` 若冒到同步层，就是每 30 秒一次白拉。
#[tokio::test]
async fn ok_heartbeat_is_swallowed() {
    let (transport, _log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        f.handshake(true).await;
        for _ in 0..5 {
            f.say("* OK Still here").await;
        }
        f.say("* 6 EXISTS").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    let mut w = watcher(transport, cfg_long(), CancellationToken::new()).await;
    let sig = next(&mut w).await;
    assert_eq!(
        sig,
        RemoteSignal::changed("acc1", DATA_TYPE_EMAIL, Some("6".into())),
        "心跳之后的第一条有效信号必须是 EXISTS 翻出来的变更"
    );
}

// ------------------------------------------------------------------ 窗口到期重入

/// 服务端不发心跳，所以必须自己按窗口主动 `DONE` 再重入；重入期间收到的变更不能丢。
///
/// 丢了会怎样：`DONE` 之后服务端把积压的 untagged 一口气吐出来，那是最新的一批变化，
/// 丢弃等于用户要等下一次触发才看到邮件。
#[tokio::test]
async fn window_expiry_reenters_idle_and_keeps_changes() {
    let (transport, log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        let idle_tag = f.handshake(true).await;

        // 等客户端因窗口到期发 DONE
        let done = f.read().await;
        assert_eq!(done, "DONE", "窗口到期必须发 DONE");
        // DONE 之后服务端把积压的变更吐出来
        f.say("* 7 EXISTS").await;
        f.say(&format!("{idle_tag} OK IDLE completed")).await;
        // 然后必须重新进入 IDLE
        let again = f.read().await;
        assert!(
            again.ends_with(" IDLE"),
            "DONE 之后必须重新 IDLE，实际: {again}"
        );
        assert_ne!(again, format!("{idle_tag} IDLE"), "重入必须换新 tag");
        f.say("+ Idling, send 'DONE' to stop.").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    let mut w = watcher(
        transport,
        cfg_short(Duration::from_millis(120)),
        CancellationToken::new(),
    )
    .await;

    let sig = next(&mut w).await;
    assert_eq!(
        sig.data_type(),
        Some(DATA_TYPE_EMAIL),
        "重入期间的变更必须保留"
    );

    let cmds = log.lock().unwrap().clone();
    assert!(cmds.iter().any(|c| c == "DONE"), "应发过 DONE：{cmds:?}");
    assert!(
        cmds.iter().filter(|c| c.ends_with(" IDLE")).count() >= 2,
        "应重入过 IDLE：{cmds:?}"
    );
}

// ------------------------------------------------------------------ 断线重连

/// **断线不补推**（契约约束 3）：重连成功必须发 `Resync`，让上层重新拉取对齐。
///
/// 这条写错的典型症状是「断网恢复后新邮件一直不出现」，而日志里一切正常——
/// 因为从协议角度看确实什么都没发生。脚本分两轮，各自对应一次 open。
#[tokio::test]
async fn reconnect_emits_resync_across_two_connections() {
    let transport = MemoryIdleTransport::new();
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let t = Arc::clone(&transport);
        let shared = Arc::clone(&log);
        tokio::spawn(async move {
            // 第 1 轮
            let s1 = t.server_side().await;
            let mut f = Fake {
                r: BufReader::new(s1),
                seen: Arc::clone(&shared),
            };
            f.greet_without_idle().await;
            f.handshake(true).await;
            drop(f); // 直接断链

            // 第 2 轮（重连）
            let s2 = t.server_side().await;
            let mut f2 = Fake {
                r: BufReader::new(s2),
                seen: Arc::clone(&shared),
            };
            f2.greet_without_idle().await;
            f2.handshake(true).await; // 新连接 tag 从头开始
            tokio::time::sleep(Duration::from_millis(400)).await;
        });
    }

    let mut w = watcher(Arc::clone(&transport), cfg_long(), CancellationToken::new()).await;
    let sig = next(&mut w).await;
    assert_eq!(
        sig,
        RemoteSignal::Resync,
        "重连后第一条信号必须是 Resync，不能假装「没变化」"
    );
    assert_eq!(transport.opens(), 2, "应恰好重连一次");
}

// ------------------------------------------------------------------ 取消

/// 取消必须能打断正在等待的 IDLE（否则关应用要等满 25 分钟窗口）。
#[tokio::test]
async fn cancellation_interrupts_waiting_idle() {
    let (transport, _log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        f.handshake(true).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
    });

    let cancel = CancellationToken::new();
    let mut w = watcher(transport, cfg_long(), cancel.clone()).await;
    cancel.cancel();

    let err = w.next_signal().await.expect_err("取消必须上报");
    assert_eq!(err.kind(), ErrorKind::Cancelled);
}

/// IDLE 期间**只允许** `DONE` / 重新 `IDLE`：真机上发其他命令会被服务端静默吞掉，
/// 那条命令的完成响应永远不来，等它就会把整条连接挂死。
#[tokio::test]
async fn idling_never_sends_other_commands() {
    let (transport, log) = spawn_server(|mut f| async move {
        f.greet_without_idle().await;
        f.handshake(true).await;
        // 推几条变更，逼 watcher 反复走 next_signal 的读路径
        f.say("* 6 EXISTS").await;
        f.say("* 7 EXISTS").await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    });

    let mut w = watcher(
        transport,
        cfg_short(Duration::from_secs(10)),
        CancellationToken::new(),
    )
    .await;
    let _ = next(&mut w).await;
    let _ = next(&mut w).await;

    let cmds = log.lock().unwrap().clone();
    // 前 3 条是握手命令，之后只允许 DONE / IDLE
    for c in cmds.iter().skip(3) {
        let upper = c.to_ascii_uppercase();
        assert!(
            upper.ends_with("IDLE") || upper == "DONE",
            "idling 期间不允许发其他命令，实际发了: {c}（全部命令 {cmds:?}）"
        );
    }
}
