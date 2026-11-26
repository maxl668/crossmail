//! CrossMail 业务层（S4）：账号、邮件、草稿、搜索、凭证密文仓、正文缓存。
//!
//! 边界：本层不加网络，只做「本地状态 + 业务规则」；真正与服务器打交道的是
//! `crossmail-jmap` / `crossmail-imap` 的 Provider 实现，同步调度在 `crossmail-sync`。

pub mod account;
pub mod cache;
pub mod credential;
pub mod diagnostics;
pub mod mail;
pub mod render;
pub mod search;

use crossmail_core::{MailError, MailResult};
use crossmail_storage::Database;
use std::sync::{Arc, Mutex, MutexGuard};

pub use account::{AccountPatch, AccountService, Endpoint, NewAccount, ProtocolSetup};
pub use cache::{BodyCache, CacheConfig, CacheKind, EvictionReport};
pub use credential::{
    is_sealed, ChaChaSecretCodec, MemorySecretCodec, SecretCodec, SecretVault, SEALED_PREFIX,
};
pub use diagnostics::{DiagnosticsService, RECENT_LOG_LINES};
pub use mail::{DeletedEmail, Draft, MailService};
pub use render::{
    AllowScope, AssetPermission, RenderRequest, RenderService, RenderedBody, ONCE_TTL_MINUTES,
};
pub use search::{SearchScope, SearchService};

/// 统一短锁：SQLite 连接不是 `Sync`，业务层全程串行访问；
/// 调用方必须在拿到守卫的那一小段内用完，禁止跨 await 或再进另一把锁。
pub(crate) fn lock_db(db: &Arc<Mutex<Database>>) -> MailResult<MutexGuard<'_, Database>> {
    db.lock()
        .map_err(|_| MailError::Storage("数据库锁已损坏".into()))
}
