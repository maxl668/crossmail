//! JMAP EventSource 推送（RFC 8620 §7 / J-012）。
//!
//! 协议形态：`GET eventSourceUrl` 拿一条 `text/event-stream` 长连接，服务端在
//! 账号状态变化时下发
//!
//! ```text
//! event: state
//! data: {"@type":"StateChange","changed":{"b":{"Email":"s3mbq"}}}
//!
//! ```
//!
//! 真机（Stalwart）实测的三条硬事实，直接决定本模块的设计：
//!
//! 1. **`id:` 从不下发** —— 所以 SSE 的 `Last-Event-ID` 续传形同虚设。断线期间
//!    的变化必然有缺口，客户端只能「重连后重新对齐 state」，不能假装没漏。
//! 2. **`event: ping` 约 30 秒一次，且不理会 `ping` 参数**（传 1 传 5 都一样）。
//!    因此空闲超时必须显著大于 30s，否则正常连接会被误判为死链。
//! 3. **`closeafter=state` 会在首个 state 后主动断开** —— 可以拿来做「等下一次变化」
//!    的一次性订阅，此时 EOF 是**正常结束**，不是错误。

use crate::transport::EventStream;
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::Id;
use crossmail_core::provider::CancellationToken;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

/// `event:` 字段取值：状态变化。
pub const STATE_EVENT: &str = "state";
/// `event:` 字段取值：心跳。
pub const PING_EVENT: &str = "ping";

/// 服务端心跳间隔的实测值（真机约 30s，且不随 `ping` 参数变化）。
pub const SERVER_PING_INTERVAL: Duration = Duration::from_secs(30);

/// 空闲超时默认值：留出两个心跳周期的余量，避免把慢心跳误判成死链。
pub const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

// ---------------------------------------------------------------------------
// SSE 切帧
// ---------------------------------------------------------------------------

/// 一条已切分好的 SSE 事件。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub id: Option<String>,
    /// 多行 `data:` 已按规范用 `\n` 连接。
    pub data: String,
    /// 服务端要求重连间隔（毫秒）。
    pub retry: Option<u64>,
}

/// 增量 SSE 解码器。
///
/// TCP 分块边界与事件边界无关：一个事件可能被拆在两个 chunk 里，一个 chunk 也可能
/// 含多个事件。用缓冲 + 行扫描处理，**不假设 chunk 边界**。
#[derive(Debug, Default)]
pub struct SseDecoder {
    buf: Vec<u8>,
    event: Option<String>,
    id: Option<String>,
    data: String,
    has_data: bool,
    retry: Option<u64>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一段字节，返回本次能完整切出的事件（按到达顺序）。
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((line, consumed)) = next_line(&self.buf) {
            self.buf.drain(..consumed);
            if let Some(ev) = self.on_line(&line) {
                out.push(ev);
            }
        }
        out
    }

    fn on_line(&mut self, line: &str) -> Option<SseEvent> {
        if line.is_empty() {
            return self.dispatch();
        }
        // 注释行（`:` 开头）用于保活，按规范忽略。
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_string()),
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "id" => self.id = Some(value.to_string()),
            "retry" => {
                if let Ok(ms) = value.parse::<u64>() {
                    self.retry = Some(ms);
                }
            }
            _ => {}
        }
        None
    }

    fn dispatch(&mut self) -> Option<SseEvent> {
        // 规范说 data 为空则不分发；但 `event: ping` 只有事件名没有 data，
        // 而心跳是判断连接死活的唯一依据，所以「有事件名」也要放行。
        if !self.has_data && self.event.is_none() {
            self.id = None;
            self.retry = None;
            return None;
        }
        let ev = SseEvent {
            event: self.event.take(),
            id: self.id.take(),
            data: std::mem::take(&mut self.data),
            retry: self.retry.take(),
        };
        self.has_data = false;
        Some(ev)
    }
}

/// 取出一整行（不含行尾）；行尾可以是 `\r\n`、`\n` 或单独的 `\r`。
///
/// 返回 `(行内容, 已消费字节数)`；缓冲区里还没有完整行时返回 `None`。
fn next_line(buf: &[u8]) -> Option<(String, usize)> {
    let mut i = 0;
    while i < buf.len() {
        match buf[i] {
            b'\n' => {
                let line = String::from_utf8_lossy(&buf[..i]).into_owned();
                return Some((line, i + 1));
            }
            b'\r' => {
                if i + 1 == buf.len() {
                    // 可能是 `\r\n` 被切开了，等下一个 chunk 再判断
                    return None;
                }
                let end = if buf[i + 1] == b'\n' { i + 2 } else { i + 1 };
                let line = String::from_utf8_lossy(&buf[..i]).into_owned();
                return Some((line, end));
            }
            _ => i += 1,
        }
    }
    None
}

// ---------------------------------------------------------------------------
// StateChange
// ---------------------------------------------------------------------------

/// RFC 8620 §7.1 `StateChange`：`changed[账号][数据类型] = 新 state`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateChange {
    pub changed: BTreeMap<Id, BTreeMap<String, Id>>,
}

impl StateChange {
    pub fn is_empty(&self) -> bool {
        self.changed.values().all(|m| m.is_empty())
    }

    /// 某个账号下某类数据的当前 state。
    pub fn state_of(&self, account: &str, data_type: &str) -> Option<&Id> {
        self.changed.get(account).and_then(|m| m.get(data_type))
    }

    /// 用后来的变化覆盖更早的（state 只增不减，后者为准）。
    pub fn merge(&mut self, other: &StateChange) {
        for (account, types) in &other.changed {
            let entry = self.changed.entry(account.clone()).or_default();
            for (ty, state) in types {
                entry.insert(ty.clone(), state.clone());
            }
        }
    }
}

/// 解析 `StateChange` 负载。
///
/// 宽容点：非字符串 state 值直接跳过（服务端扩展可能塞对象），但 `@type` 明确
/// 不是 `StateChange` 时报错——静默接受会让上层拿到永远对不上的 state。
pub fn parse_state_change(value: &Value) -> MailResult<StateChange> {
    if let Some(ty) = value.get("@type").and_then(Value::as_str) {
        if ty != "StateChange" {
            return Err(MailError::Protocol(format!(
                "推送类型不是 StateChange: {ty}"
            )));
        }
    }
    let changed = value
        .get("changed")
        .and_then(Value::as_object)
        .ok_or_else(|| MailError::Protocol("StateChange 缺少 changed 字段".into()))?;
    let mut out: BTreeMap<Id, BTreeMap<String, Id>> = BTreeMap::new();
    for (account, types) in changed {
        let Some(types) = types.as_object() else {
            continue;
        };
        let mut map = BTreeMap::new();
        for (ty, state) in types {
            if let Some(s) = state.as_str() {
                map.insert(ty.clone(), s.to_string());
            }
        }
        out.insert(account.clone(), map);
    }
    Ok(StateChange { changed: out })
}

// ---------------------------------------------------------------------------
// 推送流
// ---------------------------------------------------------------------------

/// `closeafter` 取值（RFC 8620 §7.3）：不关闭 / 首个 state 后关闭。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CloseAfter {
    #[default]
    No,
    State,
}

impl CloseAfter {
    pub fn as_str(self) -> &'static str {
        match self {
            CloseAfter::No => "no",
            CloseAfter::State => "state",
        }
    }
}

/// 订阅参数。
#[derive(Debug, Clone)]
pub struct PushConfig {
    /// 要订阅的数据类型；空集合按通配 `*` 处理（真机上留空会 400）。
    pub types: Vec<String>,
    pub close_after: CloseAfter,
    /// 期望心跳间隔（秒）。真机不理会该参数，恒约 30s。
    pub ping: Option<u32>,
    /// 多久收不到任何字节就判定连接已死。
    pub idle_timeout: Duration,
    /// 重连退避起点。
    pub reconnect_base: Duration,
    /// 重连退避上限。
    pub reconnect_max: Duration,
}

impl Default for PushConfig {
    fn default() -> Self {
        Self {
            types: vec!["*".to_string()],
            close_after: CloseAfter::No,
            ping: Some(SERVER_PING_INTERVAL.as_secs() as u32),
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            reconnect_base: Duration::from_millis(200),
            reconnect_max: Duration::from_secs(30),
        }
    }
}

impl PushConfig {
    /// 只订阅指定类型。
    pub fn for_types(types: &[&str]) -> Self {
        Self {
            types: types.iter().map(|t| (*t).to_string()).collect(),
            ..Self::default()
        }
    }

    /// 一次性订阅：收到首个 state 即结束（`closeafter=state`）。
    pub fn single_shot(types: &[&str]) -> Self {
        Self {
            close_after: CloseAfter::State,
            ..Self::for_types(types)
        }
    }
}

/// 推送流上的一次更新。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushUpdate {
    /// 状态变化：`changed[账号][类型]` 与本地已知 state 不同即为需要增量同步。
    State(StateChange),
    /// 心跳，仅用于证明连接还活着。
    Ping,
    /// 订阅结束：`closeafter=state` 收到首个 state 后服务端主动关闭，或对端断流。
    EndOfStream,
    /// **断线重连成功后补发的对齐信号**：断线期间的变化不会补推（真机没有 `id:`），
    /// 收到它必须重新拉一次 state 补齐，否则会以为「没变化」而漏掉整段更新。
    Resync,
}

/// 单条连接的读取器：负责切帧、识别心跳、把 JSON 解析成 `StateChange`。
#[derive(Debug)]
pub struct PushReader {
    stream: EventStream,
    decoder: SseDecoder,
    idle_timeout: Duration,
    pending: VecDeque<PushUpdate>,
}

impl PushReader {
    pub fn new(stream: EventStream, idle_timeout: Duration) -> Self {
        Self {
            stream,
            decoder: SseDecoder::new(),
            idle_timeout,
            pending: VecDeque::new(),
        }
    }

    /// 取下一条更新；`Ok(None)` 表示连接已结束（正常关闭或断流）。
    pub async fn next_update(&mut self) -> MailResult<Option<PushUpdate>> {
        loop {
            if let Some(update) = self.pending.pop_front() {
                return Ok(Some(update));
            }
            let chunk =
                match tokio::time::timeout(self.idle_timeout, self.stream.next_chunk()).await {
                    Ok(Some(chunk)) => chunk?,
                    Ok(None) => return Ok(None),
                    Err(_) => {
                        return Err(MailError::Network(format!(
                            "推送空闲超时（{}s 无任何数据，服务端心跳约 {}s）",
                            self.idle_timeout.as_secs(),
                            SERVER_PING_INTERVAL.as_secs()
                        )))
                    }
                };
            for event in self.decoder.feed(&chunk) {
                if let Some(update) = self.classify(event)? {
                    self.pending.push_back(update);
                }
            }
        }
    }

    fn classify(&self, event: SseEvent) -> MailResult<Option<PushUpdate>> {
        match event.event.as_deref() {
            Some(PING_EVENT) => Ok(Some(PushUpdate::Ping)),
            Some(STATE_EVENT) => Ok(Some(PushUpdate::State(parse_state_change(
                &serde_json::from_str(&event.data)
                    .map_err(|e| MailError::Parse(format!("推送负载不是 JSON: {e}")))?,
            )?))),
            // 没有 event 名但负载是 StateChange：部分实现省掉 event 字段，宽容接受。
            None if event.data.contains("StateChange") => {
                let value = serde_json::from_str(&event.data)
                    .map_err(|e| MailError::Parse(format!("推送负载不是 JSON: {e}")))?;
                Ok(Some(PushUpdate::State(parse_state_change(&value)?)))
            }
            // 其他事件名（服务端私有扩展）忽略，不让它打断主循环。
            _ => Ok(None),
        }
    }
}

/// 带重连的订阅会话。
///
/// 由调用方驱动（每次 `next()` 传入 client），因此不需要 `'static`、不用把连接
/// 塞进后台任务，也能在离线测试里用 Mock 通道完整跑通重连路径。
///
/// 三条规则：
/// - **断线必然有缺口**：重连成功后的第一条更新是 `Resync`，上层必须重新对齐 state。
/// - **`closeafter=state` 的 EOF 是正常结束**，不重连，直接给 `EndOfStream`。
/// - 退避从 200ms 起指数翻倍、封顶 30s；等待期间响应取消。
#[derive(Debug)]
pub struct PushSession {
    cfg: PushConfig,
    cancel: CancellationToken,
    reader: Option<PushReader>,
    pending: VecDeque<PushUpdate>,
    backoff: Duration,
    need_resync: bool,
    ended: bool,
}

impl PushSession {
    pub fn new(cfg: PushConfig, cancel: CancellationToken) -> Self {
        let backoff = cfg.reconnect_base;
        Self {
            cfg,
            cancel,
            reader: None,
            pending: VecDeque::new(),
            backoff,
            need_resync: false,
            ended: false,
        }
    }

    pub fn config(&self) -> &PushConfig {
        &self.cfg
    }

    /// 当前退避时长（排障用）。
    pub fn backoff(&self) -> Duration {
        self.backoff
    }

    /// 是否处于「连接中断、等待重连」状态。
    pub fn is_disconnected(&self) -> bool {
        self.reader.is_none() && !self.ended
    }

    /// 立即建立订阅连接（幂等）。
    ///
    /// **顺序很重要：先 `connect`，再拉初始快照。** 反过来的话，快照之后、连接
    /// 建立之前发生的改动不会被推过来，也不会有任何提示——静默丢事件。
    /// 有了这个入口，「已订阅」才是可观测状态，而不是「我调了 watch 就算订阅了」。
    pub async fn connect<T: crate::transport::Transport>(
        &mut self,
        client: &crate::client::JmapClient<T>,
    ) -> MailResult<()> {
        self.ensure_connected(client).await
    }

    /// 取下一个推送；内部按需建连、断线重连。
    pub async fn next<T: crate::transport::Transport>(
        &mut self,
        client: &crate::client::JmapClient<T>,
    ) -> MailResult<PushUpdate> {
        loop {
            self.cancel.check()?;
            if let Some(update) = self.pending.pop_front() {
                return Ok(update);
            }
            if self.ended {
                return Ok(PushUpdate::EndOfStream);
            }

            self.ensure_connected(client).await?;
            if self.need_resync {
                // 断线（或建连失败）期间的事件不会补推，先让上层重新对齐。
                // 标志只在「发出 Resync」时清除；`connect()` 阶段产生的空档
                // 也留到这里交出去，不能被吞。
                self.need_resync = false;
                return Ok(PushUpdate::Resync);
            }

            let result = match self.reader.as_mut() {
                Some(reader) => reader.next_update().await,
                None => continue,
            };
            match result {
                Ok(Some(update)) => return Ok(update),
                Ok(None) => {
                    self.reader = None;
                    if self.cfg.close_after == CloseAfter::State {
                        self.ended = true;
                        return Ok(PushUpdate::EndOfStream);
                    }
                    self.need_resync = true;
                }
                Err(e) if is_retryable(&e) => {
                    self.reader = None;
                    self.need_resync = true;
                    let wait = self.backoff;
                    self.grow_backoff();
                    self.sleep_or_cancel(wait).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// 建连（含可重试失败的重试）；失败时置 `need_resync`，因为空档已经产生。
    async fn ensure_connected<T: crate::transport::Transport>(
        &mut self,
        client: &crate::client::JmapClient<T>,
    ) -> MailResult<()> {
        if self.reader.is_some() {
            return Ok(());
        }
        loop {
            self.cancel.check()?;
            match client.open_push(&self.cfg).await {
                Ok(reader) => {
                    self.reader = Some(reader);
                    self.backoff = self.cfg.reconnect_base;
                    return Ok(());
                }
                Err(e) if is_retryable(&e) => {
                    // 从这次失败到下一次连上之间的变化一个都收不到，必须标空档
                    self.need_resync = true;
                    let wait = self.backoff;
                    self.grow_backoff();
                    self.sleep_or_cancel(wait).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn grow_backoff(&mut self) {
        self.backoff = (self.backoff * 2).min(self.cfg.reconnect_max);
    }

    /// 分片睡眠，保证取消能及时生效（长退避时不会卡住退出）。
    async fn sleep_or_cancel(&self, total: Duration) -> MailResult<()> {
        let step = Duration::from_millis(25);
        let mut left = total;
        while left > Duration::ZERO {
            self.cancel.check()?;
            let slice = left.min(step);
            tokio::time::sleep(slice).await;
            left -= slice;
        }
        Ok(())
    }
}

/// 哪些错误值得重连：网络抖动、限流、5xx 值得；认证失败、能力不支持重连也没用。
/// SSE 与 WebSocket 推送会话共用同一套判定。
pub(crate) fn is_retryable(error: &MailError) -> bool {
    match error {
        MailError::Network(_) | MailError::RateLimited(_) => true,
        // 5xx 是服务端临时故障，重连有意义；4xx（含认证失败）重连也没用。
        MailError::Server { status, .. } => *status >= 500,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(chunk: &str) -> Vec<SseEvent> {
        SseDecoder::new().feed(chunk.as_bytes())
    }

    #[test]
    fn dispatches_state_event_from_real_server_payload() {
        // 真机原样负载
        let events = one("event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"b\":{\"Email\":\"s3mbq\"}}}\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("state"));
        let change = parse_state_change(&serde_json::from_str(&events[0].data).unwrap()).unwrap();
        assert_eq!(change.state_of("b", "Email"), Some(&"s3mbq".to_string()));
    }

    #[test]
    fn survives_chunk_boundaries_inside_a_line() {
        let full = "event: state\ndata: {\"changed\":{\"b\":{\"Email\":\"s1\"}}}\n\n";
        let mut decoder = SseDecoder::new();
        let mut events = Vec::new();
        // 逐字节喂，模拟最恶劣的切分
        for byte in full.as_bytes() {
            events.extend(decoder.feed(&[*byte]));
        }
        assert_eq!(events.len(), 1, "逐字节切分后仍应还原出 1 个事件");
        assert_eq!(events[0].data, "{\"changed\":{\"b\":{\"Email\":\"s1\"}}}");
    }

    #[test]
    fn multiple_events_in_one_chunk() {
        let events = one("event: ping\n\nevent: state\ndata: {\"changed\":{}}\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[1].event.as_deref(), Some("state"));
    }

    #[test]
    fn crlf_line_endings_and_comments() {
        let events = one(": keep-alive\r\nevent: state\r\ndata: {\"changed\":{}}\r\n\r\n");
        assert_eq!(events.len(), 1, "注释行应被忽略");
        assert_eq!(events[0].event.as_deref(), Some("state"));
    }

    #[test]
    fn multi_line_data_is_joined_with_newline() {
        let events = one("data: a\ndata: b\n\n");
        assert_eq!(events[0].data, "a\nb");
    }

    #[test]
    fn retry_field_is_captured() {
        let events = one("retry: 3000\nevent: state\ndata: {}\n\n");
        assert_eq!(events[0].retry, Some(3000));
    }

    #[test]
    fn ping_without_data_still_dispatches() {
        // 真机心跳就是 `event: ping` + 空行；若按浏览器规范丢弃，
        // 客户端会以为连接已死，在 30s 后误判超时。
        let events = one("event: ping\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("ping"));
        assert_eq!(events[0].data, "");
    }

    #[test]
    fn rejects_non_state_change_payload() {
        let err = parse_state_change(&serde_json::json!({"@type": "Something"})).unwrap_err();
        assert_eq!(err.kind(), crossmail_core::error::ErrorKind::Protocol);
    }

    #[test]
    fn skips_non_string_state_values() {
        let change = parse_state_change(&serde_json::json!({
            "changed": { "b": { "Email": "s1", "Weird": {"x": 1} } }
        }))
        .unwrap();
        assert_eq!(change.state_of("b", "Email"), Some(&"s1".to_string()));
        assert_eq!(change.state_of("b", "Weird"), None);
    }

    #[test]
    fn merge_keeps_latest_state() {
        let mut a = parse_state_change(&serde_json::json!({
            "changed": { "b": { "Email": "s1", "Mailbox": "m1" } }
        }))
        .unwrap();
        let b = parse_state_change(&serde_json::json!({
            "changed": { "b": { "Email": "s2" } }
        }))
        .unwrap();
        a.merge(&b);
        assert_eq!(a.state_of("b", "Email"), Some(&"s2".to_string()));
        assert_eq!(a.state_of("b", "Mailbox"), Some(&"m1".to_string()));
    }
}
