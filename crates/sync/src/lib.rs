//! CrossMail 同步引擎（Sprint 3）。
//!
//! 分层：`conflict`（冲突裁决）→ `engine`（增量同步 + 幂等落库 + 状态校准）
//! → `offline`（离线队列回放）→ `scheduler`（四类触发与批量合并）。
//! 本 crate 只依赖 `MailProvider` 契约，不感知具体协议。

pub mod conflict;
pub mod engine;
pub mod offline;
pub mod scheduler;
pub mod testing;
pub mod wakeup;

pub use conflict::{merge, pending_ops_for_email, Dimension, MergeOutcome};
pub use engine::{ApplyStats, ConflictReport, EngineConfig, SyncEngine, SyncOutcome};
pub use offline::{OfflineReplayer, ReplayConfig, ReplayError, ReplayReport};
pub use scheduler::{Decision, RunReason, Scheduler, SchedulerConfig, Trigger};
pub use wakeup::{run_push_sync, PushSyncConfig, PushSyncEvent, PushSyncReport, WakeupReason};

pub const CRATE_NAME: &str = "crossmail-sync";
