//! IMAP IDLE 推送（需求 I-008）：把「选中文件夹有新变化」变成协议无关的 `RemoteSignal`。
//!
//! 与 JMAP EventSource 相比，IDLE 有三处**真机倒逼**的特殊性，实现必须正面处理：
//!
//! 1. **IDLE 期间服务端会静默吞掉非 `DONE` 命令**。本机 Stalwart 实测：IDLE 中发 `NOOP`，
//!    连一个字节都不回，那条命令的完成响应永远不来。所以「等待推送」与「发送 `DONE`」
//!    必须走**两条互不干扰的通道**（读半边 + 写半边），且状态机在 idling 期间绝不允许
//!    发其他命令——否则等 tag 会永久挂死。
//! 2. **服务端不发心跳**。IDLE 空转 20 秒完全静默，无人能借此判断连接是否还活着。
//!    因此必须自己按 `idle_window`（默认 25 分钟，RFC 2177 建议小于 29 分钟）主动
//!    `DONE` 再重入，既当保活探测，也让「假死连接」有机会暴露出来。
//! 3. **断线不补推**。IDLE 只推「此刻正在发生」的变化，断线期间的变更一条都不会重放。
//!    所以重连成功后必须立刻发一条 [`RemoteSignal::Resync`]，让上层重新拉取对齐——
//!    绝不能因为「没收到变化」就推断「没有变化」。
//!
//! 读路径上还有一个容易写错的地方：`read_line` 若被 `select!` 取消，已读进缓冲的字节
//! 会连同 future 一起丢掉，之后整条连接的行边界全部错位。因此这里**不直接读**，而是
//! 用一个只由自己拥有的后台任务把行泵进 channel；取消只影响 `recv()`，字节安全。

use crate::client::ImapClient;
use crate::provider::ImapProvider;
use crate::response::{self, Untagged};
use crate::transport::{connect_tls_endpoint, AsyncStream, Endpoint, ImapIo};
use async_trait::async_trait;
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::provider::CancellationToken;
use crossmail_core::watch::{RemoteSignal, RemoteWatcher, DATA_TYPE_EMAIL};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::sync::{mpsc, Mutex};

/// IDLE 配置。
#[derive(Debug, Clone)]
pub struct IdleConfig {
    /// 要监视的文件夹（IMAP 文件夹名，与 `Mailbox::id` 一致）。
    pub mailbox: String,
    /// 单次 IDLE 最长时长；到期主动 `DONE` 再重入，兼作保活探测。
    pub idle_window: Duration,
    /// 断线重连的初始退避。
    pub reconnect_base: Duration,
    /// 断线重连的退避上限。
    pub reconnect_max: Duration,
    /// 取消检查粒度（窗口等待被切成小片，保证取消能及时生效）。
    pub cancel_poll: Duration,
}

impl IdleConfig {
    /// 监视某个文件夹，其余取默认值。
    pub fn for_mailbox(mailbox: impl Into<String>) -> Self {
        Self {
            mailbox: mailbox.into(),
            // RFC 2177 §3：客户端至少要每 29 分钟重新发起一次 IDLE。
            idle_window: Duration::from_secs(25 * 60),
            reconnect_base: Duration::from_millis(200),
            reconnect_max: Duration::from_secs(30),
            cancel_poll: Duration::from_millis(100),
        }
    }

    /// 测试用：窗口短、退避快。
    pub fn quick_for_test(mailbox: impl Into<String>) -> Self {
        Self {
            mailbox: mailbox.into(),
            idle_window: Duration::from_millis(300),
            reconnect_base: Duration::from_millis(1),
            reconnect_max: Duration::from_millis(5),
            cancel_poll: Duration::from_millis(10),
        }
    }

    /// 第 `attempt` 次重连前的等待（指数退避，封顶 `reconnect_max`）。
    pub fn backoff(&self, attempt: u32) -> Duration {
        let raw = self.reconnect_base.as_millis() as u64 * 2u64.saturating_pow(attempt.min(16));
        Duration::from_millis(raw.min(self.reconnect_max.as_millis() as u64))
    }
}

/// 分半之后的协议通道：读走后台泵，写可随时取用。
///
/// 两个半边都用 `Arc<Mutex<_>>` 共享：`ImapClient` 在握手阶段正常收发，IDLE 阶段则由
/// watcher 直接操作同一对半边（此时 client 不会再用，不存在竞争）。
struct SplitIo {
    lines: Arc<Mutex<mpsc::Receiver<MailResult<String>>>>,
    writer: Arc<Mutex<WriteHalf<Box<dyn AsyncStream>>>>,
}

#[async_trait]
impl ImapIo for SplitIo {
    async fn write_line(&mut self, line: &str) -> MailResult<()> {
        let mut data = line.as_bytes().to_vec();
        data.extend_from_slice(b"\r\n");
        self.write_bytes(&data).await
    }

    async fn write_bytes(&mut self, data: &[u8]) -> MailResult<()> {
        let mut w = self.writer.lock().await;
        w.write_all(data)
            .await
            .map_err(|e| MailError::Network(format!("写入失败: {e}")))?;
        w.flush()
            .await
            .map_err(|e| MailError::Network(format!("刷新失败: {e}")))
    }

    async fn read_line(&mut self) -> MailResult<String> {
        let mut rx = self.lines.lock().await;
        match rx.recv().await {
            Some(item) => item,
            None => Err(MailError::Network("连接已被服务端关闭".into())),
        }
    }

    /// IDLE 连接只跑 LOGIN / SELECT / CAPABILITY / IDLE，这些命令的响应里不会出现 literal。
    /// 真出现就是协议错位，宁可立刻报错也不要静默吞掉后把后续响应全错位。
    async fn read_literal(&mut self, _len: usize) -> MailResult<Vec<u8>> {
        Err(MailError::Protocol(
            "IDLE 通道不接受 literal 响应（说明响应已错位）".into(),
        ))
    }
}

/// 把读半边泵成行的后台任务：只有它碰底层读缓冲，因此永远不会被 `select!` 取消。
fn spawn_line_pump(
    reader: BufReader<ReadHalf<Box<dyn AsyncStream>>>,
) -> mpsc::Receiver<MailResult<String>> {
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        let mut reader = reader;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => {
                    let _ = tx
                        .send(Err(MailError::Network("连接已被服务端关闭".into())))
                        .await;
                    return;
                }
                Ok(_) => {
                    // 行内容按 UTF-8 宽松解码：IMAP 允许原始 8bit，严格解码会把
                    // 一封含非 UTF-8 标题的通知变成整条连接的致命错误。
                    let mut end = buf.len();
                    while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
                        end -= 1;
                    }
                    let line = String::from_utf8_lossy(&buf[..end]).to_string();
                    if tx.send(Ok(line)).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(Err(MailError::Network(format!("读取失败: {e}"))))
                        .await;
                    return;
                }
            }
        }
    });
    rx
}

/// IDLE 连接的建立方式。
///
/// 抽出来只为一个目的：**重连后的 `Resync` 语义必须能离线验证**。断线不补推是 IDLE 最
/// 容易写错的地方（漏了它，用户会看到「新邮件要等下一次定时同步才出现」，而且日志里
/// 一切正常）。真连很难稳定复现断线，所以把「怎么建连」抽成可注入的，测试里用内存
/// 双工流就能把「断开 → 重连 → 发 Resync」整条路径跑完。
#[async_trait]
pub trait IdleTransport: Send + Sync {
    /// 建立一条新连接，返回**尚未认证**的裸字节流。
    async fn open(&self) -> MailResult<Box<dyn AsyncStream>>;
}

/// 生产实现：按端点建 TLS。
pub struct TlsIdleTransport {
    ep: Endpoint,
}

impl TlsIdleTransport {
    pub fn new(ep: Endpoint) -> Self {
        Self { ep }
    }
}

#[async_trait]
impl IdleTransport for TlsIdleTransport {
    async fn open(&self) -> MailResult<Box<dyn AsyncStream>> {
        let (stream, leftover) = connect_tls_endpoint(&self.ep).await?.into_inner();
        if !leftover.is_empty() {
            // 建连后不该有残留字节；有就说明读缓冲与这条流不属于同一条连接，
            // 继续用会让整条流从第一帧起就错位。
            return Err(MailError::Protocol(format!(
                "建连后残留 {} 字节未消费数据，拒绝继续",
                leftover.len()
            )));
        }
        Ok(stream)
    }
}

/// 测试辅助：内存双工流，可精确复现协议时序与断线。
pub mod testing {
    use super::*;
    use std::collections::VecDeque;

    /// 每次 `open()` 造一对内存流：客户端侧交给 watcher，服务端侧交回测试脚本。
    #[derive(Default)]
    pub struct MemoryIdleTransport {
        servers: Mutex<VecDeque<tokio::io::DuplexStream>>,
        opens: std::sync::atomic::AtomicUsize,
    }

    impl MemoryIdleTransport {
        pub fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        /// 取第 `n` 条连接的服务端侧（`open()` 之后才有）。
        pub async fn server_side(&self) -> tokio::io::DuplexStream {
            loop {
                if let Some(s) = self.servers.lock().await.pop_front() {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }

        /// 已建立的连接数（重连次数 = 它 − 1）。
        pub fn opens(&self) -> usize {
            self.opens.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl IdleTransport for MemoryIdleTransport {
        async fn open(&self) -> MailResult<Box<dyn AsyncStream>> {
            let (client, server) = tokio::io::duplex(8192);
            self.servers.lock().await.push_back(server);
            self.opens.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(client))
        }
    }
}

/// 一次已进入 idling 状态的连接。
struct IdleConn {
    writer: Arc<Mutex<WriteHalf<Box<dyn AsyncStream>>>>,
    lines: Arc<Mutex<mpsc::Receiver<MailResult<String>>>>,
    /// 当前 IDLE 使用的 tag（用 `I` 前缀与 `ImapClient` 的 `A` 前缀分开，便于诊断）。
    idle_tag: String,
    /// 本次 IDLE 何时该主动 `DONE`。
    deadline: tokio::time::Instant,
    /// 已知的 `UIDNEXT`，作为 `state` 上报（仅为诊断/去重，真游标由同步页回传）。
    uid_next: Option<u32>,
}

/// `wake` 的三种结果。
enum Wake {
    Line(String),
    WindowExpired,
    Cancelled,
}

/// IMAP IDLE 推送源。
pub struct ImapWatcher {
    transport: Arc<dyn IdleTransport>,
    username: String,
    password: String,
    account_id: String,
    cfg: IdleConfig,
    cancel: CancellationToken,
    conn: Option<IdleConn>,
    /// 窗口到期重入期间顺带收到的变更先攒着，等下一条 `next_signal` 直接吐出。
    pending: VecDeque<RemoteSignal>,
    tag_seq: u32,
    reconnect_attempt: u32,
}

impl ImapWatcher {
    /// 建立监视并**在返回前进入 IDLE**。
    ///
    /// 顺序是契约的一部分：先进入 IDLE、再去拉初始快照。反序会在「快照之后、IDLE 之前」
    /// 留出空档，期间的变更既不会被推送也不会报错，等于静默丢事件。
    pub async fn subscribe(
        transport: Arc<dyn IdleTransport>,
        username: &str,
        password: &str,
        account_id: impl Into<String>,
        cfg: IdleConfig,
        cancel: CancellationToken,
    ) -> MailResult<Self> {
        let mut watcher = Self {
            transport,
            username: username.to_string(),
            password: password.to_string(),
            account_id: account_id.into(),
            cfg,
            cancel,
            conn: None,
            pending: VecDeque::new(),
            tag_seq: 0,
            reconnect_attempt: 0,
        };
        // 首次建连失败直接上报，不做退避重试：认证错/能力缺失这种事重试一万次也一样，
        // 捂着反而让上层以为在正常等待推送。
        watcher.establish().await?;
        Ok(watcher)
    }

    fn next_tag(&mut self) -> String {
        self.tag_seq += 1;
        format!("I{}", self.tag_seq)
    }

    /// 从行通道读一行；`None` 表示连接已结束。
    async fn recv_line(
        lines: &Arc<Mutex<mpsc::Receiver<MailResult<String>>>>,
    ) -> Option<MailResult<String>> {
        let mut rx = lines.lock().await;
        rx.recv().await
    }

    /// 建连 → 认证 → 取能力 → SELECT → 进入 IDLE。
    async fn establish(&mut self) -> MailResult<()> {
        let stream = self.transport.open().await?;
        let (rd, wr) = tokio::io::split(stream);
        let writer = Arc::new(Mutex::new(wr));
        let lines = Arc::new(Mutex::new(spawn_line_pump(BufReader::new(rd))));
        let split = SplitIo {
            lines: Arc::clone(&lines),
            writer: Arc::clone(&writer),
        };

        let mut client = ImapClient::new(Box::new(split));
        client.read_greeting().await?;
        if client.capabilities().is_empty() {
            client.fetch_capabilities().await?;
        }
        ImapProvider::authenticate_client(&mut client, &self.username, &self.password).await?;
        // 认证后能力集会变大（真机登录前 9 项、登录后 52 项），必须重新取，
        // 否则 `IDLE` 会一直被当成不支持。
        client.fetch_capabilities().await?;
        if !client.capabilities().supports_idle() {
            return Err(MailError::Unsupported(
                "服务端未声明 IDLE，无法使用推送（请回退到定时同步）".into(),
            ));
        }
        let state = client.select(&self.cfg.mailbox).await?;

        let idle_tag = self.next_tag();
        client.send_raw_line(&format!("{idle_tag} IDLE")).await?;
        // 必须等到 `+ ` 才算真正进入 idling；等到 tagged 响应说明被拒。
        let entered = loop {
            match Self::recv_line(&lines).await {
                Some(Ok(line)) if line.starts_with('+') => break Ok(()),
                Some(Ok(line)) => {
                    if let Some(t) = response::parse_tagged(&line)? {
                        if t.tag == idle_tag {
                            break Err(MailError::Unsupported(format!(
                                "服务端拒绝 IDLE: {}",
                                t.text
                            )));
                        }
                    }
                    // IDLE 之前可能还有 SELECT 留下的 untagged 尾巴，忽略
                }
                Some(Err(e)) => break Err(e),
                None => break Err(MailError::Network("等待 IDLE 确认时连接断开".into())),
            }
        };
        entered?;

        let deadline = tokio::time::Instant::now() + self.cfg.idle_window;
        self.conn = Some(IdleConn {
            writer,
            lines,
            idle_tag,
            deadline,
            uid_next: state.uid_next,
        });
        self.reconnect_attempt = 0;
        Ok(())
    }

    /// 主动 `DONE` 并重入 IDLE。
    ///
    /// 期间收到的变更不丢弃，先攒进 `pending`：`DONE` 之后服务端会把积压的 untagged
    /// 一口气吐出来，这些恰恰是最新的一批变化，丢了会让上层等下一次触发。
    async fn cycle_idle(&mut self) -> MailResult<()> {
        let (writer, lines, old_tag) = match self.conn.as_ref() {
            Some(c) => (
                Arc::clone(&c.writer),
                Arc::clone(&c.lines),
                c.idle_tag.clone(),
            ),
            None => return Ok(()),
        };

        {
            let mut w = writer.lock().await;
            w.write_all(b"DONE\r\n")
                .await
                .map_err(|e| MailError::Network(format!("发送 DONE 失败: {e}")))?;
            let _ = w.flush().await;
        }

        // 读到本轮 IDLE 的完成响应为止；中途的变更行照收。
        let mut collected: Vec<RemoteSignal> = Vec::new();
        let mut finished = false;
        while !finished {
            match Self::recv_line(&lines).await {
                Some(Ok(line)) => {
                    if let Some(t) = response::parse_tagged(&line)? {
                        if t.tag == old_tag {
                            if !t.is_ok() {
                                // DONE 被拒 = 连接状态已不可信，放弃这条连接走重连
                                self.conn = None;
                                return Ok(());
                            }
                            finished = true;
                        }
                        continue;
                    }
                    if let Some(sig) = self.classify(&line)? {
                        collected.push(sig);
                    }
                }
                Some(Err(e)) => {
                    self.conn = None;
                    if e.is_retryable() {
                        return Ok(());
                    }
                    return Err(e);
                }
                None => {
                    self.conn = None;
                    return Ok(());
                }
            }
        }

        // 重新进入 IDLE
        let tag = self.next_tag();
        {
            let mut w = writer.lock().await;
            let payload = format!("{tag} IDLE\r\n");
            w.write_all(payload.as_bytes())
                .await
                .map_err(|e| MailError::Network(format!("发送 IDLE 失败: {e}")))?;
            let _ = w.flush().await;
        }
        let mut entered = false;
        while !entered {
            match Self::recv_line(&lines).await {
                Some(Ok(line)) => {
                    if line.starts_with('+') {
                        entered = true;
                    } else if let Some(sig) = self.classify(&line)? {
                        collected.push(sig);
                    }
                }
                Some(Err(e)) => {
                    self.conn = None;
                    if e.is_retryable() {
                        return Ok(());
                    }
                    return Err(e);
                }
                None => {
                    self.conn = None;
                    return Ok(());
                }
            }
        }

        self.pending.extend(collected);
        if let Some(conn) = self.conn.as_mut() {
            conn.idle_tag = tag;
            conn.deadline = tokio::time::Instant::now() + self.cfg.idle_window;
        }
        Ok(())
    }

    /// 等一行 / 等窗口到期 / 等取消。取消粒度由 `cancel_poll` 决定。
    async fn wake(&mut self) -> MailResult<Wake> {
        let (lines, deadline, poll, cancel) = match self.conn.as_ref() {
            Some(c) => (
                Arc::clone(&c.lines),
                c.deadline,
                self.cfg.cancel_poll,
                self.cancel.clone(),
            ),
            None => return Ok(Wake::WindowExpired),
        };

        loop {
            if cancel.is_cancelled() {
                return Ok(Wake::Cancelled);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(Wake::WindowExpired);
            }
            let slice = (deadline - now).min(poll);
            let line = tokio::select! {
                biased;
                // 取消只影响 recv()：已进 channel 的行不会丢（泵任务自己拥有读缓冲）。
                msg = Self::recv_line(&lines) => Some(msg),
                _ = tokio::time::sleep(slice) => None,
            };
            match line {
                None => continue,
                Some(None) => {
                    self.conn = None;
                    return Err(MailError::Network("连接已被服务端关闭".into()));
                }
                Some(Some(Err(e))) => {
                    self.conn = None;
                    return Err(e);
                }
                Some(Some(Ok(text))) => return Ok(Wake::Line(text)),
            }
        }
    }

    /// 把一行 untagged 响应翻译成信号（`None` = 与数据无关，忽略）。
    fn classify(&mut self, line: &str) -> MailResult<Option<RemoteSignal>> {
        if !line.starts_with('*') {
            return Ok(None);
        }
        let account = self.account_id.clone();
        match response::parse_untagged(line)? {
            // Stalwart 在 SELECTED 状态也会推 STATUS（含 UIDNEXT），这是最准确的 state。
            //
            // 真机会推**其他**文件夹的 STATUS（实测在 Drafts 上 IDLE 也会收到 INBOX 的
            // STATUS）。多报一条同类型同步无害且幂等，漏报才会真丢数据，所以不过滤。
            Untagged::Status { items, .. } => {
                // UID 空间是 32 位；STATUS 里的数值按 u64 解析，超出范围就不记。
                let uid_next = items
                    .get("UIDNEXT")
                    .copied()
                    .and_then(|u| u32::try_from(u).ok());
                if let (Some(conn), Some(u)) = (self.conn.as_mut(), uid_next) {
                    conn.uid_next = Some(u);
                }
                Ok(Some(RemoteSignal::changed(
                    account,
                    DATA_TYPE_EMAIL,
                    uid_next.map(|u| u.to_string()),
                )))
            }
            // 新邮件：`* 48 EXISTS`（不带 UIDNEXT，沿用已知值，仅作诊断）
            Untagged::Exists(_) => {
                let state = self
                    .conn
                    .as_ref()
                    .and_then(|c| c.uid_next)
                    .map(|u| u.to_string());
                Ok(Some(RemoteSignal::changed(account, DATA_TYPE_EMAIL, state)))
            }
            // 删除：`* 48 EXPUNGE`
            Untagged::Expunge(_) => Ok(Some(RemoteSignal::changed(account, DATA_TYPE_EMAIL, None))),
            // 标志位变化：`* 48 FETCH (FLAGS ... UID 54)`
            Untagged::Fetch(_) => Ok(Some(RemoteSignal::changed(account, DATA_TYPE_EMAIL, None))),
            // `* OK Still here` 之类的保活响应是心跳，不是数据（约束 1）。
            Untagged::Ok(..) => Ok(None),
            Untagged::Recent(_) | Untagged::Capability(_) | Untagged::List(_) => Ok(None),
            Untagged::Bye(text) => Err(MailError::Network(format!("服务端主动断开: {text}"))),
            Untagged::No(_) | Untagged::Other(_) => Ok(None),
        }
    }

    async fn reconnect(&mut self) -> MailResult<()> {
        loop {
            self.cancel.check()?;
            match self.establish().await {
                Ok(()) => return Ok(()),
                Err(err) => {
                    // 认证错 / 能力缺失属于配置问题，重试无意义，直接抛出。
                    if !err.is_retryable() {
                        return Err(err);
                    }
                    let wait = self.cfg.backoff(self.reconnect_attempt);
                    self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
                    // 退避也要能被取消打断，否则关应用时要等满 30 秒。
                    let deadline = tokio::time::Instant::now() + wait;
                    while tokio::time::Instant::now() < deadline {
                        self.cancel.check()?;
                        let left = deadline - tokio::time::Instant::now();
                        tokio::time::sleep(left.min(self.cfg.cancel_poll)).await;
                    }
                }
            }
        }
    }
}

#[async_trait]
impl RemoteWatcher for ImapWatcher {
    async fn next_signal(&mut self) -> MailResult<Option<RemoteSignal>> {
        loop {
            if let Some(sig) = self.pending.pop_front() {
                return Ok(Some(sig));
            }
            self.cancel.check()?;

            if self.conn.is_none() {
                self.reconnect().await?;
                // 断线期间的变化不会补推（约束 3），重连成功必须让上层重新对齐。
                return Ok(Some(RemoteSignal::Resync));
            }

            match self.wake().await {
                Ok(Wake::Cancelled) => return Err(MailError::Cancelled),
                Ok(Wake::WindowExpired) => self.cycle_idle().await?,
                Ok(Wake::Line(line)) => {
                    if let Some(sig) = self.classify(&line)? {
                        return Ok(Some(sig));
                    }
                }
                Err(err) => {
                    self.conn = None;
                    if !err.is_retryable() {
                        return Err(err);
                    }
                    // 可重试错误不在这里睡：交给 reconnect 的退避逻辑统一处理。
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let cfg = IdleConfig {
            mailbox: "INBOX".into(),
            idle_window: Duration::from_secs(1500),
            reconnect_base: Duration::from_millis(200),
            reconnect_max: Duration::from_secs(30),
            cancel_poll: Duration::from_millis(100),
        };
        assert_eq!(cfg.backoff(0), Duration::from_millis(200));
        assert_eq!(cfg.backoff(1), Duration::from_millis(400));
        assert_eq!(cfg.backoff(3), Duration::from_millis(1600));
        assert_eq!(cfg.backoff(20), Duration::from_secs(30), "必须封顶");
    }

    #[test]
    fn default_idle_window_under_rfc_limit() {
        // RFC 2177 建议小于 29 分钟，取 25 分钟留出余量
        let cfg = IdleConfig::for_mailbox("INBOX");
        assert!(cfg.idle_window < Duration::from_secs(29 * 60));
    }
}
