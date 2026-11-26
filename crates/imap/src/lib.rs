//! IMAP4rev1 + SMTP 协议核心（Sprint S2）。
//!
//! 分层：
//! - `atom` / `response`：响应词法与语法解析（纯函数，可离线单测）
//! - `capability` / `conn`：能力协商、TLS 策略、指数退避重连
//! - `bodystructure` / `uid` / `utf7`：BODYSTRUCTURE、UID 集合、文件夹名编码
//! - `cte`：MIME 传输编码解码（base64 / quoted-printable）
//! - `sync`：UIDPLUS / CONDSTORE 增量与降级链
//! - `client` / `smtp`：命令-响应循环与发信
//! - `draft` / `gmail` / `autoconfig`：草稿合并、Gmail 标签适配、自动配置
//! - `idle`：IDLE 推送（I-008），把「选中文件夹有变化」变成 `RemoteSignal`

pub mod atom;
pub mod autoconfig;
pub mod bodystructure;
pub mod capability;
pub mod client;
pub mod conn;
pub mod cte;
pub mod draft;
pub mod gmail;
pub mod idle;
pub mod provider;
pub mod response;
pub mod rfc2047;
pub mod smtp;
pub mod sync;
pub mod transport;
pub mod uid;
pub mod utf7;

pub use client::{ExpungeStrategy, ImapClient, MoveOutcome, MoveStrategy, SelectedState};
pub use idle::{IdleConfig, ImapWatcher};
pub use provider::{ImapProvider, ImapProviderConfig, SmtpAuth, SmtpConnector, TcpSmtpConnector};
pub use smtp::SmtpClient;
pub use transport::{connect_tls, connect_tls_endpoint, Endpoint};

pub const CRATE_NAME: &str = "crossmail-imap";
