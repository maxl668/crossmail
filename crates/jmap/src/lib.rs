//! CrossMail JMAP 协议核心（RFC 8620 / 8621，Sprint 1）。
//!
//! 分层：`transport`（可替换的 HTTP 通道）→ `session`（能力协商）→ `auth`（凭证）
//! → `parse`（JSON 归一到统一领域模型）→ `client`（方法调用）→ `sync`（增量同步）
//! → `provider`（S3-02 统一 `MailProvider` 适配层）→ `push`（EventSource 推送）
//! → `watcher`（推送流量 → 协议无关的 `RemoteWatcher`）。

pub mod auth;
pub mod client;
pub mod parse;
pub mod provider;
pub mod push;
pub mod session;
pub mod sync;
pub mod transport;
pub mod watcher;
pub mod ws;

pub use auth::{AuthMethod, BasicAuth, OAuth2State};
pub use client::JmapClient;
pub use parse::Identity;
pub use provider::JmapProvider;
pub use push::{
    parse_state_change, CloseAfter, PushConfig, PushReader, PushSession, PushUpdate, SseDecoder,
    SseEvent, StateChange,
};
pub use session::{Session, MAIL_CAPABILITY};
pub use sync::{Changes, MailboxSetOutcome, QueryPage, SetOutcome, SubmissionOutcome};
pub use watcher::JmapWatcher;
pub use ws::{WsFrame, WsPushSession, WsStream};

pub const CRATE_NAME: &str = "crossmail-jmap";
