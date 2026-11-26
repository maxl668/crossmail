//! JMAP Provider 适配层（S3-02）：把 JMAP 客户端包装成统一 `MailProvider`。
//!
//! 上层业务只见 `MailProvider`，不感知协议类型（A-001）。
//! 本层只做「协议能力 → 统一语义」的映射，不含业务规则（幂等/冲突由 sync 引擎负责）。

use crate::auth::AuthMethod;
use crate::client::JmapClient;
use crate::parse::{build_email_create, EmailBlob, Identity};
use crate::push::{PushConfig, PushSession};
use crate::sync::SetOutcome;
use crate::transport::{ReqwestTransport, Transport};
use async_trait::async_trait;
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Flag, Id, Mailbox, MailboxRole, OutgoingMessage, ProtocolKind};
use crossmail_core::provider::{
    AccountInfo, BlobRef, CancellationToken, IdRemap, MailProvider, MailboxDraft, ProgressReporter,
    ProviderCapabilities, SentReceipt, SyncPage, SyncRequest,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::RwLock;
use uuid::Uuid;

/// JMAP 账号的 `MailProvider` 实现。
pub struct JmapProvider<T: Transport = ReqwestTransport> {
    client: JmapClient<T>,
    email_address: String,
    /// EmailSubmission 的 identityId；缺省时由 `Identity/get` 按邮箱地址解析。
    ///
    /// **不能拿邮箱地址直接当 identityId**：RFC 8621 §6 的 `Identity.id` 是服务端
    /// 自建标识（真机 Stalwart 上 `admin@example.com` → `"b"`），填邮箱地址会被
    /// 拒绝为 `invalidProperties[identityId]`。
    identity_id: Option<String>,
    /// 能力缓存：`MailProvider::capabilities` 是同步方法，无法在此 await。
    caps: RwLock<ProviderCapabilities>,
}

impl JmapProvider<ReqwestTransport> {
    /// 以「endpoint + 认证」直接构造（S1 已完成会话抓取逻辑，这里只做包装）。
    pub fn new(
        endpoint: impl Into<String>,
        auth: AuthMethod,
        email_address: impl Into<String>,
    ) -> Self {
        let email = email_address.into();
        Self::from_client(
            JmapClient::new(ReqwestTransport::default(), endpoint, auth),
            email,
        )
    }
}

impl<T: Transport> JmapProvider<T> {
    pub fn from_client(client: JmapClient<T>, email_address: impl Into<String>) -> Self {
        Self {
            client,
            email_address: email_address.into(),
            identity_id: None,
            caps: RwLock::new(ProviderCapabilities {
                jmap_mail: true,
                ..ProviderCapabilities::default()
            }),
        }
    }

    /// 指定 EmailSubmission 使用的 identityId。
    pub fn with_identity(mut self, identity_id: impl Into<String>) -> Self {
        self.identity_id = Some(identity_id.into());
        self
    }

    pub fn client(&self) -> &JmapClient<T> {
        &self.client
    }

    pub fn email_address(&self) -> &str {
        &self.email_address
    }

    /// 列出服务端可用发送身份（UI 选身份 / 排障用）。
    pub async fn identities(&self) -> MailResult<Vec<Identity>> {
        self.client.identity_get().await
    }

    /// 订阅账号状态变化（RFC 8620 §7.3 EventSource）。
    ///
    /// 返回的 [`PushSession`] 由调用方驱动——每次 `next(self.client())` 取一条更新，
    /// 内部自动处理断线重连与「可能漏了事件」的 `Resync` 提示。这样既不需要把
    /// 连接塞进后台任务（provider 不持有 `'static` 自引用），也能用 Mock 通道完整
    /// 覆盖重连路径。
    pub async fn watch(
        &self,
        cfg: PushConfig,
        cancel: CancellationToken,
    ) -> MailResult<PushSession> {
        let session = self.client.session().await?;
        // 两个推送入口任一可用即可订阅：只有 eventSourceUrl 或只有 WebSocket
        // （RFC 8887）的服务端都不该在这里被拒——watcher 会按可用性选择通道。
        let sse_ok = session.provider_capabilities().push;
        let ws_ok = session.websocket_url().is_some();
        if !sse_ok && !ws_ok {
            return Err(MailError::Unsupported(
                "服务端未提供 eventSourceUrl 或 WebSocket 推送能力，无法订阅推送".into(),
            ));
        }
        Ok(PushSession::new(cfg, cancel))
    }

    /// 服务端声明的两个推送入口（排障用）：`(EventSource, WebSocket)`。
    ///
    /// WebSocket（RFC 8887）真机声明了 `supportsPush: true`，但本版本只有 HTTP 通道，
    /// 推送统一走 EventSource；此处暴露地址供上层探测与未来扩展。
    pub async fn push_endpoints(&self) -> MailResult<(Option<String>, Option<String>)> {
        let session = self.client.session().await?;
        Ok((session.event_source_url.clone(), session.websocket_url()))
    }

    async fn refresh_caps(&self) -> MailResult<ProviderCapabilities> {
        let caps = self.client.capabilities().await?;
        if let Ok(mut guard) = self.caps.write() {
            *guard = caps.clone();
        }
        Ok(caps)
    }

    async fn mailboxes(&self) -> MailResult<Vec<Mailbox>> {
        Ok(self.client.mailbox_get(None).await?.mailboxes)
    }

    async fn mailbox_by_role(&self, role: MailboxRole) -> MailResult<Id> {
        self.mailboxes()
            .await?
            .into_iter()
            .find(|m| m.role == role)
            .map(|m| m.id)
            .ok_or_else(|| MailError::Unsupported(format!("服务端缺少 {role:?} 文件夹")))
    }
}

/// `Mailbox/set` 部分失败必须抛出，不能当作成功静默吞掉（S1 契约一致）。
fn ensure_set_ok(outcome: &SetOutcome, what: &str) -> MailResult<()> {
    let failed = !outcome.not_created.is_empty()
        || !outcome.not_updated.is_empty()
        || !outcome.not_destroyed.is_empty();
    if failed {
        return Err(MailError::Protocol(format!(
            "{what} 部分失败: {}",
            crate::sync::describe_set_failures(outcome)
        )));
    }
    Ok(())
}

#[async_trait]
impl<T: Transport + 'static> MailProvider for JmapProvider<T> {
    fn protocol(&self) -> ProtocolKind {
        ProtocolKind::Jmap
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.caps.read().map(|c| c.clone()).unwrap_or_default()
    }

    async fn verify_credentials(&self) -> MailResult<AccountInfo> {
        let caps = self.refresh_caps().await?;
        Ok(AccountInfo {
            account_id: self.client.account_id().await?,
            email_address: self.email_address.clone(),
            capabilities: caps,
        })
    }
    async fn list_mailboxes(&self) -> MailResult<Vec<Mailbox>> {
        let account_id = self.client.account_id().await?;
        let mut list = self.mailboxes().await?;
        for m in &mut list {
            if m.account_id.is_empty() {
                m.account_id = account_id.clone();
            }
        }
        Ok(list)
    }

    async fn create_mailbox(&self, draft: MailboxDraft) -> MailResult<Mailbox> {
        let id = self
            .client
            .create_mailbox(&draft.name, draft.parent_id.as_deref())
            .await?;
        let account_id = self.client.account_id().await?;
        let mut mb = Mailbox::new(id, account_id, draft.name.clone());
        mb.parent_id = draft.parent_id;
        mb.role = draft.role;
        Ok(mb)
    }

    async fn rename_mailbox(&self, id: &Id, name: &str) -> MailResult<()> {
        let mut update = BTreeMap::new();
        update.insert(id.clone(), json!({ "name": name }));
        let outcome = self
            .client
            .mailbox_set(BTreeMap::new(), update, &[])
            .await?;
        ensure_set_ok(&outcome, "Mailbox/set(rename)")
    }

    async fn delete_mailbox(&self, id: &Id) -> MailResult<()> {
        let outcome = self
            .client
            .mailbox_set(BTreeMap::new(), BTreeMap::new(), std::slice::from_ref(id))
            .await?;
        ensure_set_ok(&outcome, "Mailbox/set(destroy)")
    }

    async fn sync_emails(
        &self,
        mut req: SyncRequest,
        cancel: CancellationToken,
        reporter: Option<&dyn ProgressReporter>,
    ) -> MailResult<SyncPage> {
        if req.account_id.is_empty() {
            req.account_id = self.client.account_id().await?;
        }
        let page = self.client.sync_emails(&req, &cancel, reporter).await?;
        // 能力可能随会话刷新而变化，这里顺带更新缓存
        let _ = self.refresh_caps().await;
        Ok(page)
    }

    async fn fetch_email(&self, id: &Id, include_body: bool) -> MailResult<crossmail_core::Email> {
        if id.is_empty() {
            return Err(MailError::InvalidInput("邮件 id 为空".into()));
        }
        self.client
            .email_get(std::slice::from_ref(id), include_body)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| MailError::NotFound(format!("邮件 {id} 不存在")))
    }

    async fn set_flags(&self, ids: &[Id], flag: Flag, enabled: bool) -> MailResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let outcome = self.client.set_keywords(ids, &flag, enabled).await?;
        ensure_set_ok(&outcome, "Email/set(keywords)")
    }

    async fn move_emails(&self, ids: &[Id], from: &Id, to: &Id) -> MailResult<Vec<IdRemap>> {
        if ids.is_empty() {
            return Err(MailError::InvalidInput("未选择邮件".into()));
        }
        if from == to {
            return Ok(Vec::new());
        }
        let outcome = self.client.move_emails(ids, from, to).await?;
        ensure_set_ok(&outcome, "Email/set(move)")?;
        // JMAP 的 emailId 稳定（移动只是改 mailboxIds），不存在身份漂移
        Ok(Vec::new())
    }

    async fn copy_emails(&self, ids: &[Id], to: &Id) -> MailResult<()> {
        if ids.is_empty() {
            return Err(MailError::InvalidInput("未选择邮件".into()));
        }
        let outcome = self.client.copy_emails(ids, to).await?;
        ensure_set_ok(&outcome, "Email/set(copy)")
    }

    async fn delete_emails(&self, ids: &[Id]) -> MailResult<()> {
        if ids.is_empty() {
            return Err(MailError::InvalidInput("未选择邮件".into()));
        }
        let trash = self.mailbox_by_role(MailboxRole::Trash).await.ok();
        let outcome = self.client.delete_emails(ids, trash.as_deref()).await?;
        ensure_set_ok(&outcome, "Email/set(delete)")
    }

    async fn save_draft(&self, msg: &OutgoingMessage) -> MailResult<Id> {
        msg.validate().map_err(MailError::InvalidInput)?;
        let draft_id = self.mailbox_by_role(MailboxRole::Drafts).await?;

        let mut blobs = Vec::new();
        for attachment in &msg.attachments {
            let uploaded = self
                .client
                .upload_blob(&attachment.data, &attachment.content_type)
                .await?;
            blobs.push(EmailBlob {
                blob_id: uploaded.id,
                name: attachment.name.clone(),
                content_type: uploaded.content_type,
                size: uploaded.size,
            });
        }

        let from = msg
            .from
            .first()
            .ok_or_else(|| MailError::InvalidInput("缺少发件人".into()))?;
        let message_id = format!("<{}@{}>", Uuid::new_v4(), from.domain());
        let draft = build_email_create(msg, &draft_id, &message_id, &blobs);

        let mut create = BTreeMap::new();
        create.insert("draft".to_string(), draft);
        let outcome = self.client.email_set(create, BTreeMap::new(), &[]).await?;
        ensure_set_ok(&outcome, "Email/set(create draft)")?;
        outcome
            .created
            .get("draft")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| MailError::Protocol("Email/set 未返回草稿 id".into()))
    }

    async fn send_email(&self, msg: OutgoingMessage) -> MailResult<SentReceipt> {
        let identity = match self.identity_id.clone() {
            Some(id) => id,
            None => self.client.resolve_identity_id(&self.email_address).await?,
        };
        self.client.submit_email(&msg, &identity).await
    }

    async fn upload_blob(&self, data: &[u8], content_type: &str) -> MailResult<BlobRef> {
        self.client.upload_blob(data, content_type).await
    }

    async fn download_blob(&self, id: &str) -> MailResult<Vec<u8>> {
        self.client
            .download_blob(id, "blob", "application/octet-stream")
            .await
    }
}
