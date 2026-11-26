//! 同步执行：一次性同步 + 推送驱动会话。
//!
//! [`run_push_session`] 把 `run_push_sync` 与协议 watcher 拼起来，并把内部的
//! `PushSyncEvent` 收敛成对外 [`PushNotice`]（附带同步后的未读角标）。整个函数被
//! `subscription` 放进**同一个 `block_on` 栈帧**调用——因为 `JmapWatcher` 借用 provider，
//! 不能 `'static`，也不能拆进 `tokio::spawn`。

use crate::backend::{build_backend, Backend};
use crate::notice::{MailboxUnread, PushNotice, PushReason, SyncSummary};
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Account, OutgoingMessage};
use crossmail_core::provider::{CancellationToken, MailProvider, SentReceipt};
use crossmail_imap::idle::IdleConfig;
use crossmail_jmap::push::PushConfig;
use crossmail_jmap::watcher::JmapWatcher;
use crossmail_storage::Database;
use crossmail_sync::engine::SyncEngine;
use crossmail_sync::{
    run_push_sync, PushSyncConfig, PushSyncEvent, PushSyncReport, SyncOutcome, WakeupReason,
};
use std::sync::{Arc, Mutex};

/// IMAP IDLE 固定监听收件箱；推送增量同步也必须限定同一文件夹。
const IMAP_IDLE_MAILBOX: &str = "INBOX";

/// 同步阻塞版一次性同步：给 FFI 的同步接口用，runtime 细节留在本层。
///
/// FFI 所有方法都是同步阻塞的；在这里建一个当前线程的 tokio runtime 并 `block_on`，
/// 避免把 uniffi 门面和 tokio 概念搅在一起。IMAP 建连是异步的，必须在 runtime 内。
///
/// JMAP 的 `SyncRequest.mailbox_id = None` 表示整账号同步；IMAP provider 则强制要求
/// 非空（见 `crates/imap/src/provider.rs` 的 `sync_emails`），所以两条协议走不同编排。
pub fn sync_account_blocking(
    db: &Arc<Mutex<Database>>,
    account: &Account,
    secret: &str,
    cancel: CancellationToken,
) -> MailResult<SyncSummary> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MailError::Storage(format!("同步运行时启动失败：{e}")))?;
    runtime.block_on(async {
        match build_backend(account, secret).await? {
            Backend::Jmap(provider) => sync_account_once(db, provider, &account.id, cancel).await,
            Backend::Imap(provider) => {
                sync_imap_account_once(db, provider, &account.id, cancel).await
            }
        }
    })
}

/// 按需回源拉取单封邮件正文（阻塞版），给 FFI 的 `read_body` / `render_body` 用。
///
/// 增量同步按 I-005 只拉元数据，正文在首次阅读时才取，避免初始同步被大附件拖垮。
/// UI 门面全是同步接口，所以 runtime 细节留在本层，与 `sync_account_blocking` 一致。
/// provider 的 `fetch_part` 用 `BODY.PEEK[]`，不会把这封邮件标成已读。
pub fn fetch_email_blocking(
    account: &Account,
    secret: &str,
    email_id: &str,
) -> MailResult<crossmail_core::model::Email> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MailError::Storage(format!("正文拉取运行时启动失败：{e}")))?;
    let id = email_id.to_string();
    runtime.block_on(async {
        match build_backend(account, secret).await? {
            Backend::Jmap(provider) => provider.fetch_email(&id, true).await,
            Backend::Imap(provider) => provider.fetch_email(&id, true).await,
        }
    })
}

/// 发信（阻塞版），给 FFI 的 `send_email` 用。
///
/// FFI 门面是同步接口，SMTP 建连/认证/DATA 都是异步的，所以 runtime 细节集中在
/// 本层（与 `sync_account_blocking` 一致）。按账号协议分派：IMAP 走 SMTP 提交，
/// JMAP 走 `EmailSubmission`。provider 的 SMTP 通道已在 `build_backend` 里按
/// 账号 `smtp` 配置装配，缺配置时返回「该账号未配置 SMTP 发信」而不是超时。
///
/// 明文凭证只作为参数存在，构造完 provider 即离开作用域，不落盘、不写日志。
pub fn send_message_blocking(
    account: &Account,
    secret: &str,
    message: OutgoingMessage,
) -> MailResult<SentReceipt> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| MailError::Storage(format!("发信运行时启动失败：{e}")))?;
    runtime.block_on(async {
        match build_backend(account, secret).await? {
            Backend::Jmap(provider) => provider.send_email(message).await,
            Backend::Imap(provider) => provider.send_email(message).await,
        }
    })
}

/// 把服务器文件夹列表落库。
///
/// 同步引擎 `apply_page` 会对每封邮件写 `email_mailboxes(email_id, mailbox_id)`，
/// 该表外键指向 `mailboxes(id)`。真连账号的本地库若是空的（或只有本地自建的
/// 系统文件夹），服务器返回的 id（JMAP 常是 `b`/`d` 这类短 id）根本不存在，
/// 初始快照必然 `FOREIGN KEY constraint failed`。编排层必须在每次真同步前
/// 把 provider 的 folder 元数据对齐进本地库——这正是本 crate 的职责。
/// `account_id` 是**本地**账号 id：provider 返回的 folder 同样带协议侧 accountId，
/// 必须一并归一，否则 `mailboxes.account_id` 外键指向不存在的 accounts 行。
/// 返回落库的服务器文件夹 id 列表，供 IMAP 逐文件夹同步使用。
async fn seed_mailboxes<P: MailProvider>(
    db: &Arc<Mutex<Database>>,
    provider: &Arc<P>,
    account_id: &str,
) -> MailResult<Vec<String>> {
    let mut mailboxes = provider.list_mailboxes().await?;
    for mailbox in &mut mailboxes {
        mailbox.account_id = account_id.to_string();
    }
    let ids: Vec<String> = mailboxes.iter().map(|m| m.id.clone()).collect();
    let guard = db
        .lock()
        .map_err(|_| MailError::Storage("数据库锁已损坏".into()))?;
    for mailbox in &mailboxes {
        guard.upsert_mailbox(mailbox)?;
    }
    Ok(ids)
}

/// 一次性同步一个账号（JMAP / 整账号语义）。返回变更摘要，供 `sync_account` FFI 方法用。
/// 保持原签名不变：`crates/app/tests/app_tests.rs` 的 mock provider 单测依赖它。
pub async fn sync_account_once<P: MailProvider>(
    db: &Arc<Mutex<Database>>,
    provider: Arc<P>,
    account_id: &str,
    cancel: CancellationToken,
) -> MailResult<SyncSummary> {
    let _ = seed_mailboxes(db, &provider, account_id).await?;
    let engine = SyncEngine::new(provider, db.clone());
    let (outcome, _conflicts) = engine
        .sync_with_report(account_id, None, cancel, None)
        .await?;
    Ok(summary_of(&outcome))
}

/// IMAP 专用一次性同步：provider 的 `sync_emails` 要求 `SyncRequest.mailbox_id` 非空，
/// 所以先把服务器文件夹落库，再逐文件夹跑增量同步并汇总成一份摘要。
/// 每个文件夹有独立游标（`SyncEngine::cursor_key`），互不干扰。
async fn sync_imap_account_once<P: MailProvider>(
    db: &Arc<Mutex<Database>>,
    provider: Arc<P>,
    account_id: &str,
    cancel: CancellationToken,
) -> MailResult<SyncSummary> {
    let ids = seed_mailboxes(db, &provider, account_id).await?;
    let engine = SyncEngine::new(provider, db.clone());
    sync_mailbox_ids(&engine, account_id, &ids, cancel).await
}

/// 按文件夹列表依次同步，累加计数并合并标志位。任一文件夹失败即整体失败，
/// 游标只在单个文件夹成功应用后推进，不会造成半套数据。
async fn sync_mailbox_ids<P: MailProvider>(
    engine: &SyncEngine<P>,
    account_id: &str,
    ids: &[String],
    cancel: CancellationToken,
) -> MailResult<SyncSummary> {
    if ids.is_empty() {
        return Err(MailError::InvalidInput(
            "IMAP 账号没有可同步的文件夹".into(),
        ));
    }
    let mut total = SyncSummary {
        inserted: 0,
        updated: 0,
        deleted: 0,
        unchanged: 0,
        has_more: false,
        full_resync: false,
        new_state: None,
    };
    for id in ids {
        let (outcome, _conflicts) = engine
            .sync_with_report(account_id, Some(id.clone()), cancel.clone(), None)
            .await?;
        let part = summary_of(&outcome);
        total.inserted += part.inserted;
        total.updated += part.updated;
        total.deleted += part.deleted;
        total.unchanged += part.unchanged;
        total.has_more |= part.has_more;
        total.full_resync |= part.full_resync;
        total.new_state = part.new_state.or(total.new_state);
    }
    Ok(total)
}

fn summary_of(outcome: &SyncOutcome) -> SyncSummary {
    SyncSummary {
        inserted: outcome.stats.inserted as u64,
        updated: outcome.stats.updated as u64,
        deleted: outcome.stats.deleted as u64,
        unchanged: outcome.stats.unchanged as u64,
        has_more: outcome.has_more,
        full_resync: outcome.full_resync,
        new_state: outcome.new_state.clone(),
    }
}

fn map_reason(reason: WakeupReason) -> PushReason {
    match reason {
        WakeupReason::Push => PushReason::Push,
        WakeupReason::Resync => PushReason::Resync,
        WakeupReason::Closing => PushReason::Closing,
    }
}

/// 从本地库读当前未读角标：`(全账号合计, 分箱明细)`。
///
/// 这是同步成功后才读的（`run_push_sync` 已 apply 完再回调），所以拿到的是最新值。
/// 锁污染或查询失败时返回 `(0, [])` 而不是编造——角标宁可暂时为 0，也不推错数。
fn read_unread(db: &Arc<Mutex<Database>>, account_id: &str) -> (u64, Vec<MailboxUnread>) {
    let Ok(guard) = db.lock() else {
        return (0, Vec::new());
    };
    let Ok(map) = guard.unread_counts(account_id) else {
        return (0, Vec::new());
    };
    let by: Vec<MailboxUnread> = map
        .iter()
        .map(|(id, n)| MailboxUnread {
            mailbox_id: id.clone(),
            unread: u64::from(*n),
        })
        .collect();
    let total = by.iter().map(|m| m.unread).sum();
    (total, by)
}

/// 把一条内部驱动事件收敛成对外通知（附带未读角标）。抽成函数是为了离线单测覆盖。
pub fn to_notice(event: PushSyncEvent, db: &Arc<Mutex<Database>>, account_id: &str) -> PushNotice {
    match event {
        PushSyncEvent::Synced {
            reason,
            outcome,
            coalesced,
            ..
        } => {
            let (unread_total, unread_by_mailbox) = read_unread(db, account_id);
            PushNotice::Synced {
                reason: map_reason(reason),
                summary: summary_of(&outcome),
                unread_total,
                unread_by_mailbox,
                coalesced: coalesced as u64,
            }
        }
        PushSyncEvent::Failed {
            reason,
            error,
            retry_in,
        } => PushNotice::Failed {
            reason: map_reason(reason),
            error,
            retry_in_ms: retry_in.map(|d| d.as_millis() as u64),
        },
        PushSyncEvent::Closed => PushNotice::Closed,
    }
}

/// 跑一轮完整的推送订阅：建 watcher → 拉初始快照 → 事件驱动同步。
///
/// **顺序是硬要求**：watcher 返回时连接已建立；先拉初始快照再进驱动循环，
/// 否则「建连之后、驱动之前」到达的变更会被静默丢掉（见 `run_push_sync` 文档）。
///
/// `emit` 是同步回调，在 tokio worker 线程里执行；调用方必须只在里面做「发进 channel」
/// 这类无阻塞动作，真正的跨语言回调交给 `subscription` 的派发线程。
pub async fn run_push_session<F>(
    db: &Arc<Mutex<Database>>,
    account_id: &str,
    backend: Backend,
    cancel: CancellationToken,
    mut emit: F,
) -> MailResult<PushSyncReport>
where
    F: FnMut(PushNotice),
{
    let aid = account_id.to_string();
    let events_db = db.clone();

    // 每条事件都在成功同步之后收敛，读到的未读角标是最新值。
    let mut forward = |event: PushSyncEvent| emit(to_notice(event, &events_db, &aid));

    match backend {
        Backend::Jmap(provider) => {
            let engine = SyncEngine::new(provider.clone(), db.clone());
            // JMAP 的 None = 整账号同步，驱动循环沿用默认配置。
            let cfg = PushSyncConfig::default();
            // JmapWatcher 借用 provider：两者都只活在这个分支里，不要求 'static。
            let mut watcher =
                JmapWatcher::subscribe(provider.as_ref(), PushConfig::default(), cancel.clone())
                    .await?;
            let _ = seed_mailboxes(db, &provider, account_id).await?;
            engine
                .sync_with_report(account_id, None, cancel.clone(), None)
                .await?;
            run_push_sync(
                &engine,
                account_id,
                &mut watcher,
                &cfg,
                cancel,
                &mut forward,
            )
            .await
        }
        Backend::Imap(provider) => {
            let engine = SyncEngine::new(provider.clone(), db.clone());
            // IDLE 只监听 INBOX，驱动循环的增量同步也必须限定同一文件夹；
            // mailbox_id=None 会被 IMAP provider 判为 InvalidInput。
            let cfg = PushSyncConfig {
                mailbox_id: Some(IMAP_IDLE_MAILBOX.to_string()),
                ..PushSyncConfig::default()
            };
            // IDLE 独占一条独立连接，不受原会话影响。
            let mut watcher = provider
                .watch(IdleConfig::for_mailbox(IMAP_IDLE_MAILBOX), cancel.clone())
                .await?;
            // 初始快照仍按全部服务器文件夹逐个拉取，避免「建连后、驱动前」漏变更。
            let ids = seed_mailboxes(db, &provider, account_id).await?;
            sync_mailbox_ids(&engine, account_id, &ids, cancel.clone()).await?;
            run_push_sync(
                &engine,
                account_id,
                &mut watcher,
                &cfg,
                cancel,
                &mut forward,
            )
            .await
        }
    }
}
