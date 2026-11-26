//! CrossMail 核心库：统一领域模型、错误体系与协议抽象层。
//!
//! 设计约束（需求表 A-001~A-008）：
//! * 上层业务不感知底层协议类型（JMAP / IMAP+SMTP）
//! * 同一数据结构兼容双协议
//! * 所有耗时操作支持异步与取消
//! * 冲突按「删除 > 移动 > 标记」优先级处理

pub mod error;
pub mod model;
pub mod provider;
pub mod watch;

pub use error::{ErrorKind, MailError, MailResult};
pub use model::*;
pub use provider::*;
pub use watch::{RemoteSignal, RemoteWatcher, DATA_TYPE_EMAIL, DATA_TYPE_MAILBOX};
