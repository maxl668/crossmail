//! HTTP 传输抽象（J-002 / J-011）。
//!
//! 抽离通道是为了让协议逻辑完全可离线单测：生产用 `ReqwestTransport`，
//! 测试用 `testing::MockTransport`，两者对上层是同一个 trait。

use async_trait::async_trait;
use crossmail_core::error::{MailError, MailResult};
use serde_json::Value;
use std::time::Duration;

pub use crate::ws::{WsFrame, WsStream};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestKind {
    Session,
    Api,
    Upload,
    Download,
    Token,
    /// EventSource 长连接（RFC 8620 §7.3）。
    Events,
    /// WebSocket 推送长连接（RFC 8887）。
    WebSocket,
}

/// SSE 长连接句柄。
///
/// 返回分块字节流而不是解析后的事件：切帧（`\r\n`/`\n` 边界、多行 `data`）属于
/// 协议语义，统一放在 `push` 模块里做，才能脱离网络单测。
pub struct EventStream {
    rx: tokio::sync::mpsc::Receiver<MailResult<Vec<u8>>>,
}

impl EventStream {
    pub fn new(rx: tokio::sync::mpsc::Receiver<MailResult<Vec<u8>>>) -> Self {
        Self { rx }
    }

    /// 取下一个字节块；`None` 表示连接已结束（正常 EOF 或错误下一条之后）。
    pub async fn next_chunk(&mut self) -> Option<MailResult<Vec<u8>>> {
        self.rx.recv().await
    }
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EventStream(..)")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCall {
    pub kind: RequestKind,
    pub url: String,
    pub auth_header: Option<String>,
    pub body: Option<Value>,
    pub content_type: Option<String>,
    pub payload_len: usize,
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn get_json(&self, url: &str, auth: Option<&str>) -> MailResult<Value>;
    async fn post_json(&self, url: &str, auth: Option<&str>, body: &Value) -> MailResult<Value>;
    async fn post_form(&self, url: &str, form: &[(String, String)]) -> MailResult<Value>;
    async fn upload_bytes(
        &self,
        url: &str,
        auth: Option<&str>,
        content_type: &str,
        data: &[u8],
    ) -> MailResult<Value>;
    async fn download_bytes(&self, url: &str, auth: Option<&str>) -> MailResult<Vec<u8>>;

    /// 打开 EventSource 长连接（RFC 8620 §7.3）。
    ///
    /// `last_event_id` 对应 SSE 的 `Last-Event-ID`（断线续传）。注意真机 Stalwart
    /// 从不发 `id:` 字段，所以断线后事件必然有缺口，调用方不能依赖它补齐。
    async fn open_event_stream(
        &self,
        url: &str,
        auth: Option<&str>,
        last_event_id: Option<&str>,
    ) -> MailResult<EventStream>;

    /// 打开 WebSocket 推送长连接（RFC 8887）。
    ///
    /// 默认不支持：能力协商在 session 层做，只有真具备 WS 通道的 transport
    /// 才覆写它。这样旧测试 Mock 不必感知 WS 的存在，watcher 的「WS 优先、
    /// SSE 兜底」在 Mock 环境里自然落到 SSE 路径。
    async fn connect_ws(&self, _url: &str, _auth: Option<&str>) -> MailResult<WsStream> {
        Err(MailError::Unsupported(
            "当前传输通道不支持 WebSocket 推送".into(),
        ))
    }
}

/// 生产通道：reqwest + rustls（无 OpenSSL，交叉编译友好）。
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new(timeout: Duration) -> MailResult<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| MailError::Network(e.to_string()))?;
        Ok(Self { client })
    }
}

impl Default for ReqwestTransport {
    fn default() -> Self {
        Self::new(Duration::from_secs(30)).unwrap_or_else(|_| Self {
            client: reqwest::Client::new(),
        })
    }
}

fn request_builder(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    auth: Option<&str>,
) -> reqwest::RequestBuilder {
    let mut rb = client.request(method, url);
    if let Some(value) = auth {
        rb = rb.header("Authorization", value);
    }
    rb
}

async fn read_json(resp: reqwest::Response) -> MailResult<Value> {
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| MailError::Network(e.to_string()))?;
    if !(200..300).contains(&status) {
        return Err(MailError::from_http(status, &text));
    }
    serde_json::from_str(&text).map_err(|e| MailError::Parse(e.to_string()))
}

#[async_trait]
impl Transport for ReqwestTransport {
    async fn get_json(&self, url: &str, auth: Option<&str>) -> MailResult<Value> {
        let resp = request_builder(&self.client, reqwest::Method::GET, url, auth)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        read_json(resp).await
    }

    async fn post_json(&self, url: &str, auth: Option<&str>, body: &Value) -> MailResult<Value> {
        let resp = request_builder(&self.client, reqwest::Method::POST, url, auth)
            .json(body)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        read_json(resp).await
    }

    async fn post_form(&self, url: &str, form: &[(String, String)]) -> MailResult<Value> {
        let resp = self
            .client
            .post(url)
            .form(form)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        read_json(resp).await
    }

    async fn upload_bytes(
        &self,
        url: &str,
        auth: Option<&str>,
        content_type: &str,
        data: &[u8],
    ) -> MailResult<Value> {
        let resp = request_builder(&self.client, reqwest::Method::POST, url, auth)
            .header("Content-Type", content_type)
            .body(data.to_vec())
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        read_json(resp).await
    }

    async fn download_bytes(&self, url: &str, auth: Option<&str>) -> MailResult<Vec<u8>> {
        let resp = request_builder(&self.client, reqwest::Method::GET, url, auth)
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        if !(200..300).contains(&status) {
            return Err(MailError::from_http(
                status,
                &String::from_utf8_lossy(&bytes),
            ));
        }
        Ok(bytes.to_vec())
    }

    async fn open_event_stream(
        &self,
        url: &str,
        auth: Option<&str>,
        last_event_id: Option<&str>,
    ) -> MailResult<EventStream> {
        // 长连接不能沿用默认 30s 总超时——那会在第 30 秒把正常连接掐掉。
        // 只保留连接超时，读超时交给上层按心跳间隔判断。
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| MailError::Network(e.to_string()))?;
        let mut rb = client.get(url).header("Accept", "text/event-stream");
        if let Some(value) = auth {
            rb = rb.header("Authorization", value);
        }
        if let Some(id) = last_event_id {
            rb = rb.header("Last-Event-ID", id);
        }
        let resp = rb
            .send()
            .await
            .map_err(|e| MailError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            return Err(MailError::from_http(status, &text));
        }
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // 内容类型不对说明不是 SSE（典型是被网关 HTML 错误页接管），早知道早报错。
        if !content_type.starts_with("text/event-stream") {
            return Err(MailError::Protocol(format!(
                "EventSource 响应内容类型错误: {content_type}"
            )));
        }

        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let mut resp = resp;
        tokio::spawn(async move {
            loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        if tx.send(Ok(chunk.to_vec())).await.is_err() {
                            break; // 消费者已丢弃，无需继续读
                        }
                    }
                    Ok(None) => break, // 服务端正常关闭（如 closeafter=state）
                    Err(e) => {
                        let _ = tx.send(Err(MailError::Network(e.to_string()))).await;
                        break;
                    }
                }
            }
        });
        Ok(EventStream::new(rx))
    }

    async fn connect_ws(&self, url: &str, auth: Option<&str>) -> MailResult<WsStream> {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::Message;

        // 握手请求必须经 IntoClientRequest 生成——它会补齐 Host / Upgrade /
        // Connection / Sec-WebSocket-Key / Version。手搓 http::Request 会缺
        // Sec-WebSocket-Key，被 tungstenite 直接拒绝（真机实测踩到）。
        // 认证与子协议（RFC 8887 §4.1 要求 `jmap`）在规范请求上附加。
        let mut request = url
            .into_client_request()
            .map_err(|e| MailError::InvalidInput(format!("WS 地址非法: {e}")))?;
        {
            let headers = request.headers_mut();
            headers.insert(
                "Sec-WebSocket-Protocol",
                tokio_tungstenite::tungstenite::http::HeaderValue::from_static("jmap"),
            );
            if let Some(value) = auth {
                let value = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(value)
                    .map_err(|_| MailError::InvalidInput("Authorization 头值非法".into()))?;
                headers.insert("Authorization", value);
            }
        }

        let handshake =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, None);
        let (ws, _resp) = match tokio::time::timeout(Duration::from_secs(15), handshake).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                return Err(MailError::Network(format!("WS 握手失败: {e}")));
            }
            Err(_) => return Err(MailError::Network("WS 握手超时（15s）".into())),
        };

        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<crate::ws::WsOut>(16);
        let (in_tx, in_rx) = tokio::sync::mpsc::channel::<MailResult<WsFrame>>(64);
        let (mut write, mut read) = ws.split();

        // 泵任务：出站注册帧落库到 sink；入站文本帧/控制帧转译为 WsFrame；
        // 每 25s 发一个协议层 Ping 维持活性（RFC 8887 不定义服务端心跳，
        // 空闲链路只能靠客户端自己保活，Pong 也计为活动）。
        // Ping 的对端应答 Pong 由 tungstenite 在读路径上自动回，无需手写。
        tokio::spawn(async move {
            let mut keepalive = tokio::time::interval(Duration::from_secs(25));
            keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    out = out_rx.recv() => {
                        let msg = match out {
                            Some(crate::ws::WsOut::Text(t)) => Message::Text(t.into()),
                            Some(crate::ws::WsOut::Ping) => Message::Ping(Vec::new().into()),
                            // 会话侧句柄已全部丢弃：礼貌关闭，不再续命。
                            None => {
                                let _ = write.send(Message::Close(None)).await;
                                break;
                            }
                        };
                        if write.send(msg).await.is_err() {
                            break;
                        }
                    }
                    frame = read.next() => {
                        let outcome = match frame {
                            Some(Ok(Message::Text(t))) => Ok(WsFrame::Text(t.as_str().to_string())),
                            // 协议层活性：刷新会话的空闲计时，语义同 SSE 心跳。
                            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => Ok(WsFrame::Activity),
                            Some(Ok(Message::Binary(_))) => {
                                Err(MailError::Protocol("WS 推送收到二进制帧（RFC 8887 只用文本）".into()))
                            }
                            Some(Ok(Message::Frame(_))) => continue,
                            Some(Ok(Message::Close(_))) | None => Ok(WsFrame::Closed),
                            Some(Err(e)) => Err(MailError::Network(format!("WS 读失败: {e}"))),
                        };
                        let is_end = matches!(outcome, Ok(WsFrame::Closed)) || outcome.is_err();
                        if in_tx.send(outcome).await.is_err() || is_end {
                            break;
                        }
                    }
                    _ = keepalive.tick() => {
                        if write.send(Message::Ping(Vec::new().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(WsStream::new(out_tx, in_rx))
    }
}

/// 借用通道：让测试可以在持有 `&MockTransport` 的同时构造客户端并检查调用记录。
#[async_trait]
impl<T: Transport> Transport for &T {
    async fn get_json(&self, url: &str, auth: Option<&str>) -> MailResult<Value> {
        (**self).get_json(url, auth).await
    }
    async fn post_json(&self, url: &str, auth: Option<&str>, body: &Value) -> MailResult<Value> {
        (**self).post_json(url, auth, body).await
    }
    async fn post_form(&self, url: &str, form: &[(String, String)]) -> MailResult<Value> {
        (**self).post_form(url, form).await
    }
    async fn upload_bytes(
        &self,
        url: &str,
        auth: Option<&str>,
        content_type: &str,
        data: &[u8],
    ) -> MailResult<Value> {
        (**self).upload_bytes(url, auth, content_type, data).await
    }
    async fn download_bytes(&self, url: &str, auth: Option<&str>) -> MailResult<Vec<u8>> {
        (**self).download_bytes(url, auth).await
    }
    async fn open_event_stream(
        &self,
        url: &str,
        auth: Option<&str>,
        last_event_id: Option<&str>,
    ) -> MailResult<EventStream> {
        (**self).open_event_stream(url, auth, last_event_id).await
    }

    async fn connect_ws(&self, url: &str, auth: Option<&str>) -> MailResult<WsStream> {
        (**self).connect_ws(url, auth).await
    }
}

/// 共享通道：Provider 适配层需要 `'static` 通道，同时测试仍要检查调用记录。
#[async_trait]
impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    async fn get_json(&self, url: &str, auth: Option<&str>) -> MailResult<Value> {
        (**self).get_json(url, auth).await
    }
    async fn post_json(&self, url: &str, auth: Option<&str>, body: &Value) -> MailResult<Value> {
        (**self).post_json(url, auth, body).await
    }
    async fn post_form(&self, url: &str, form: &[(String, String)]) -> MailResult<Value> {
        (**self).post_form(url, form).await
    }
    async fn upload_bytes(
        &self,
        url: &str,
        auth: Option<&str>,
        content_type: &str,
        data: &[u8],
    ) -> MailResult<Value> {
        (**self).upload_bytes(url, auth, content_type, data).await
    }
    async fn download_bytes(&self, url: &str, auth: Option<&str>) -> MailResult<Vec<u8>> {
        (**self).download_bytes(url, auth).await
    }
    async fn open_event_stream(
        &self,
        url: &str,
        auth: Option<&str>,
        last_event_id: Option<&str>,
    ) -> MailResult<EventStream> {
        (**self).open_event_stream(url, auth, last_event_id).await
    }

    async fn connect_ws(&self, url: &str, auth: Option<&str>) -> MailResult<WsStream> {
        (**self).connect_ws(url, auth).await
    }
}

/// 离线测试通道（公开导出，供 crate 内外的集成测试复用）。
pub mod testing {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Debug, Clone, PartialEq)]
    pub enum MockResponse {
        Json(Value),
        Bytes(Vec<u8>),
        HttpError {
            status: u16,
            message: String,
        },
        /// SSE 长连接：按顺序吐出若干字节块。
        /// `hold_open = true` 时**不关闭**连接（用于验证空闲超时/心跳缺失）。
        Stream {
            chunks: Vec<Vec<u8>>,
            hold_open: bool,
        },
        /// WebSocket 长连接（RFC 8887）：按顺序吐出若干帧。
        /// `hold_open = true` 时不关闭（验证空闲超时 / 活性帧语义 / 断线重连）。
        Ws {
            frames: Vec<crate::ws::WsFrame>,
            hold_open: bool,
        },
    }

    #[derive(Debug, Default)]
    pub struct MockTransport {
        responses: Mutex<VecDeque<MockResponse>>,
        calls: Mutex<Vec<RecordedCall>>,
        /// `hold_open` 的流把发送端留在这里，防止 channel 关闭。
        open_streams: Mutex<Vec<tokio::sync::mpsc::Sender<MailResult<Vec<u8>>>>>,
        /// WS 会话发出的出站文本帧（注册/注销），测试可逐条断言。
        ws_sent: std::sync::Arc<Mutex<Vec<String>>>,
        /// `hold_open` 的 WS 连接把入站端留在这里，防止会话句柄过早 EOF。
        ws_streams: Mutex<Vec<tokio::sync::mpsc::Sender<MailResult<WsFrame>>>>,
    }

    impl MockTransport {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn push(&self, response: MockResponse) {
            self.responses
                .lock()
                .expect("mock responses lock")
                .push_back(response);
        }

        pub fn push_json(&self, value: Value) {
            self.push(MockResponse::Json(value));
        }

        pub fn push_bytes(&self, bytes: Vec<u8>) {
            self.push(MockResponse::Bytes(bytes));
        }

        /// 推一条 SSE 长连接；`chunks` 按顺序下发，发完即关闭（模拟 closeafter=state）。
        pub fn push_stream(&self, chunks: Vec<Vec<u8>>) {
            self.push(MockResponse::Stream {
                chunks,
                hold_open: false,
            });
        }

        /// 推一条**保持打开**的 SSE 长连接（心跳缺失 / 空闲超时场景）。
        pub fn push_stream_open(&self, chunks: Vec<Vec<u8>>) {
            self.push(MockResponse::Stream {
                chunks,
                hold_open: true,
            });
        }

        /// 推一条 WS 连接：`frames` 按顺序下发，发完即关闭。
        pub fn push_ws_frames(&self, frames: Vec<crate::ws::WsFrame>) {
            self.push(MockResponse::Ws {
                frames,
                hold_open: false,
            });
        }

        /// 推一条**保持打开**的 WS 连接（空闲超时 / 断线重连场景）。
        pub fn push_ws_frames_open(&self, frames: Vec<crate::ws::WsFrame>) {
            self.push(MockResponse::Ws {
                frames,
                hold_open: true,
            });
        }

        /// WS 会话已发出的出站文本帧（`WebSocketPushEnable`/`Disable`），供断言。
        pub fn ws_sent(&self) -> Vec<String> {
            self.ws_sent.lock().expect("mock ws_sent lock").clone()
        }

        pub fn push_http_error(&self, status: u16, message: &str) {
            self.push(MockResponse::HttpError {
                status,
                message: message.to_string(),
            });
        }

        pub fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().expect("mock calls lock").clone()
        }

        pub fn last_call(&self) -> Option<RecordedCall> {
            self.calls().pop()
        }

        fn record(&self, call: RecordedCall) {
            self.calls.lock().expect("mock calls lock").push(call);
        }

        fn take(&self) -> MailResult<MockResponse> {
            self.responses
                .lock()
                .expect("mock responses lock")
                .pop_front()
                .ok_or_else(|| MailError::Protocol("MockTransport 未配置响应".into()))
        }

        fn take_json(&self) -> MailResult<Value> {
            match self.take()? {
                MockResponse::Json(v) => Ok(v),
                MockResponse::HttpError { status, message } => {
                    Err(MailError::from_http(status, &message))
                }
                MockResponse::Bytes(_) => Err(MailError::Protocol("期望 JSON 响应".into())),
                MockResponse::Stream { .. } => {
                    Err(MailError::Protocol("期望 JSON 响应，收到 SSE 流".into()))
                }
                MockResponse::Ws { .. } => {
                    Err(MailError::Protocol("期望 JSON 响应，收到 WS 连接".into()))
                }
            }
        }

        fn take_bytes(&self) -> MailResult<Vec<u8>> {
            match self.take()? {
                MockResponse::Bytes(b) => Ok(b),
                MockResponse::HttpError { status, message } => {
                    Err(MailError::from_http(status, &message))
                }
                MockResponse::Json(_) => Err(MailError::Protocol("期望二进制响应".into())),
                MockResponse::Stream { .. } => {
                    Err(MailError::Protocol("期望二进制响应，收到 SSE 流".into()))
                }
                MockResponse::Ws { .. } => {
                    Err(MailError::Protocol("期望二进制响应，收到 WS 连接".into()))
                }
            }
        }

        fn take_stream(&self) -> MailResult<EventStream> {
            let (tx, rx) = tokio::sync::mpsc::channel(256);
            match self.take()? {
                MockResponse::Stream { chunks, hold_open } => {
                    for chunk in chunks {
                        tx.try_send(Ok(chunk)).map_err(|_| {
                            MailError::Protocol("MockTransport 流式响应块过多".into())
                        })?;
                    }
                    if hold_open {
                        self.open_streams
                            .lock()
                            .expect("mock streams lock")
                            .push(tx);
                    }
                    Ok(EventStream::new(rx))
                }
                MockResponse::HttpError { status, message } => {
                    Err(MailError::from_http(status, &message))
                }
                _ => Err(MailError::Protocol("期望 SSE 流式响应".into())),
            }
        }

        fn take_ws(&self) -> MailResult<WsStream> {
            // 队列头不是 WS 连接时不消费任何响应——真实语义是「服务端没接受
            // WS 握手」，watcher 要原样把 SSE 响应留给降级路径用。
            let front_is_ws = {
                let guard = self.responses.lock().expect("mock responses lock");
                matches!(guard.front(), Some(MockResponse::Ws { .. }))
            };
            if !front_is_ws {
                return Err(MailError::Unsupported(
                    "Mock 队列里没有 WS 连接响应（模拟服务端未接受 WS 握手）".into(),
                ));
            }
            let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<crate::ws::WsOut>(16);
            let (in_tx, in_rx) = tokio::sync::mpsc::channel::<MailResult<WsFrame>>(64);
            match self.take()? {
                MockResponse::Ws { frames, hold_open } => {
                    for frame in frames {
                        in_tx
                            .try_send(Ok(frame))
                            .map_err(|_| MailError::Protocol("MockTransport WS 帧过多".into()))?;
                    }
                    if hold_open {
                        self.ws_streams
                            .lock()
                            .expect("mock ws_streams lock")
                            .push(in_tx);
                    }
                    // 回显出站文本帧供断言；连接随 mock 响应耗尽自然结束。
                    let sent = std::sync::Arc::clone(&self.ws_sent);
                    tokio::spawn(async move {
                        while let Some(out) = out_rx.recv().await {
                            if let crate::ws::WsOut::Text(text) = out {
                                if let Ok(mut guard) = sent.lock() {
                                    guard.push(text);
                                }
                            }
                        }
                    });
                    Ok(WsStream::new(out_tx, in_rx))
                }
                MockResponse::HttpError { status, message } => {
                    Err(MailError::from_http(status, &message))
                }
                _ => Err(MailError::Protocol("期望 WS 连接响应".into())),
            }
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn get_json(&self, url: &str, auth: Option<&str>) -> MailResult<Value> {
            self.record(RecordedCall {
                kind: RequestKind::Session,
                url: url.to_string(),
                auth_header: auth.map(str::to_string),
                body: None,
                content_type: None,
                payload_len: 0,
            });
            self.take_json()
        }

        async fn post_json(
            &self,
            url: &str,
            auth: Option<&str>,
            body: &Value,
        ) -> MailResult<Value> {
            self.record(RecordedCall {
                kind: RequestKind::Api,
                url: url.to_string(),
                auth_header: auth.map(str::to_string),
                body: Some(body.clone()),
                content_type: None,
                payload_len: 0,
            });
            self.take_json()
        }

        async fn post_form(&self, url: &str, form: &[(String, String)]) -> MailResult<Value> {
            self.record(RecordedCall {
                kind: RequestKind::Token,
                url: url.to_string(),
                auth_header: None,
                body: Some(Value::Object(
                    form.iter()
                        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                        .collect(),
                )),
                content_type: None,
                payload_len: 0,
            });
            self.take_json()
        }

        async fn upload_bytes(
            &self,
            url: &str,
            auth: Option<&str>,
            content_type: &str,
            data: &[u8],
        ) -> MailResult<Value> {
            self.record(RecordedCall {
                kind: RequestKind::Upload,
                url: url.to_string(),
                auth_header: auth.map(str::to_string),
                body: None,
                content_type: Some(content_type.to_string()),
                payload_len: data.len(),
            });
            self.take_json()
        }

        async fn download_bytes(&self, url: &str, auth: Option<&str>) -> MailResult<Vec<u8>> {
            self.record(RecordedCall {
                kind: RequestKind::Download,
                url: url.to_string(),
                auth_header: auth.map(str::to_string),
                body: None,
                content_type: None,
                payload_len: 0,
            });
            self.take_bytes()
        }

        async fn open_event_stream(
            &self,
            url: &str,
            auth: Option<&str>,
            last_event_id: Option<&str>,
        ) -> MailResult<EventStream> {
            self.record(RecordedCall {
                kind: RequestKind::Events,
                url: url.to_string(),
                auth_header: auth.map(str::to_string),
                body: last_event_id.map(|id| serde_json::json!({ "lastEventId": id })),
                content_type: None,
                payload_len: 0,
            });
            self.take_stream()
        }

        async fn connect_ws(&self, url: &str, auth: Option<&str>) -> MailResult<WsStream> {
            self.record(RecordedCall {
                kind: RequestKind::WebSocket,
                url: url.to_string(),
                auth_header: auth.map(str::to_string),
                body: None,
                content_type: None,
                payload_len: 0,
            });
            self.take_ws()
        }
    }
}
