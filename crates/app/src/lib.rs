//! CrossMail 编排层（S6-16）：把「账号 + 凭证」变成可同步的 provider，跑一次性同步或推送驱动同步。
//!
//! ## 为什么单独一个 crate
//!
//! 到 S6-15 为止，`service` 刻意不碰网络（纯本地业务层），`ffi` 刻意只做本地门面
//! （连 tokio 都没有），而 `imap`/`jmap`/`sync` 各自是协议能力。三者都能跑，但**没有一处
//! 生产代码把它们接起来**——provider 只在 live 测试里手工 `connect`。UI 想「刷新一次」
//! 或「订阅新邮件推送」时，缺的正是这条编排链路。本 crate 补上它，且只依赖 `core`，
//! 不含任何 uniffi/UI 概念，便于离线用假 provider/watcher 完整回归。
//!
//! ## 线程模型（三处来自既有代码的硬约束）
//!
//! 1. **`MailProvider` 非 dyn 兼容**：它有 `async fn` 且带 `&dyn ProgressReporter` 参数，
//!    `SyncEngine<P>` 必须泛型实例化。故按协议用 [`Backend`] 枚举分派，不做 `Box<dyn …>`。
//! 2. **`JmapWatcher` 借用 `&JmapProvider`**（有生命周期 `'a`），不能 `'static`。
//!    所以整个推送循环放进**单个 [`tokio::runtime::Runtime::block_on`] 栈帧**：provider、
//!    engine、watcher 全在同一作用域，future 无需 `'static`。`subscribe_push` 在独立 OS
//!    线程里建 runtime 再 `block_on`，句柄持有取消令牌。
//! 3. **回调绝不在 tokio worker 上直调跨语言 UI**：`run_push_sync` 的 `on_event` 是同步
//!    `FnMut`，在 worker 线程里执行；若直接调 Kotlin/Swift/Python 回调，会吊住 worker
//!    （GIL/主线程/重入）。故事件经 `std::sync::mpsc` 派发到**专用派发线程**再回调。
//!
//! 明文凭证只在本 crate 的函数参数里短暂存在，构造完 provider 即离开作用域，不落盘、
//! 不进任何 DTO、不写日志。

mod backend;
mod drive;
mod notice;
mod subscription;

pub use backend::{build_backend, Backend};
pub use drive::{
    fetch_email_blocking, run_push_session, send_message_blocking, sync_account_blocking,
    sync_account_once, to_notice,
};
pub use notice::{MailboxUnread, PushNotice, PushReason, SyncSummary};
pub use subscription::{subscribe_push, PushHandle};
