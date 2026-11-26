//! S6-12：JMAP EventSource 推送离线测试（Mock 通道，无网络依赖）。
//!
//! 这些用例锁死的是**真机行为**，不是协议文档的理想形态：
//! - `eventSourceUrl` 是模板，原样请求 400；
//! - 心跳是 `event: ping` 且只有事件名没有 data；
//! - `closeafter=state` 的 EOF 是正常结束；
//! - 服务端从不发 `id:`，断线后必须 `Resync` 重新对齐。

use crossmail_core::error::ErrorKind;
use crossmail_core::provider::CancellationToken;
use crossmail_core::watch::{RemoteSignal, RemoteWatcher, DATA_TYPE_EMAIL, DATA_TYPE_MAILBOX};
use crossmail_jmap::auth::AuthMethod;
use crossmail_jmap::client::JmapClient;
use crossmail_jmap::provider::JmapProvider;
use crossmail_jmap::push::{CloseAfter, PushConfig, PushSession, PushUpdate, StateChange};
use crossmail_jmap::session::Session;
use crossmail_jmap::transport::testing::MockTransport;
use crossmail_jmap::transport::{RequestKind, Transport};
use crossmail_jmap::watcher::JmapWatcher;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

/// 真机的 eventSourceUrl 就是这种带三个占位符的模板。
const REAL_TEMPLATE: &str =
    "https://host:10000/jmap/eventsource/?types={types}&closeafter={closeafter}&ping={ping}";

fn session_value(event_source_url: Option<&str>) -> Value {
    let mut value = json!({
        "username": "admin@example.com",
        "apiUrl": "https://host:10000/jmap/",
        "downloadUrl": "https://host:10000/jmap/download/{accountId}/{blobId}/{name}?type={type}",
        "uploadUrl": "https://host:10000/jmap/upload/{accountId}/",
        "state": "sess-1",
        "capabilities": {
            "urn:ietf:params:jmap:core": {},
            "urn:ietf:params:jmap:mail": {},
            "urn:ietf:params:jmap:websocket": {
                "url": "wss://host:10000/jmap/ws",
                "supportsPush": true
            }
        },
        "accounts": {
            "b": {
                "name": "admin@example.com", "isPersonal": true, "isReadOnly": false,
                "accountCapabilities": { "urn:ietf:params:jmap:mail": {} }
            }
        },
        "primaryAccounts": { "urn:ietf:params:jmap:mail": "b" }
    });
    match event_source_url {
        Some(url) => {
            value["eventSourceUrl"] = json!(url);
        }
        None => {
            value.as_object_mut().unwrap().remove("eventSourceUrl");
        }
    }
    value
}

fn session(url: Option<&str>) -> Session {
    Session::from_value(&session_value(url)).expect("session 合法")
}

fn client(mock: &MockTransport) -> JmapClient<&MockTransport> {
    JmapClient::with_session(
        mock,
        session(Some(REAL_TEMPLATE)),
        AuthMethod::bearer("tok"),
    )
}

fn state_chunk(state: &str) -> Vec<u8> {
    format!("event: state\ndata: {{\"@type\":\"StateChange\",\"changed\":{{\"b\":{{\"Email\":\"{state}\"}}}}}}\n\n")
        .into_bytes()
}

fn ping_chunk() -> Vec<u8> {
    b"event: ping\n\n".to_vec()
}

// ---------------------------------------------------------------------------
// URL 构造
// ---------------------------------------------------------------------------

/// 真机模板必须被完整替换：残留任何一个 `{...}` 都会被服务端 400。
#[test]
fn endpoint_replaces_all_template_placeholders() {
    let s = session(Some(REAL_TEMPLATE));
    let url = s
        .event_source_endpoint(&["Email".into()], "no", Some(30))
        .unwrap();
    assert_eq!(
        url,
        "https://host:10000/jmap/eventsource/?types=Email&closeafter=no&ping=30"
    );
    assert!(!url.contains('{'), "不能留下未替换的占位符: {url}");
}

/// 空类型集在真机上是 400，语义上的「都要」是通配 `*`。
#[test]
fn endpoint_uses_wildcard_for_empty_types() {
    let s = session(Some(REAL_TEMPLATE));
    let url = s.event_source_endpoint(&[], "state", None).unwrap();
    assert!(url.contains("types=*"), "{url}");
    assert!(url.contains("closeafter=state"), "{url}");
    assert!(!url.contains("{ping}"), "不关心心跳也要清掉占位符: {url}");
}

/// 非模板 URL（RFC 允许的另一种形态）按查询串追加参数。
#[test]
fn endpoint_appends_query_for_plain_url() {
    let s = session(Some("https://host/events/"));
    let url = s
        .event_source_endpoint(&["Email".into(), "Mailbox".into()], "no", Some(5))
        .unwrap();
    assert_eq!(
        url,
        "https://host/events/?types=Email,Mailbox&closeafter=no&ping=5"
    );
}

#[test]
fn endpoint_rejects_unknown_placeholder_and_bad_closeafter() {
    let s = session(Some("https://host/events/?types={types}&foo={foo}"));
    let err = s.event_source_endpoint(&[], "no", None).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Protocol);
    assert!(err.to_string().contains("未替换"), "{err}");

    let s = session(Some(REAL_TEMPLATE));
    let err = s.event_source_endpoint(&[], "forever", None).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn endpoint_requires_event_source_url() {
    let s = session(None);
    let err = s.event_source_endpoint(&[], "no", None).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    // 少了 eventSourceUrl 时能力位必须如实为 false
    assert!(!s.provider_capabilities().push);
}

/// 真机同时声明了 WebSocket 推送（RFC 8887）：地址可探测，watcher 按可用性选通道。
#[test]
fn websocket_url_is_exposed_for_probing() {
    let s = session(Some(REAL_TEMPLATE));
    assert_eq!(
        s.websocket_url().as_deref(),
        Some("wss://host:10000/jmap/ws")
    );
    assert!(s.provider_capabilities().push);
}

// ---------------------------------------------------------------------------
// 单条连接读取
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_push_requests_the_resolved_template_url() {
    let mock = MockTransport::new();
    mock.push_stream(vec![state_chunk("s1")]);
    let c = client(&mock);

    let mut reader = c
        .open_push(&PushConfig::for_types(&["Email"]))
        .await
        .unwrap();
    let update = reader.next_update().await.unwrap();
    assert!(matches!(update, Some(PushUpdate::State(_))));

    let call = mock.last_call().expect("应有建连请求");
    assert_eq!(call.kind, RequestKind::Events);
    assert!(call.url.contains("types=Email"), "实际 URL: {}", call.url);
    assert!(!call.url.contains('{'), "URL 未替换: {}", call.url);
}

/// `closeafter=state` 下服务端关闭连接 = **正常结束**，不是错误。
#[tokio::test]
async fn reader_returns_none_after_server_closes() {
    let mock = MockTransport::new();
    mock.push_stream(vec![state_chunk("s2")]);
    let c = client(&mock);
    let mut reader = c
        .open_push(&PushConfig::single_shot(&["Email"]))
        .await
        .unwrap();

    match reader.next_update().await.unwrap() {
        Some(PushUpdate::State(change)) => {
            assert_eq!(change.state_of("b", "Email"), Some(&"s2".to_string()));
        }
        other => panic!("期望 State，实际 {other:?}"),
    }
    assert_eq!(reader.next_update().await.unwrap(), None, "EOF 应返回 None");
}

#[tokio::test]
async fn reader_reports_ping_as_liveness() {
    let mock = MockTransport::new();
    mock.push_stream(vec![ping_chunk()]);
    let c = client(&mock);
    let mut reader = c
        .open_push(&PushConfig::for_types(&["Email"]))
        .await
        .unwrap();
    assert_eq!(reader.next_update().await.unwrap(), Some(PushUpdate::Ping));
}

/// 连接一声不吭（没有心跳也没有事件）：必须超时报错，不能永远挂着。
#[tokio::test]
async fn reader_times_out_on_silent_connection() {
    let mock = MockTransport::new();
    mock.push_stream_open(vec![]);
    let c = client(&mock);
    let mut cfg = PushConfig::for_types(&["Email"]);
    cfg.idle_timeout = Duration::from_millis(60);
    let mut reader = c.open_push(&cfg).await.unwrap();

    let err = reader.next_update().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Network);
    assert!(err.to_string().contains("心跳"), "{err}");
}

/// 建连失败要原样暴露（认证错、URL 错不是「没变化」）。
#[tokio::test]
async fn open_push_surfaces_http_error() {
    let mock = MockTransport::new();
    mock.push_http_error(401, "unauthorized");
    let c = client(&mock);
    let err = c
        .open_push(&PushConfig::for_types(&["Email"]))
        .await
        .unwrap_err();
    assert!(matches!(
        err.kind(),
        ErrorKind::Authentication | ErrorKind::Server
    ));
}

// ---------------------------------------------------------------------------
// 重连会话
// ---------------------------------------------------------------------------

/// 断线后重连：先给 `Resync`（断线期间的缺口不会补推），再给新的事件。
#[tokio::test]
async fn session_emits_resync_after_disconnect() {
    let mock = MockTransport::new();
    // 第一条连接：一个事件后服务端断开
    mock.push_stream(vec![state_chunk("s1")]);
    // 第二条连接：重连后继续推
    mock.push_stream(vec![state_chunk("s2")]);
    let c = client(&mock);

    let mut cfg = PushConfig::for_types(&["Email"]);
    cfg.reconnect_base = Duration::from_millis(10);
    let mut session = PushSession::new(cfg, CancellationToken::new());

    match session.next(&c).await.unwrap() {
        PushUpdate::State(change) => {
            assert_eq!(change.state_of("b", "Email"), Some(&"s1".to_string()))
        }
        other => panic!("期望首个 State，实际 {other:?}"),
    }
    assert_eq!(
        session.next(&c).await.unwrap(),
        PushUpdate::Resync,
        "断线重连后必须先要求上层重新对齐 state"
    );
    match session.next(&c).await.unwrap() {
        PushUpdate::State(change) => {
            assert_eq!(change.state_of("b", "Email"), Some(&"s2".to_string()))
        }
        other => panic!("期望重连后的 State，实际 {other:?}"),
    }
}

/// `closeafter=state` 的结束不重连：给一次 `EndOfStream` 后就该收手。
#[tokio::test]
async fn session_single_shot_does_not_reconnect() {
    let mock = MockTransport::new();
    mock.push_stream(vec![state_chunk("s9")]);
    let c = client(&mock);
    let mut cfg = PushConfig::single_shot(&["Email"]);
    cfg.reconnect_base = Duration::from_millis(10);
    let mut session = PushSession::new(cfg, CancellationToken::new());

    assert!(matches!(
        session.next(&c).await.unwrap(),
        PushUpdate::State(_)
    ));
    assert_eq!(session.next(&c).await.unwrap(), PushUpdate::EndOfStream);
    assert_eq!(
        session.next(&c).await.unwrap(),
        PushUpdate::EndOfStream,
        "结束后重复取仍应是 EndOfStream，而不是重新建连"
    );
    let events = mock
        .calls()
        .into_iter()
        .filter(|c| c.kind == RequestKind::Events)
        .count();
    assert_eq!(events, 1, "不该重连，实际建连 {events} 次");
}

/// 5xx 值得重连；重连成功后必须补 `Resync`（建连失败也算断过）。
#[tokio::test]
async fn session_retries_server_error_then_resyncs() {
    let mock = MockTransport::new();
    mock.push_http_error(503, "service unavailable");
    mock.push_stream(vec![state_chunk("s5")]);
    let c = client(&mock);
    let mut cfg = PushConfig::for_types(&["Email"]);
    cfg.reconnect_base = Duration::from_millis(10);
    let mut session = PushSession::new(cfg, CancellationToken::new());

    assert_eq!(session.next(&c).await.unwrap(), PushUpdate::Resync);
    assert!(matches!(
        session.next(&c).await.unwrap(),
        PushUpdate::State(_)
    ));
    let events = mock
        .calls()
        .into_iter()
        .filter(|c| c.kind == RequestKind::Events)
        .count();
    assert_eq!(events, 2, "503 后应重试一次，实际 {events} 次");
}

/// 认证失败重连也没用：立刻返回，不做无意义重试。
#[tokio::test]
async fn session_does_not_retry_auth_failure() {
    let mock = MockTransport::new();
    mock.push_http_error(401, "unauthorized");
    let c = client(&mock);
    let mut session = PushSession::new(PushConfig::for_types(&["Email"]), CancellationToken::new());
    let err = session.next(&c).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Authentication);
    assert_eq!(
        mock.calls()
            .iter()
            .filter(|c| c.kind == RequestKind::Events)
            .count(),
        1,
        "认证失败不该重试"
    );
}

#[tokio::test]
async fn session_honors_cancellation() {
    let mock = MockTransport::new();
    let c = client(&mock);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut session = PushSession::new(PushConfig::for_types(&["Email"]), cancel);
    let err = session.next(&c).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Cancelled);
    assert!(mock.calls().is_empty(), "取消后不该建连");
}

/// 退避是指数增长且有上限：断线久拖不能变成每秒一次的风暴。
///
/// 一个 `next()` 内部会把重试循环跑到底，所以「退避是否增长」看的是
/// 连续失败后最终落在哪个值，以及一共尝试了几次。
#[tokio::test]
async fn session_backoff_grows_and_caps() {
    let mock = MockTransport::new();
    for _ in 0..3 {
        mock.push_http_error(500, "boom");
    }
    let c = client(&mock);
    let mut cfg = PushConfig::for_types(&["Email"]);
    cfg.reconnect_base = Duration::from_millis(1);
    cfg.reconnect_max = Duration::from_millis(8);
    let mut session = PushSession::new(cfg, CancellationToken::new());

    // 第 4 次建连没有脚本响应 → Protocol，不是可重试错误，循环退出
    let err = session.next(&c).await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Protocol);
    assert_eq!(
        session.backoff(),
        Duration::from_millis(8),
        "1→2→4→8 应逐级翻倍"
    );
    assert_eq!(
        mock.calls()
            .iter()
            .filter(|c| c.kind == RequestKind::Events)
            .count(),
        4,
        "3 次可重试失败 + 1 次致命失败"
    );
}

/// 上限生效：退避不会无限增长。
#[tokio::test]
async fn session_backoff_respects_max() {
    let mock = MockTransport::new();
    for _ in 0..5 {
        mock.push_http_error(500, "boom");
    }
    let c = client(&mock);
    let mut cfg = PushConfig::for_types(&["Email"]);
    cfg.reconnect_base = Duration::from_millis(1);
    cfg.reconnect_max = Duration::from_millis(2);
    let mut session = PushSession::new(cfg, CancellationToken::new());

    let _ = session.next(&c).await;
    assert_eq!(
        session.backoff(),
        Duration::from_millis(2),
        "超过上限应被压回 max"
    );
}

// ---------------------------------------------------------------------------
// Provider 适配层
// ---------------------------------------------------------------------------

/// 服务端两个推送入口都没有时，`watch` 必须明确报不支持，而不是假装订阅成功。
#[tokio::test]
async fn provider_watch_requires_push_capability() {
    let mock = MockTransport::new();
    // eventSourceUrl 与 WebSocket 能力都摘掉：缺一个还能走另一个，两个全没有才该拒。
    let mut no_push = session_value(None);
    no_push["capabilities"]
        .as_object_mut()
        .unwrap()
        .remove("urn:ietf:params:jmap:websocket");
    let p = JmapProvider::from_client(
        JmapClient::with_session(
            &mock,
            Session::from_value(&no_push).expect("session 合法"),
            AuthMethod::bearer("tok"),
        ),
        "admin@example.com",
    );
    let err = p
        .watch(PushConfig::default(), CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Unsupported);
}

#[tokio::test]
async fn provider_exposes_both_push_endpoints() {
    let mock = MockTransport::new();
    let p = JmapProvider::from_client(
        JmapClient::with_session(
            &mock,
            session(Some(REAL_TEMPLATE)),
            AuthMethod::bearer("tok"),
        ),
        "admin@example.com",
    );
    let (es, ws) = p.push_endpoints().await.unwrap();
    assert_eq!(es.as_deref(), Some(REAL_TEMPLATE));
    assert_eq!(ws.as_deref(), Some("wss://host:10000/jmap/ws"));
}

/// `StateChange` 合并语义：后到的 state 覆盖先到的，其他类型保留。
#[test]
fn state_change_merge_semantics() {
    let mut a: StateChange = crossmail_jmap::push::parse_state_change(&json!({
        "changed": { "b": { "Email": "s1", "Mailbox": "m1" } }
    }))
    .unwrap();
    let b = crossmail_jmap::push::parse_state_change(&json!({
        "changed": { "b": { "Email": "s2" }, "c": { "Email": "x1" } }
    }))
    .unwrap();
    a.merge(&b);
    assert_eq!(a.state_of("b", "Email"), Some(&"s2".to_string()));
    assert_eq!(a.state_of("b", "Mailbox"), Some(&"m1".to_string()));
    assert_eq!(a.state_of("c", "Email"), Some(&"x1".to_string()));
    assert!(!a.is_empty());

    let _ = CloseAfter::State.as_str();
}

/// `connect()` 让「订阅已建立」可观测：建连后就绪，随后的事件不会漏。
///
/// 之前的实现只在第一次 `next()` 里懒建连，上层拿不到「已经连上」这个信号，
/// 于是「先拉快照、改动、再取推送」的顺序会在空档里静默丢事件（真机实测踩到）。
#[tokio::test]
async fn session_connect_is_observable_and_does_not_emit_bogus_resync() {
    let mock = MockTransport::new();
    mock.push_stream(vec![state_chunk("s1")]);
    let c = client(&mock);
    let mut session = PushSession::new(PushConfig::for_types(&["Email"]), CancellationToken::new());

    session.connect(&c).await.expect("建连应成功");
    assert!(
        mock.calls().iter().any(|c| c.kind == RequestKind::Events),
        "connect() 必须真的发出建连请求"
    );
    // 建连成功从未断过 → 不该冒出 Resync
    match session.next(&c).await.unwrap() {
        PushUpdate::State(change) => {
            assert_eq!(change.state_of("b", "Email"), Some(&"s1".to_string()))
        }
        other => panic!("期望 State，实际 {other:?}"),
    }
}

/// 建连失败过再成功：`connect()` 只负责连上，`Resync` 留给 `next()` 交出去。
#[tokio::test]
async fn session_connect_after_failure_flags_resync_for_next() {
    let mock = MockTransport::new();
    mock.push_http_error(503, "boom");
    mock.push_stream(vec![state_chunk("s1")]);
    let c = client(&mock);
    let mut cfg = PushConfig::for_types(&["Email"]);
    cfg.reconnect_base = Duration::from_millis(1);
    let mut session = PushSession::new(cfg, CancellationToken::new());

    session.connect(&c).await.expect("重试后应连上");
    assert_eq!(
        session.next(&c).await.unwrap(),
        PushUpdate::Resync,
        "失败过的建连意味着空档，必须先要求上层重新对齐"
    );
}

// ---------------------------------------------------------------------------
// S6-13：推送流量 → 协议无关信号（RemoteWatcher）
// ---------------------------------------------------------------------------

fn provider(mock: &MockTransport) -> JmapProvider<&MockTransport> {
    JmapProvider::from_client(client(mock), "admin@example.com")
}

fn multi_type_chunk() -> Vec<u8> {
    b"event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{\"b\":{\"Email\":\"s1\",\"Mailbox\":\"m1\"}}}\n\n"
        .to_vec()
}

/// 心跳必须被适配层吞掉：它每 30 秒来一次，冒到同步层就是每 30 秒一次白拉。
#[tokio::test]
async fn watcher_swallows_heartbeat_and_emits_changed() {
    let mock = MockTransport::new();
    mock.push_stream(vec![ping_chunk(), state_chunk("s9")]);
    let p = provider(&mock);

    let mut watcher = JmapWatcher::subscribe(
        &p,
        PushConfig::for_types(&["Email"]),
        CancellationToken::new(),
    )
    .await
    .expect("订阅应成功");

    let signal = watcher.next_signal().await.expect("读信号失败");
    assert_eq!(
        signal,
        Some(RemoteSignal::changed(
            "b",
            DATA_TYPE_EMAIL,
            Some("s9".into())
        )),
        "心跳之后的第一条有效信号必须是 StateChange 翻出来的 Changed"
    );
}

/// 一个 StateChange 同时覆盖多类数据 → 拆成逐类型信号，供上层按类型过滤。
#[tokio::test]
async fn watcher_splits_state_change_by_data_type() {
    let mock = MockTransport::new();
    mock.push_stream(vec![multi_type_chunk()]);
    let p = provider(&mock);

    let mut watcher = JmapWatcher::subscribe(
        &p,
        PushConfig::for_types(&["Email", "Mailbox"]),
        CancellationToken::new(),
    )
    .await
    .expect("订阅应成功");

    let first = watcher.next_signal().await.unwrap().unwrap();
    assert_eq!(first.data_type(), Some(DATA_TYPE_EMAIL));
    let second = watcher.next_signal().await.unwrap().unwrap();
    assert_eq!(second.data_type(), Some(DATA_TYPE_MAILBOX));
    assert_eq!(
        second,
        RemoteSignal::changed("b", DATA_TYPE_MAILBOX, Some("m1".into()))
    );
}

/// 一次性订阅（`closeafter=state`）的 EOF 是正常结束，要翻成 `Closed` 而不是报错。
#[tokio::test]
async fn watcher_maps_end_of_stream_to_closed() {
    let mock = MockTransport::new();
    mock.push_stream(vec![state_chunk("s1")]);
    let p = provider(&mock);

    let mut watcher = JmapWatcher::subscribe(
        &p,
        PushConfig::single_shot(&["Email"]),
        CancellationToken::new(),
    )
    .await
    .expect("订阅应成功");

    assert_eq!(
        watcher.next_signal().await.unwrap().unwrap().data_type(),
        Some(DATA_TYPE_EMAIL)
    );
    assert_eq!(
        watcher.next_signal().await.unwrap(),
        Some(RemoteSignal::Closed),
        "服务端主动断开不是故障"
    );
}

/// `changed` 为空（服务端只说「变了」没说哪类）→ 保守按 Resync 对齐，不能当无变化。
#[tokio::test]
async fn watcher_treats_empty_change_as_resync() {
    let mock = MockTransport::new();
    mock.push_stream(vec![
        b"event: state\ndata: {\"@type\":\"StateChange\",\"changed\":{}}\n\n".to_vec(),
    ]);
    let p = provider(&mock);

    let mut watcher = JmapWatcher::subscribe(
        &p,
        PushConfig::for_types(&["Email"]),
        CancellationToken::new(),
    )
    .await
    .expect("订阅应成功");

    assert_eq!(
        watcher.next_signal().await.unwrap(),
        Some(RemoteSignal::Resync)
    );
}

/// 服务端没声明 `eventSourceUrl` 时必须在订阅阶段就拒绝，而不是拿到一个永远静默的 watcher。
#[tokio::test]
async fn watcher_requires_server_push_capability() {
    let mock = MockTransport::new();
    let c = JmapClient::with_session(&mock, session(None), AuthMethod::bearer("tok"));
    let p = JmapProvider::from_client(c, "admin@example.com");

    let err = JmapWatcher::subscribe(
        &p,
        PushConfig::for_types(&["Email"]),
        CancellationToken::new(),
    )
    .await
    .err()
    .expect("缺能力位必须失败");
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(
        mock.calls().iter().all(|c| c.kind != RequestKind::Events),
        "能力位为 false 时不该白建一次长连接"
    );
}

// ---------------------------------------------------------------------------
// RFC 8887 WebSocket 推送（S6-17）：watcher 通道选择与会话语义
// ---------------------------------------------------------------------------

use crossmail_jmap::ws::{WsFrame, WsPushSession};

const WS_URL: &str = "wss://host:10000/jmap/ws";

/// 出站帧由 mock 的回显任务异步记录，测试里必须有界等待而非裸断言。
async fn until_ws_sent(mock: &MockTransport, n: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let sent = mock.ws_sent();
        if sent.len() >= n || Instant::now() > deadline {
            return sent;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn ws_state(state: &str, push_state: Option<&str>) -> WsFrame {
    let mut body = json!({
        "@type": "StateChange",
        "changed": { "b": { "Email": state } }
    });
    if let Some(ps) = push_state {
        body["pushState"] = json!(ps);
    }
    WsFrame::Text(body.to_string())
}

/// 服务端声明 WS 能力时优先走 WebSocket，且注册帧语义正确：
/// SSE 语法里的通配 `*` 必须翻译成 `dataTypes: null`，而不是原样发出去。
#[tokio::test]
async fn ws_watcher_prefers_websocket_and_translates_wildcard() {
    let mock = MockTransport::new();
    mock.push_ws_frames(vec![ws_state("w1", None)]);
    let p = provider(&mock);

    let mut watcher = JmapWatcher::subscribe(&p, PushConfig::default(), CancellationToken::new())
        .await
        .expect("订阅应成功");
    assert!(watcher.using_websocket(), "真机声明 WS 能力时应优先 WS");

    let sent = until_ws_sent(&mock, 1).await;
    assert!(!sent.is_empty(), "必须发出 WebSocketPushEnable 注册帧");
    let enable: Value = serde_json::from_str(&sent[0]).unwrap();
    assert_eq!(enable["@type"], "WebSocketPushEnable");
    assert_eq!(enable["dataTypes"], Value::Null, "通配 * 的 WS 语义是 null");
    assert!(enable.get("pushState").is_none(), "首连没有已知 pushState");

    let signal = watcher.next_signal().await.expect("读信号失败");
    match signal {
        Some(RemoteSignal::Changed {
            account,
            data_type,
            state,
        }) => {
            assert_eq!(account, "b");
            assert_eq!(data_type, DATA_TYPE_EMAIL);
            assert_eq!(state.as_deref(), Some("w1"));
        }
        other => panic!("期望 Changed，实际 {other:?}"),
    }
}

/// 协议层活性（Ping/Pong）与 SSE 心跳同语义：适配层必须吞掉。
#[tokio::test]
async fn ws_watcher_swallows_activity_frames() {
    let mock = MockTransport::new();
    mock.push_ws_frames(vec![WsFrame::Activity, ws_state("w2", None)]);
    let p = provider(&mock);

    let mut watcher = JmapWatcher::subscribe(
        &p,
        PushConfig::for_types(&["Email"]),
        CancellationToken::new(),
    )
    .await
    .expect("订阅应成功");

    let signal = watcher.next_signal().await.expect("读信号失败");
    match signal {
        Some(RemoteSignal::Changed { state, .. }) => assert_eq!(state.as_deref(), Some("w2")),
        other => panic!("期望 Changed，实际 {other:?}"),
    }
}

/// 服务端声明了 WS 但握手被拒：降级 EventSource，语义不缩水。
#[tokio::test]
async fn ws_watcher_falls_back_to_sse_when_handshake_refused() {
    let mock = MockTransport::new();
    // 队列里只有 SSE 响应：模拟「WS 握手失败 / 服务端实际不接受」。
    mock.push_stream(vec![state_chunk("s5")]);
    let p = provider(&mock);

    let mut watcher = JmapWatcher::subscribe(
        &p,
        PushConfig::for_types(&["Email"]),
        CancellationToken::new(),
    )
    .await
    .expect("降级后订阅应成功");
    assert!(!watcher.using_websocket(), "握手被拒必须回落 SSE");

    let signal = watcher.next_signal().await.expect("读信号失败");
    match signal {
        Some(RemoteSignal::Changed { state, .. }) => assert_eq!(state.as_deref(), Some("s5")),
        other => panic!("期望 Changed，实际 {other:?}"),
    }
}

/// `RequestError`（注册被拒/认证过期）必须立即报错，不能静默空转。
#[tokio::test]
async fn ws_request_error_surfaces_immediately() {
    let mock = MockTransport::new();
    mock.push_ws_frames(vec![WsFrame::Text(
        json!({
            "@type": "RequestError",
            "requestId": null,
            "status": 401,
            "detail": "authentication expired"
        })
        .to_string(),
    )]);
    let p = provider(&mock);

    let mut watcher = JmapWatcher::subscribe(
        &p,
        PushConfig::for_types(&["Email"]),
        CancellationToken::new(),
    )
    .await
    .expect("订阅（建连）阶段成功");
    let err = watcher.next_signal().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Server, "{err}");
}

/// 断线重连语义与 SSE 对齐：先 `Resync` 再吐新变化；`pushState` 记录后
/// 在重连的注册帧里回传（RFC 8887 的宜补加速器，不是正确性依赖）。
#[tokio::test]
async fn ws_session_reconnect_emits_resync_and_carries_push_state() {
    let mock = MockTransport::new();
    // 第一条连接：一个带 pushState 的变更，随后服务端关闭（帧耗尽即 EOF）。
    mock.push_ws_frames(vec![ws_state("w1", Some("bbb"))]);
    // 第二条连接：重连后的新变更。
    mock.push_ws_frames(vec![ws_state("w2", None)]);

    let mut cfg = PushConfig::for_types(&["Email"]);
    cfg.reconnect_base = Duration::from_millis(1);
    let mut session = WsPushSession::new(cfg, CancellationToken::new());

    let connect = || async { mock.connect_ws(WS_URL, None).await };
    session.connect(connect).await.expect("首连应成功");

    match session.next(connect).await.unwrap() {
        PushUpdate::State(change) => {
            assert_eq!(change.state_of("b", "Email"), Some(&"w1".to_string()))
        }
        other => panic!("期望 State，实际 {other:?}"),
    }
    // 连接关闭 → 重连成功 → 必须先给 Resync（断线期间的缺口不能假装没有）。
    assert_eq!(session.next(connect).await.unwrap(), PushUpdate::Resync);
    match session.next(connect).await.unwrap() {
        PushUpdate::State(change) => {
            assert_eq!(change.state_of("b", "Email"), Some(&"w2".to_string()))
        }
        other => panic!("期望 State，实际 {other:?}"),
    }

    let sent = until_ws_sent(&mock, 2).await;
    assert!(sent.len() >= 2, "重连后必须重新注册，实际出站帧: {sent:?}");
    let re_enable: Value = serde_json::from_str(&sent[1]).unwrap();
    assert_eq!(
        re_enable["pushState"], "bbb",
        "pushState 应随重连注册帧回传"
    );
}

/// 一次性订阅（closeafter=state）：收到首个 state 后显式发
/// `WebSocketPushDisable` 注销并正常结束，不是错误。
#[tokio::test]
async fn ws_session_single_shot_sends_disable_and_ends() {
    let mock = MockTransport::new();
    mock.push_ws_frames(vec![ws_state("w1", None)]);

    let cfg = PushConfig::single_shot(&["Email"]);
    let mut session = WsPushSession::new(cfg, CancellationToken::new());
    let connect = || async { mock.connect_ws(WS_URL, None).await };
    session.connect(connect).await.expect("建连应成功");

    match session.next(connect).await.unwrap() {
        PushUpdate::State(_) => {}
        other => panic!("期望 State，实际 {other:?}"),
    }
    assert_eq!(
        session.next(connect).await.unwrap(),
        PushUpdate::EndOfStream,
        "一次性订阅的关闭是正常结束"
    );

    let sent = until_ws_sent(&mock, 2).await;
    let frames: Vec<Value> = sent
        .iter()
        .map(|f| serde_json::from_str(f).unwrap())
        .collect();
    assert!(
        frames.iter().any(|f| f["@type"] == "WebSocketPushDisable"),
        "收尾必须显式注销，实际出站帧: {frames:?}"
    );
}
