//! 把 JMAP 推送流量适配成协议无关的 [`RemoteWatcher`]（S6-13 / RFC 8887 补齐）。
//!
//! 这一层**只做翻译**，不做决策：把 `PushUpdate` 翻成 `RemoteSignal`，把心跳丢掉，
//! 把 `EndOfStream` 翻成 `Closed`。同步策略（合并窗口、退避、何时真跑一次同步）
//! 全在 `crossmail-sync` 的驱动里，协议层不掺和。
//!
//! 通道选择：服务端声明 `urn:ietf:params:jmap:websocket`（RFC 8887）且握手注册
//! 成功时走 WebSocket；否则回落到 EventSource（RFC 8620 §7.3）。降级是**构建时
//! 一次性决策**——订阅建成后就锁定通道，不做运行中切换（切换必然伴随断线，
//! 断线路径两边语义一致：`Resync` 重新对齐）。
//!
//! 四处必须做对的地方：
//!
//! 1. **心跳必须在适配层吞掉**（SSE 是 `event: ping`，WS 是协议层 Ping/Pong）。
//!    心跳冒到同步层就是周期性无意义全量拉取。
//! 2. **一个 `StateChange` 可以覆盖多个数据类型**（`changed[账号][Email|Mailbox]`）。
//!    拆成逐类型信号，同步层才能按自己关心的类型过滤，而不是被动接受全部。
//! 3. **`changed` 为空时按 `Resync` 处理**。服务端只说「有变化」却没说哪类，属于
//!    信息不足；此时保守地对齐一次，好过静默当作无变化。
//! 4. **订阅返回前必须已完成建连**（两个通道同一条硬要求），调用方随后拉的初始
//!    快照才不会漏掉「建连之后、快照之前」的变更。

use crate::client::JmapClient;
use crate::provider::JmapProvider;
use crate::push::{PushConfig, PushSession, PushUpdate, StateChange};
use crate::transport::{ReqwestTransport, Transport};
use crate::ws::WsPushSession;
use async_trait::async_trait;
use crossmail_core::error::MailResult;
use crossmail_core::provider::CancellationToken;
use crossmail_core::watch::{RemoteSignal, RemoteWatcher};
use std::collections::VecDeque;

/// JMAP 推送 watcher：持有一条已建立的推送连接，按需翻译成 [`RemoteSignal`]。
///
/// 借用 `JmapProvider` 而不是自带客户端，是因为会话层需要 client 来建连/重连；
/// 把 client 复制一份会让两条连接各自缓存 session，状态容易漂。
pub struct JmapWatcher<'a, T: Transport = ReqwestTransport> {
    client: &'a JmapClient<T>,
    /// EventSource 兜底会话（未用 WS 时为唯一活跃通道）。
    session: PushSession,
    /// WebSocket 首选会话：握手注册成功才有值。
    ws: Option<WsPushSession>,
    /// WS 重连地址（`ws` 活跃时必为 Some）。
    ws_url: Option<String>,
    /// 一个 `StateChange` 拆出的多条信号先排队，逐条吐出。
    pending: VecDeque<RemoteSignal>,
}

impl<'a, T: Transport> JmapWatcher<'a, T> {
    /// 建订阅并**立即建连**后再返回。
    ///
    /// 顺序是硬要求：调用方拿到 watcher 之后才会去拉初始快照。如果先拉快照再建连，
    /// 两者之间的变更不会被推过来，也没有任何提示——静默丢事件。这里把「已订阅」
    /// 做成返回值的先决条件，而不是调用方的一句口头约定。
    ///
    /// 通道选择：session 声明 WebSocket 推送且握手注册成功 → WS；
    /// 否则（未声明 / 握手失败 / Mock 环境不支持）→ EventSource，语义不缩水。
    pub async fn subscribe(
        provider: &'a JmapProvider<T>,
        cfg: PushConfig,
        cancel: CancellationToken,
    ) -> MailResult<Self> {
        let client = provider.client();
        let mut session = provider.watch(cfg.clone(), cancel.clone()).await?;

        let mut ws = None;
        let mut ws_url = None;
        if let Some(url) = client.session().await.ok().and_then(|s| s.websocket_url()) {
            let mut ws_session = WsPushSession::new(cfg.clone(), cancel.clone());
            let connect = || {
                let url = url.clone();
                async move { client.connect_ws_push(&url).await }
            };
            match ws_session.connect(connect).await {
                Ok(()) => {
                    ws = Some(ws_session);
                    ws_url = Some(url);
                }
                Err(_) => {
                    // 服务端声明了 WS 但握手/注册失败：按能力降级矩阵走 SSE，
                    // 而不是把推送整个判死。
                    session.connect(client).await?;
                }
            }
        } else {
            session.connect(client).await?;
        }

        Ok(Self {
            client,
            session,
            ws,
            ws_url,
            pending: VecDeque::new(),
        })
    }

    /// 当前是否走 WebSocket 通道（live 测试与排障用）。
    pub fn using_websocket(&self) -> bool {
        self.ws.is_some()
    }

    /// 底层推送会话（排障用：看退避、看是否断线）。
    pub fn session(&self) -> &PushSession {
        &self.session
    }

    fn expand(&mut self, change: &StateChange) {
        for (account, types) in &change.changed {
            for (data_type, state) in types {
                self.pending.push_back(RemoteSignal::changed(
                    account.clone(),
                    data_type.clone(),
                    Some(state.clone()),
                ));
            }
        }
    }
}

#[async_trait]
impl<'a, T: Transport> RemoteWatcher for JmapWatcher<'a, T> {
    async fn next_signal(&mut self) -> MailResult<Option<RemoteSignal>> {
        loop {
            if let Some(signal) = self.pending.pop_front() {
                return Ok(Some(signal));
            }

            let update = match self.ws.as_mut() {
                Some(ws_session) => {
                    let url = self.ws_url.clone().expect("WS 会话活跃时 ws_url 必然存在");
                    let client = self.client;
                    let connect = || {
                        let url = url.clone();
                        async move { client.connect_ws_push(&url).await }
                    };
                    ws_session.next(connect).await?
                }
                None => self.session.next(self.client).await?,
            };

            match update {
                // 心跳只证明连接还活着，不是数据。丢掉，继续等真正的变化。
                PushUpdate::Ping => continue,
                PushUpdate::Resync => return Ok(Some(RemoteSignal::Resync)),
                PushUpdate::EndOfStream => return Ok(Some(RemoteSignal::Closed)),
                PushUpdate::State(change) => {
                    self.expand(&change);
                    if self.pending.is_empty() {
                        // 服务端只说「变了」没说哪类：信息不足，按对齐处理。
                        return Ok(Some(RemoteSignal::Resync));
                    }
                }
            }
        }
    }
}
