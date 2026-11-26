//! 测试替身：可编排的 `MailProvider`，用于同步引擎 / 离线回放 / 调度器的一致性验证。

use async_trait::async_trait;
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Email, Flag, Id, Mailbox, OutgoingMessage, ProtocolKind};
use crossmail_core::provider::{
    AccountInfo, BlobRef, CancellationToken, IdRemap, MailProvider, MailboxDraft, ProgressReporter,
    ProviderCapabilities, SentReceipt, SyncPage, SyncRequest,
};
use std::collections::VecDeque;
use std::sync::Mutex;

/// 编排好的失败类型，覆盖「可重试 / 不可重试」两类分支。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockFailure {
    Network,
    RateLimited,
    Protocol,
    Unsupported,
    /// 凭证过期 / 被吊销：不可自动重试，但用户重新认证后即可继续
    Authentication,
    /// 服务端 5xx：按 HTTP/IMAP 惯例属临时故障，可重试
    ServerError,
    /// 目标资源已不存在：重试一万次也不会变，必须终止
    NotFound,
    /// 传输层超时：与网络中断同属可重试
    Timeout,
}

impl MockFailure {
    fn to_error(self) -> MailError {
        match self {
            MockFailure::Network => MailError::Network("模拟网络中断".into()),
            MockFailure::RateLimited => MailError::RateLimited("模拟限流".into()),
            MockFailure::Protocol => MailError::Protocol("模拟协议错误".into()),
            MockFailure::Unsupported => MailError::Unsupported("模拟能力缺失".into()),
            MockFailure::Authentication => MailError::Authentication("模拟凭证已过期".into()),
            MockFailure::ServerError => MailError::Server {
                status: 503,
                message: "模拟服务端暂时不可用".into(),
            },
            MockFailure::NotFound => MailError::NotFound("模拟邮件已不存在".into()),
            MockFailure::Timeout => MailError::Network("模拟读取超时".into()),
        }
    }
}

#[derive(Default)]
pub struct MockProvider {
    pages: Mutex<VecDeque<SyncPage>>,
    failures: Mutex<VecDeque<MockFailure>>,
    calls: Mutex<Vec<String>>,
    mailboxes: Mutex<Vec<Mailbox>>,
    sent: Mutex<Vec<OutgoingMessage>>,
    caps: ProviderCapabilities,
    /// 移动时是否回传身份映射（模拟 IMAP 的 UID 漂移）。默认关，需要时开。
    remap_on_move: std::sync::atomic::AtomicBool,
}

/// 模拟「服务器给这封邮件分配了新 UID」：从旧主键里取 UID 段加 1000，保证换新且可预测。
fn moved_uid(id: &str) -> String {
    let old = id.rsplit('/').next().unwrap_or("0");
    let n: u32 = old.parse().unwrap_or(0);
    format!("{}", n + 1000)
}

impl MockProvider {
    pub fn new() -> Self {
        Self {
            caps: ProviderCapabilities {
                jmap_mail: true,
                blob_upload: true,
                ..ProviderCapabilities::default()
            },
            ..Self::default()
        }
    }

    pub fn queue_page(&self, page: SyncPage) {
        if let Ok(mut q) = self.pages.lock() {
            q.push_back(page);
        }
    }

    pub fn pending_pages(&self) -> usize {
        self.pages.lock().map(|q| q.len()).unwrap_or(0)
    }

    /// 让下一次协议调用失败（按调用顺序消费）。
    pub fn fail_next(&self, failure: MockFailure) {
        if let Ok(mut q) = self.failures.lock() {
            q.push_back(failure);
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }

    /// 打开「移动后身份漂移」模拟（IMAP + UIDPLUS 的真实行为）。
    pub fn with_remap_on_move(self) -> Self {
        self.remap_on_move
            .store(true, std::sync::atomic::Ordering::Relaxed);
        self
    }

    fn emits_remap(&self) -> bool {
        self.remap_on_move
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn call_count(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }

    pub fn set_mailboxes(&self, list: Vec<Mailbox>) {
        if let Ok(mut m) = self.mailboxes.lock() {
            *m = list;
        }
    }

    pub fn sent_messages(&self) -> Vec<OutgoingMessage> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn record(&self, call: impl Into<String>) {
        if let Ok(mut c) = self.calls.lock() {
            c.push(call.into());
        }
    }

    fn maybe_fail(&self) -> MailResult<()> {
        let failure = self.failures.lock().ok().and_then(|mut q| q.pop_front());
        match failure {
            Some(f) => Err(f.to_error()),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl MailProvider for MockProvider {
    fn protocol(&self) -> ProtocolKind {
        ProtocolKind::Jmap
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.caps.clone()
    }

    async fn verify_credentials(&self) -> MailResult<AccountInfo> {
        self.record("verify");
        Ok(AccountInfo {
            account_id: "acc".into(),
            email_address: "user@example.com".into(),
            capabilities: self.caps.clone(),
        })
    }

    async fn list_mailboxes(&self) -> MailResult<Vec<Mailbox>> {
        self.record("list_mailboxes");
        self.maybe_fail()?;
        Ok(self.mailboxes.lock().map(|m| m.clone()).unwrap_or_default())
    }

    async fn create_mailbox(&self, draft: MailboxDraft) -> MailResult<Mailbox> {
        self.record(format!("create_mailbox:{}", draft.name));
        self.maybe_fail()?;
        let mut mb = Mailbox::new(format!("mb-{}", draft.name), "acc", draft.name);
        mb.role = draft.role;
        Ok(mb)
    }

    async fn rename_mailbox(&self, id: &Id, name: &str) -> MailResult<()> {
        self.record(format!("rename_mailbox:{id}:{name}"));
        self.maybe_fail()
    }

    async fn delete_mailbox(&self, id: &Id) -> MailResult<()> {
        self.record(format!("delete_mailbox:{id}"));
        self.maybe_fail()
    }

    async fn sync_emails(
        &self,
        req: SyncRequest,
        _cancel: CancellationToken,
        _reporter: Option<&dyn ProgressReporter>,
    ) -> MailResult<SyncPage> {
        self.record(format!(
            "sync:{}",
            req.mailbox_id.clone().unwrap_or_else(|| "*".into())
        ));
        self.maybe_fail()?;
        let page = self.pages.lock().ok().and_then(|mut q| q.pop_front());
        Ok(page.unwrap_or_default())
    }

    async fn fetch_email(&self, id: &Id, _include_body: bool) -> MailResult<Email> {
        self.record(format!("fetch_email:{id}"));
        self.maybe_fail()?;
        Err(MailError::NotFound(format!("{id} 不存在于替身")))
    }

    async fn set_flags(&self, ids: &[Id], flag: Flag, enabled: bool) -> MailResult<()> {
        self.record(format!("set_flags:{}:{flag:?}:{enabled}", ids.join(",")));
        self.maybe_fail()
    }

    async fn move_emails(&self, ids: &[Id], from: &Id, to: &Id) -> MailResult<Vec<IdRemap>> {
        self.record(format!("move:{}:{from}->{to}", ids.join(",")));
        self.maybe_fail()?;
        // 模拟 IMAP 的 UID 漂移：新主键 = 目标文件夹 + 后缀 `-moved`。
        // 想验「移动后队列里的后续操作被改写」，就得让 Mock 真的换 id。
        if self.emits_remap() {
            return Ok(ids
                .iter()
                .map(|id| IdRemap::new(id.clone(), format!("{to}/{}", moved_uid(id))))
                .collect());
        }
        Ok(Vec::new())
    }

    async fn copy_emails(&self, ids: &[Id], to: &Id) -> MailResult<()> {
        self.record(format!("copy:{}->{to}", ids.join(",")));
        self.maybe_fail()
    }

    async fn delete_emails(&self, ids: &[Id]) -> MailResult<()> {
        self.record(format!("delete:{}", ids.join(",")));
        self.maybe_fail()
    }

    async fn save_draft(&self, msg: &OutgoingMessage) -> MailResult<Id> {
        self.record("save_draft");
        self.maybe_fail()?;
        Ok(format!("draft-{}", msg.subject))
    }

    async fn send_email(&self, msg: OutgoingMessage) -> MailResult<SentReceipt> {
        self.record(format!("send:{}", msg.subject));
        self.maybe_fail()?;
        if let Ok(mut s) = self.sent.lock() {
            s.push(msg);
        }
        Ok(SentReceipt {
            message_id: "<mock@example.com>".into(),
            server_id: None,
            sent_mailbox_id: None,
            sent_at: chrono::Utc::now(),
        })
    }

    async fn upload_blob(&self, data: &[u8], content_type: &str) -> MailResult<BlobRef> {
        self.record("upload_blob");
        self.maybe_fail()?;
        Ok(BlobRef {
            id: format!("blob-{}", data.len()),
            size: data.len() as u64,
            content_type: content_type.to_string(),
        })
    }

    async fn download_blob(&self, id: &str) -> MailResult<Vec<u8>> {
        self.record(format!("download_blob:{id}"));
        self.maybe_fail()?;
        Ok(Vec::new())
    }
}
