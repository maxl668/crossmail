//! 推送订阅的线程编排与取消句柄。
//!
//! 三条线程各司其职，缺一不可：
//!
//! * **调用线程**：`subscribe_push` 立即返回 [`PushHandle`]，绝不阻塞 UI。
//! * **worker 线程**：自建 tokio runtime，`block_on` 整个 `run_push_session`。
//!   之所以用 OS 线程 + `block_on` 而不是 `tokio::spawn`：JMAP watcher 借用 provider，
//!   future 不是 `'static`，`spawn` 根本不接受。`block_on` 没有这个约束。
//! * **派发线程**：从 `mpsc` 取通知，调跨语言回调。`run_push_sync` 的 `on_event`
//!   在 tokio worker 线程里跑，若在那里直调 Kotlin/Swift/Python 回调，会吊住 worker
//!   （GIL / 主线程亲和 / 重入）。派发线程把这件事挪出 tokio 世界。

use crate::backend::build_backend;
use crate::drive::run_push_session;
use crate::notice::{PushNotice, PushReason};
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::Account;
use crossmail_core::provider::CancellationToken;
use crossmail_storage::Database;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

/// 一个可取消的推送订阅句柄。跨语言侧持有它，`cancel()` 后后台线程会在下一个
/// 检查点退出（IDLE/SSE 的取消轮询粒度都是百毫秒级）。
pub struct PushHandle {
    cancel: CancellationToken,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
}

impl PushHandle {
    /// 请求取消；返回 `true` 表示这是第一次取消（重复调用返回 `false`）。
    pub fn cancel(&self) -> bool {
        let already = self.cancel.is_cancelled();
        self.cancel.cancel();
        !already
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// 等待后台线程收尾。仅供 app 层离线测试与排障，UI 不要在主线程调用。
    pub fn join(&self) {
        let handles: Vec<_> = self
            .workers
            .lock()
            .map(|mut guard| guard.drain(..).collect())
            .unwrap_or_default();
        for handle in handles {
            let _ = handle.join();
        }
    }
}

/// 启动一个推送订阅。
///
/// * `on_notice`：跨语言回调的 Rust 侧包装，在**专用派发线程**里被调用。
/// * 启动失败（线程创建不出来）才返回 `Err`；建连/认证这类运行期失败通过
///   `PushNotice::Failed` 通知，不阻塞启动。
pub fn subscribe_push<F>(
    db: Arc<Mutex<Database>>,
    account: Account,
    secret: String,
    on_notice: F,
) -> MailResult<PushHandle>
where
    F: Fn(PushNotice) + Send + 'static,
{
    let cancel = CancellationToken::new();
    // 无界 channel：`emit` 只做一次 send，绝不因对端慢而阻塞 tokio worker。
    let (tx, rx) = mpsc::channel::<PushNotice>();

    // 派发线程：回调只在这里发生。channel 关闭（worker 结束、所有 tx 被 drop）即退出。
    let dispatcher = std::thread::Builder::new()
        .name("crossmail-push-dispatch".into())
        .spawn(move || {
            for notice in rx {
                on_notice(notice);
            }
        })
        .map_err(|e| MailError::Storage(format!("启动推送派发线程失败：{e}")))?;

    // worker 线程：一个 block_on 栈帧容纳 provider + watcher + engine。
    let worker_cancel = cancel.clone();
    let worker_cancel_check = cancel.clone();
    let worker = std::thread::Builder::new()
        .name("crossmail-push-worker".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    let _ = tx.send(failed_startup(e.to_string()));
                    return;
                }
            };
            runtime.block_on(async move {
                let account_id = account.id.clone();
                let backend = match build_backend(&account, &secret).await {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(PushNotice::Failed {
                            reason: PushReason::Push,
                            error: e.to_string(),
                            retry_in_ms: None,
                        });
                        let _ = tx.send(PushNotice::Closed);
                        return;
                    }
                };
                let failed_seen = Arc::new(AtomicBool::new(false));
                let seen_for_emit = Arc::clone(&failed_seen);
                let ev_tx = tx.clone();
                let emit = move |notice: PushNotice| {
                    if matches!(notice, PushNotice::Failed { .. }) {
                        seen_for_emit.store(true, Ordering::Relaxed);
                    }
                    let _ = ev_tx.send(notice);
                };
                // 启动期失败（建连 / folder 落库 / 初始快照）不走 run_push_sync 的
                // on_event，错误若被吞，UI 只收到 Closed 却永远不知道原因（真机探针
                // 就撞到 FOREIGN KEY 静默失败）。仅在「确实报错」「本轮没发过 Failed」
                // 「非用户主动取消」时补发一条；run_push_sync 的不可重试错误已自带
                // Failed，不重复发。
                if let Err(e) =
                    run_push_session(&db, &account_id, backend, worker_cancel, emit).await
                {
                    if !failed_seen.load(Ordering::Relaxed) && !worker_cancel_check.is_cancelled() {
                        let _ = tx.send(PushNotice::Failed {
                            reason: PushReason::Push,
                            error: e.to_string(),
                            retry_in_ms: None,
                        });
                    }
                }
                // 无论驱动正常结束还是报错，都补一条 Closed，让 UI 知道流已终止。
                let _ = tx.send(PushNotice::Closed);
            });
        })
        .map_err(|e| MailError::Storage(format!("启动推送 worker 线程失败：{e}")))?;

    Ok(PushHandle {
        cancel,
        workers: Mutex::new(vec![worker, dispatcher]),
    })
}

fn failed_startup(detail: String) -> PushNotice {
    PushNotice::Failed {
        reason: PushReason::Push,
        error: format!("推送运行时启动失败：{detail}"),
        retry_in_ms: None,
    }
}
