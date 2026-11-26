//! JMAP WebSocket 推送（RFC 8887，J-010 补齐）。
//!
//! 协议形态：`wss://…/jmap/ws` 长连接（session 能力
//! `urn:ietf:params:jmap:websocket`）。与 EventSource 的关键差异：
//!
//! 1. **推送不是默认开启的**——客户端连上后必须先发一条
//!    `WebSocketPushEnable` 注册帧（`dataTypes: null` 表示全部类型），服务端
//!    之后才会把 `StateChange` 作为文本帧推过来。
//! 2. **RFC 没有定义服务端心跳**。连接死活只能靠空闲超时判断：生产通道
//!    （`transport` 的 WS 泵）周期性发协议层 Ping 维持活性，Pong 也算活动。
//!    测试通道用脚本化帧序列模拟。
//! 3. **`StateChange` 可带 `pushState`**：断线重连时把它回传给服务端，服务端
//!    **宜**补发缺口期间的变化。但「宜」不是「必须」，所以与 SSE 相同的
//!    保守策略仍然成立：重连成功后先给 [`PushUpdate::Resync`]，缺口交给上层
//!    重新对齐；`pushState` 只是能补则补的加速器，不是正确性依赖。
//! 4. **请求错误是扩展的 Problem Details**（`@type: "RequestError"`），带
//!    `status`。注册被拒要立即报错而不是静默重试。

use crate::push::{is_retryable, parse_state_change, CloseAfter, PushConfig, PushUpdate};
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::provider::CancellationToken;
use serde_json::Value;
use std::collections::VecDeque;
use std::time::Duration;
use tokio::sync::mpsc;

/// 会话与传输层之间的一次 WS 帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsFrame {
    /// 文本帧（JSON）。
    Text(String),
    /// 协议层活性（Ping/Pong 控制帧），只用于刷新空闲计时。
    Activity,
    /// 对端已关闭（Close 帧 / TCP 断开）。
    Closed,
}

/// 会话 → 传输层的出站消息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WsOut {
    /// 文本帧：`WebSocketPushEnable` / `WebSocketPushDisable`。
    Text(String),
    /// 协议层 Ping（保活）。
    Ping,
}

/// 一条已建立的 WS 双向通道（生产由 transport 层泵，测试由脚本驱动）。
#[derive(Debug)]
pub struct WsStream {
    tx: mpsc::Sender<WsOut>,
    rx: mpsc::Receiver<MailResult<WsFrame>>,
}

impl WsStream {
    pub fn new(tx: mpsc::Sender<WsOut>, rx: mpsc::Receiver<MailResult<WsFrame>>) -> Self {
        Self { tx, rx }
    }

    /// 发一条文本帧；对端已断时返回 `None`（等价于连接已结束）。
    pub async fn send_text(&self, text: &str) -> Option<()> {
        self.tx.send(WsOut::Text(text.to_string())).await.ok()
    }

    /// 取下一帧；`Ok(None)` 表示连接已结束。
    pub async fn next_frame(&mut self) -> MailResult<Option<WsFrame>> {
        match self.rx.recv().await {
            Some(Ok(frame)) => Ok(Some(frame)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}

// ---------------------------------------------------------------------------
// 注册帧构造
// ---------------------------------------------------------------------------

/// `WebSocketPushEnable`（RFC 8887 §4.3.5.2）。
///
/// `dataTypes: null` 表示订阅全部类型——`PushConfig::default()` 的通配 `*`
/// 是 SSE 语法，WS 侧语义等价物就是 null，必须做这层翻译而不是把 `*`
/// 原样发出去。
pub fn push_enable_frame(types: &[String], push_state: Option<&str>) -> String {
    let mut body = serde_json::json!({ "@type": "WebSocketPushEnable" });
    let data_types: Vec<&str> = types
        .iter()
        .map(|t| t.as_str())
        .filter(|t| !t.is_empty() && *t != "*")
        .collect();
    if data_types.is_empty() {
        body["dataTypes"] = Value::Null;
    } else {
        body["dataTypes"] = serde_json::json!(data_types);
    }
    if let Some(state) = push_state {
        body["pushState"] = Value::String(state.to_string());
    }
    body.to_string()
}

/// `WebSocketPushDisable`（RFC 8887 §4.3.5.3）：一次性订阅收尾时显式注销。
pub fn push_disable_frame() -> String {
    serde_json::json!({ "@type": "WebSocketPushDisable" }).to_string()
}

// ---------------------------------------------------------------------------
// 帧分类
// ---------------------------------------------------------------------------

/// 把一条文本帧解析成推送更新。
///
/// - `StateChange` → 记录 `pushState`（有则回传给重连用）并给 `State`；
/// - `RequestError` → 立即报错（注册被拒 / 认证过期），不让会话静默空转；
/// - 其他 `@type`（服务端扩展 / 心跳对象）忽略——未知帧打断主循环比宽容
///   接受更糟，这与 SSE 分类器对未知 `event:` 名的处理一致。
pub fn classify_ws_text(
    text: &str,
    last_push_state: &mut Option<String>,
) -> MailResult<Option<PushUpdate>> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| MailError::Parse(format!("WS 推送帧不是 JSON: {e}")))?;
    match value.get("@type").and_then(Value::as_str) {
        Some("StateChange") => {
            if let Some(state) = value.get("pushState").and_then(Value::as_str) {
                *last_push_state = Some(state.to_string());
            }
            Ok(Some(PushUpdate::State(parse_state_change(&value)?)))
        }
        Some("RequestError") => {
            let status = value.get("status").and_then(Value::as_u64).unwrap_or(400) as u16;
            let detail = value
                .get("detail")
                .and_then(Value::as_str)
                .unwrap_or("服务端未说明");
            Err(MailError::Server {
                status,
                message: format!("WebSocket 推送被拒: {detail}"),
            })
        }
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// 带（重连的）WS 推送会话
// ---------------------------------------------------------------------------

/// WS 版 [`crate::push::PushSession`]：语义完全对齐——
/// 断线必然有缺口（重连后先 `Resync`）、一次性订阅的关闭是正常结束、
/// 退避从 `reconnect_base` 起指数翻倍封顶 `reconnect_max`、等待期响应取消。
///
/// 由调用方驱动（每次 `next()` 传入连接工厂），不要求 `'static`，
/// 离线测试可以用 Mock 通道完整跑通重连路径。
#[derive(Debug)]
pub struct WsPushSession {
    cfg: PushConfig,
    cancel: CancellationToken,
    stream: Option<WsStream>,
    pending: VecDeque<PushUpdate>,
    backoff: Duration,
    need_resync: bool,
    ended: bool,
    last_push_state: Option<String>,
}

impl WsPushSession {
    pub fn new(cfg: PushConfig, cancel: CancellationToken) -> Self {
        let backoff = cfg.reconnect_base;
        Self {
            cfg,
            cancel,
            stream: None,
            pending: VecDeque::new(),
            backoff,
            need_resync: false,
            ended: false,
            last_push_state: None,
        }
    }

    pub fn config(&self) -> &PushConfig {
        &self.cfg
    }

    pub fn backoff(&self) -> Duration {
        self.backoff
    }

    pub fn is_disconnected(&self) -> bool {
        self.stream.is_none() && !self.ended
    }

    /// 立即建立 WS 连接并发送注册帧（幂等）。
    ///
    /// 与 SSE 相同的顺序契约：先建连注册，上层再拉初始快照。
    pub async fn connect<F, Fut>(&mut self, open: F) -> MailResult<()>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = MailResult<WsStream>>,
    {
        self.ensure_connected(&open).await
    }

    /// 取下一个推送；内部按需建连、断线重连。
    pub async fn next<F, Fut>(&mut self, open: F) -> MailResult<PushUpdate>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = MailResult<WsStream>>,
    {
        loop {
            self.cancel.check()?;
            if let Some(update) = self.pending.pop_front() {
                return Ok(update);
            }
            if self.ended {
                return Ok(PushUpdate::EndOfStream);
            }

            self.ensure_connected(&open).await?;
            if self.need_resync {
                // 断线/建连失败期间的事件不保证补齐（pushState 只是宜补），
                // 先让上层重新对齐，缺口宁可多算不能漏。
                self.need_resync = false;
                return Ok(PushUpdate::Resync);
            }

            let result = match self.stream.as_mut() {
                Some(stream) => {
                    match tokio::time::timeout(self.cfg.idle_timeout, stream.next_frame()).await {
                        Ok(frame) => frame,
                        Err(_) => Err(MailError::Network(format!(
                            "WS 推送空闲超时（{}s 无任何帧）",
                            self.cfg.idle_timeout.as_secs()
                        ))),
                    }
                }
                None => continue,
            };
            match result {
                Ok(Some(WsFrame::Text(text))) => {
                    let update = classify_ws_text(&text, &mut self.last_push_state)?;
                    if let Some(update) = update {
                        // 一次性订阅：收到首个 state 即收尾，显式注销再结束。
                        if self.cfg.close_after == CloseAfter::State {
                            self.push_disable().await;
                            self.ended = true;
                        }
                        self.pending.push_back(update);
                    }
                }
                Ok(Some(WsFrame::Activity)) => {
                    self.pending.push_back(PushUpdate::Ping);
                }
                Ok(Some(WsFrame::Closed)) | Ok(None) => {
                    self.stream = None;
                    if self.cfg.close_after == CloseAfter::State {
                        self.ended = true;
                        return Ok(PushUpdate::EndOfStream);
                    }
                    self.need_resync = true;
                }
                Err(e) if is_retryable(&e) => {
                    self.stream = None;
                    self.need_resync = true;
                    let wait = self.backoff;
                    self.grow_backoff();
                    self.sleep_or_cancel(wait).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn ensure_connected<F, Fut>(&mut self, open: &F) -> MailResult<()>
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = MailResult<WsStream>>,
    {
        if self.stream.is_some() {
            return Ok(());
        }
        loop {
            self.cancel.check()?;
            match open().await {
                Ok(stream) => {
                    // 注册帧必须在任何读之前发出，否则连接活着也收不到推送。
                    let enable =
                        push_enable_frame(&self.cfg.types, self.last_push_state.as_deref());
                    match stream.send_text(&enable).await {
                        Some(()) => {
                            self.stream = Some(stream);
                            self.backoff = self.cfg.reconnect_base;
                            return Ok(());
                        }
                        // 注册帧都发不进去，等于没连上：走可重试路径。
                        None => {
                            self.need_resync = true;
                            let wait = self.backoff;
                            self.grow_backoff();
                            self.sleep_or_cancel(wait).await?;
                        }
                    }
                }
                Err(e) if is_retryable(&e) => {
                    self.need_resync = true;
                    let wait = self.backoff;
                    self.grow_backoff();
                    self.sleep_or_cancel(wait).await?;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn push_disable(&mut self) {
        if let Some(stream) = self.stream.as_ref() {
            let _ = stream.send_text(&push_disable_frame()).await;
        }
        self.stream = None;
    }

    fn grow_backoff(&mut self) {
        self.backoff = (self.backoff * 2).min(self.cfg.reconnect_max);
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_frame_translates_wildcard_to_null() {
        // SSE 语法里的 "*" 不是 JMAP 数据类型名；WS 侧的「都要」是 dataTypes: null。
        let frame = push_enable_frame(&["*".to_string()], None);
        let v: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["@type"], "WebSocketPushEnable");
        assert_eq!(v["dataTypes"], Value::Null);
        assert!(v.get("pushState").is_none());
    }

    #[test]
    fn enable_frame_passes_through_concrete_types_and_push_state() {
        let frame = push_enable_frame(&["Email".into(), "Mailbox".into()], Some("bbb"));
        let v: Value = serde_json::from_str(&frame).unwrap();
        assert_eq!(v["dataTypes"], serde_json::json!(["Email", "Mailbox"]));
        assert_eq!(v["pushState"], "bbb");
    }

    #[test]
    fn disable_frame_shape() {
        let v: Value = serde_json::from_str(&push_disable_frame()).unwrap();
        assert_eq!(v["@type"], "WebSocketPushDisable");
    }

    #[test]
    fn classify_records_push_state_and_parses_change() {
        let mut last = None;
        let update = classify_ws_text(
            r#"{"@type":"StateChange","changed":{"a456":{"Mailbox":"d35ecb040aab"}},"pushState":"bbb"}"#,
            &mut last,
        )
        .unwrap()
        .unwrap();
        assert_eq!(last.as_deref(), Some("bbb"));
        match update {
            PushUpdate::State(change) => {
                assert_eq!(
                    change.state_of("a456", "Mailbox"),
                    Some(&"d35ecb040aab".to_string())
                )
            }
            other => panic!("期望 State，实际 {other:?}"),
        }
    }

    #[test]
    fn classify_unknown_types_are_ignored() {
        let mut last = None;
        let update = classify_ws_text(
            r#"{"@type":"SomethingElse","detail":"自定义扩展"}"#,
            &mut last,
        )
        .unwrap();
        assert!(update.is_none(), "未知帧不应打断主循环: {update:?}");
    }

    #[test]
    fn classify_request_error_is_an_immediate_failure() {
        let mut last = None;
        let err = classify_ws_text(
            r#"{"@type":"RequestError","requestId":null,"type":"urn:ietf:params:jmap:error:notJSON","status":400,"detail":"The request did not parse as I-JSON."}"#,
            &mut last,
        )
        .unwrap_err();
        assert_eq!(err.kind(), crossmail_core::error::ErrorKind::Server);
        assert!(err.to_string().contains("推送被拒"), "{err}");
    }

    #[test]
    fn classify_non_json_text_fails_loudly() {
        let mut last = None;
        let err = classify_ws_text("not-json", &mut last).unwrap_err();
        assert_eq!(err.kind(), crossmail_core::error::ErrorKind::Parse);
    }
}
