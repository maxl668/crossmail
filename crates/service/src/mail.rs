//! 邮件核心业务（S4-05）：列表 / 阅读 / 撰写 / 草稿 / 标记 / 移动 / 归档 / 删除。
//!
//! 设计原则：
//! - 本地先生效（UI 立刻有反馈），同时把动作写入离线队列，联网后由 S3 回放层推到服务端；
//! - 所有写操作先读后写，读不到就是 NotFound，不做「静默成功」；
//! - 正文走文件缓存，元数据走 SQLite，避免大字段撑爆列表查询。

use chrono::Utc;
use crossmail_core::provider::OperationKind;
use crossmail_core::{
    Address, Attachment, Email, Flag, Id, MailError, MailResult, Mailbox, MailboxRole,
    OfflineOperation, OutgoingAttachment, OutgoingMessage,
};
use crossmail_storage::{Database, EmailPage, EmailQuery};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::cache::{BodyCache, CacheKind};
use crate::lock_db;

/// 草稿写入参数（S4-05 撰写）。
#[derive(Debug, Clone)]
pub struct Draft {
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub bcc: Vec<Address>,
    pub subject: String,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub attachments: Vec<OutgoingAttachment>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

impl Draft {
    pub fn new(subject: impl Into<String>) -> Self {
        Self {
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.into(),
            text_body: None,
            html_body: None,
            attachments: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
        }
    }

    pub fn to(mut self, addrs: Vec<Address>) -> Self {
        self.to = addrs;
        self
    }

    pub fn text(mut self, body: impl Into<String>) -> Self {
        self.text_body = Some(body.into());
        self
    }

    pub fn html(mut self, body: impl Into<String>) -> Self {
        self.html_body = Some(body.into());
        self
    }

    /// 直接发送路径：把草稿内容整成待发送消息（UI「立即发送」不落草稿时用）。
    pub fn to_outgoing(&self, account_id: &str, from: Address) -> MailResult<OutgoingMessage> {
        let mut msg = OutgoingMessage::new(account_id, from, self.to.clone(), self.subject.clone());
        msg.cc = self.cc.clone();
        msg.bcc = self.bcc.clone();
        msg.text_body = self.text_body.clone();
        msg.html_body = self.html_body.clone();
        msg.attachments = self.attachments.clone();
        msg.in_reply_to = self.in_reply_to.clone();
        msg.references = self.references.clone();
        msg.validate().map_err(MailError::InvalidInput)?;
        Ok(msg)
    }

    /// 草稿阶段只校验地址格式：允许还没填收件人，但填了就必须是合法地址。
    pub fn validate_addresses(&self) -> MailResult<()> {
        for addr in self.to.iter().chain(self.cc.iter()).chain(self.bcc.iter()) {
            if !addr.is_valid() {
                return Err(MailError::InvalidInput(format!(
                    "收件人地址无效: {}",
                    addr.email
                )));
            }
        }
        Ok(())
    }
}

/// 邮件业务服务。
///
/// `Clone` 是廉价句柄语义（两个 `Arc`），用于让渲染层复用同一套缓存与数据库。
#[derive(Clone)]
pub struct MailService {
    db: Arc<Mutex<Database>>,
    cache: Option<Arc<BodyCache>>,
}

impl MailService {
    pub fn new(db: Arc<Mutex<Database>>) -> Self {
        Self { db, cache: None }
    }

    /// 带正文缓存的服务（阅读正文、草稿附件需要）。
    pub fn with_cache(db: Arc<Mutex<Database>>, cache: Arc<BodyCache>) -> Self {
        Self {
            db,
            cache: Some(cache),
        }
    }

    pub fn cache(&self) -> Option<&Arc<BodyCache>> {
        self.cache.as_ref()
    }

    // ------------------------------------------------------------ 列表 / 阅读

    pub fn list(&self, account_id: &str, q: &EmailQuery) -> MailResult<EmailPage> {
        lock_db(&self.db)?.page_emails(account_id, q)
    }

    pub fn unread_counts(
        &self,
        account_id: &str,
    ) -> MailResult<std::collections::BTreeMap<Id, u32>> {
        lock_db(&self.db)?.unread_counts(account_id)
    }

    pub fn list_mailboxes(&self, account_id: &str) -> MailResult<Vec<Mailbox>> {
        lock_db(&self.db)?.list_mailboxes(account_id)
    }

    pub fn read(&self, email_id: &str) -> MailResult<Email> {
        lock_db(&self.db)?
            .get_email(email_id)?
            .ok_or_else(|| MailError::NotFound(format!("邮件不存在: {email_id}")))
    }

    /// 读正文：优先文件缓存，其次库里登记的正文路径，都没有返回 None（需回源拉取）。
    ///
    /// 回退规则是**单向**的：请求 HTML 但只有纯文本时回退到纯文本（很多邮件本来
    /// 就只有 text/plain，直接返回空会让 UI 白屏）；反过来请求纯文本却不回退到
    /// HTML——把 HTML 源码塞进纯文本视图比空着更糟。
    pub fn read_body(&self, email_id: &str, html: bool) -> MailResult<Option<String>> {
        if let Some(cache) = &self.cache {
            if html {
                if let Some(body) = cache.load_text(email_id, CacheKind::HtmlBody)? {
                    return Ok(Some(body));
                }
                if let Some(body) = cache.load_text(email_id, CacheKind::TextBody)? {
                    return Ok(Some(body));
                }
            } else if let Some(body) = cache.load_text(email_id, CacheKind::TextBody)? {
                return Ok(Some(body));
            }
        }
        let email = self.read(email_id)?;
        Ok(if html {
            email.html_body.or(email.text_body)
        } else {
            email.text_body
        })
    }

    /// 正文落缓存 + 回写元数据里的正文路径（此后 `get_email` 也能直接带出正文）。
    pub fn save_body(
        &self,
        email_id: &str,
        text_body: Option<&str>,
        html_body: Option<&str>,
    ) -> MailResult<()> {
        let cache = self
            .cache
            .as_ref()
            .ok_or_else(|| MailError::Unsupported("未配置缓存目录，无法落盘正文".into()))?;
        let mut text_path: Option<String> = None;
        if let Some(t) = text_body {
            let p = cache.store(email_id, CacheKind::TextBody, None, t.as_bytes())?;
            text_path = Some(p.to_string_lossy().to_string());
        }
        if let Some(h) = html_body {
            cache.store(email_id, CacheKind::HtmlBody, None, h.as_bytes())?;
        }
        lock_db(&self.db)?.set_email_body_path(email_id, text_path.as_deref())?;
        cache.enforce_quota()?;
        Ok(())
    }

    // ------------------------------------------------------------ 标记 / 移动

    /// 设置已读/未读：本地立刻生效 + 变更入队。
    pub fn set_read(&self, email_id: &str, read: bool) -> MailResult<Email> {
        self.set_flag(email_id, Flag::Seen, read)
    }

    pub fn toggle_star(&self, email_id: &str, starred: bool) -> MailResult<Email> {
        self.set_flag(email_id, Flag::Flagged, starred)
    }

    pub fn set_flag(&self, email_id: &str, flag: Flag, enabled: bool) -> MailResult<Email> {
        let mut email = self.read(email_id)?;
        email.set_flag(flag.clone(), enabled);
        lock_db(&self.db)?.upsert_email(&email)?;
        self.enqueue(
            &email.account_id,
            OperationKind::SetFlag {
                email_id: email.id.clone(),
                flag,
                enabled,
            },
        )?;
        Ok(email)
    }

    /// 移动到指定文件夹（本地生效 + 入队）。
    pub fn move_to(&self, email_id: &str, target_mailbox: &str) -> MailResult<Email> {
        let mut email = self.read(email_id)?;
        let from = email
            .mailbox_ids
            .first()
            .cloned()
            .ok_or_else(|| MailError::InvalidInput("邮件不在任何文件夹中".into()))?;
        if from == target_mailbox {
            return Ok(email);
        }
        // 目标文件夹必须存在，否则等于把邮件丢进黑洞
        let exists = lock_db(&self.db)?
            .list_mailboxes(&email.account_id)?
            .into_iter()
            .any(|m| m.id == target_mailbox);
        if !exists {
            return Err(MailError::NotFound(format!(
                "目标文件夹不存在: {target_mailbox}"
            )));
        }
        email.mailbox_ids = vec![target_mailbox.to_string()];
        lock_db(&self.db)?.upsert_email(&email)?;
        self.enqueue(
            &email.account_id,
            OperationKind::Move {
                email_ids: vec![email.id.clone()],
                from,
                to: target_mailbox.to_string(),
            },
        )?;
        Ok(email)
    }

    /// 归档：移到 Archive 角色的文件夹。
    pub fn archive(&self, email_id: &str) -> MailResult<Email> {
        let email = self.read(email_id)?;
        let from = email
            .mailbox_ids
            .first()
            .cloned()
            .ok_or_else(|| MailError::InvalidInput("邮件不在任何文件夹中".into()))?;
        let archive = self
            .mailbox_by_role(&email.account_id, MailboxRole::Archive)?
            .ok_or_else(|| MailError::Unsupported("该账号没有归档文件夹".into()))?;
        let mut updated = email.clone();
        updated.mailbox_ids = vec![archive.id.clone()];
        lock_db(&self.db)?.upsert_email(&updated)?;
        self.enqueue(
            &updated.account_id,
            OperationKind::Archive {
                email_ids: vec![updated.id.clone()],
                from,
            },
        )?;
        Ok(updated)
    }

    /// 删除：优先移入回收站（可恢复），没有回收站才真删本地 + 入队 Delete。
    pub fn delete(&self, email_id: &str) -> MailResult<DeletedEmail> {
        let email = self.read(email_id)?;
        let trash = self.mailbox_by_role(&email.account_id, MailboxRole::Trash)?;
        match trash {
            Some(trash) if !email.mailbox_ids.contains(&trash.id) => {
                let mut moved = self.move_to(email_id, &trash.id)?;
                moved.set_flag(Flag::Deleted, true);
                lock_db(&self.db)?.upsert_email(&moved)?;
                // `from` 指向回收站：上一步的 Move 会把邮件放过去，Delete 紧随其后回放。
                // 即使服务器的 Move 没给 COPYUID（拿不到 remap、id 里的文件夹仍是旧的），
                // 回放层也能凭 `from` 把操作打到正确位置，而不是对着旧 UID 空转。
                self.enqueue(
                    &email.account_id,
                    OperationKind::Delete {
                        email_ids: vec![email.id.clone()],
                        from: trash.id.clone(),
                    },
                )?;
                Ok(DeletedEmail {
                    email_id: email.id,
                    trashed: true,
                })
            }
            _ => {
                lock_db(&self.db)?.delete_email(&email.id)?;
                self.enqueue(
                    &email.account_id,
                    OperationKind::Delete {
                        email_ids: vec![email.id.clone()],
                        from: email.mailbox_ids.first().cloned().unwrap_or_default(),
                    },
                )?;
                Ok(DeletedEmail {
                    email_id: email.id,
                    trashed: false,
                })
            }
        }
    }

    // ------------------------------------------------------------ 草稿 / 发送

    /// 保存草稿（S4-05 撰写）：草稿进 Drafts 文件夹，正文与附件进文件缓存。
    pub fn save_draft(&self, account_id: &str, draft: &Draft) -> MailResult<Email> {
        if draft.subject.trim().is_empty()
            && draft.to.is_empty()
            && draft.text_body.as_deref().unwrap_or("").trim().is_empty()
        {
            return Err(MailError::InvalidInput("草稿为空，无需保存".into()));
        }
        draft.validate_addresses()?;
        let drafts_box = self.ensure_mailbox(account_id, MailboxRole::Drafts, "Drafts")?;

        let id = format!("{account_id}/{}", Uuid::new_v4());
        let mut email = Email::new(id.clone(), account_id, drafts_box.id.clone());
        // 草稿也要有 From：阅读/预览草稿时会用到发件人身份，缺了它
        // 安全渲染层无法判定发件人（会把空发件人误报成可疑）。
        if let Ok(from) = self.account_address(account_id) {
            email.from = vec![from];
        }
        email.subject = draft.subject.clone();
        email.to = draft.to.clone();
        email.cc = draft.cc.clone();
        email.bcc = draft.bcc.clone();
        email.in_reply_to = draft.in_reply_to.clone();
        email.references = draft.references.clone();
        email.sent_at = None;
        email.received_at = Utc::now();
        email.flags = vec![Flag::Draft];
        email.text_body = draft.text_body.clone();
        email.html_body = draft.html_body.clone();
        email.refresh_preview();
        email.attachments = draft
            .attachments
            .iter()
            .enumerate()
            .map(|(i, a)| Attachment {
                id: format!("{id}/att{i}"),
                name: Some(a.name.clone()),
                content_type: a.content_type.clone(),
                size: a.data.len() as u64,
                blob_id: None,
                part_id: Some(format!("att{i}")),
                inline: false,
                cid: None,
            })
            .collect();

        // 正文与附件先落盘，再落库并回写正文路径，保证「库里指得到的文件一定存在」
        let mut text_path: Option<String> = None;
        if let Some(cache) = &self.cache {
            for (i, a) in draft.attachments.iter().enumerate() {
                cache.store(
                    &id,
                    CacheKind::Attachment,
                    Some(&format!("att{i}")),
                    &a.data,
                )?;
            }
            if let Some(t) = &draft.text_body {
                let p = cache.store(&id, CacheKind::TextBody, None, t.as_bytes())?;
                text_path = Some(p.to_string_lossy().to_string());
            }
            if let Some(h) = &draft.html_body {
                cache.store(&id, CacheKind::HtmlBody, None, h.as_bytes())?;
            }
        }
        lock_db(&self.db)?.upsert_email(&email)?;
        if let Some(p) = text_path {
            lock_db(&self.db)?.set_email_body_path(&id, Some(&p))?;
        }
        if let Some(cache) = &self.cache {
            cache.enforce_quota()?;
        }
        Ok(email)
    }

    pub fn list_drafts(&self, account_id: &str, limit: u32) -> MailResult<Vec<Email>> {
        let Some(drafts_box) = self.mailbox_by_role(account_id, MailboxRole::Drafts)? else {
            return Ok(Vec::new());
        };
        let page = self.list(
            account_id,
            &EmailQuery::new()
                .in_mailbox(drafts_box.id)
                .with_limit(limit),
        )?;
        Ok(page.items)
    }

    /// 把草稿整理成待发送消息（附件从缓存读回），供 Provider 发送。
    pub fn prepare_send(&self, draft_id: &str) -> MailResult<OutgoingMessage> {
        let email = self.read(draft_id)?;
        let from = self.account_address(&email.account_id)?;
        let mut attachments = Vec::new();
        for a in &email.attachments {
            let data = match (&self.cache, &a.part_id) {
                (Some(cache), Some(part)) => cache
                    .load(&email.id, CacheKind::Attachment, Some(part))?
                    .ok_or_else(|| MailError::Storage(format!("草稿附件不在缓存中: {}", a.id)))?,
                _ => {
                    return Err(MailError::Unsupported(
                        "草稿包含附件但未配置缓存目录".into(),
                    ))
                }
            };
            attachments.push(OutgoingAttachment {
                name: a.name.clone().unwrap_or_else(|| "attachment".into()),
                content_type: a.content_type.clone(),
                data,
            });
        }
        let mut msg = OutgoingMessage::new(
            email.account_id.clone(),
            from,
            email.to.clone(),
            email.subject.clone(),
        );
        msg.cc = email.cc.clone();
        msg.bcc = email.bcc.clone();
        msg.text_body = email.text_body.or_else(|| {
            self.cache
                .as_ref()
                .and_then(|c| c.load_text(&email.id, CacheKind::TextBody).ok().flatten())
        });
        msg.html_body = email.html_body;
        msg.attachments = attachments;
        msg.in_reply_to = email.in_reply_to;
        msg.references = email.references;
        msg.draft_id = Some(email.id);
        msg.validate().map_err(MailError::InvalidInput)?;
        Ok(msg)
    }

    /// 发送成功后的本地收尾：草稿出 Drafts，进 Sent（或直接删除草稿）。
    pub fn mark_sent(&self, draft_id: &str, sent_mailbox: Option<&str>) -> MailResult<Email> {
        let mut email = self.read(draft_id)?;
        email.set_flag(Flag::Draft, false);
        email.set_flag(Flag::Seen, true);
        email.sent_at = Some(Utc::now());
        email.received_at = Utc::now();
        match sent_mailbox {
            Some(mid) => email.mailbox_ids = vec![mid.to_string()],
            None => email.mailbox_ids = Vec::new(),
        }
        lock_db(&self.db)?.upsert_email(&email)?;
        Ok(email)
    }

    // ------------------------------------------------------------ 辅助

    pub fn mailbox_by_role(
        &self,
        account_id: &str,
        role: MailboxRole,
    ) -> MailResult<Option<Mailbox>> {
        Ok(lock_db(&self.db)?
            .list_mailboxes(account_id)?
            .into_iter()
            .find(|m| m.role == role))
    }

    /// 保证某角色的文件夹存在（本地缺省时自动建一个占位，等服务端同步回来再对齐）。
    pub fn ensure_mailbox(
        &self,
        account_id: &str,
        role: MailboxRole,
        name: &str,
    ) -> MailResult<Mailbox> {
        if let Some(existing) = self.mailbox_by_role(account_id, role)? {
            return Ok(existing);
        }
        let mut mailbox = Mailbox::new(format!("{account_id}/{role:?}"), account_id, name);
        mailbox.role = role;
        lock_db(&self.db)?.upsert_mailbox(&mailbox)?;
        Ok(mailbox)
    }

    /// 发送成功后的本地记账（IMAP 路径）：把草稿行**对齐**到服务端已发送里的那一份。
    ///
    /// 为什么不能直接 `mark_sent`：`send_email` 已经把同一份报文 APPEND 进服务端 Sent，
    /// 下次同步会把服务端那封按 `账号/文件夹/UID` 拉下来。若本地仍保留草稿转正的那一行
    /// （uid 为空、主键指向草稿箱），已发送里就会出现两封一模一样的邮件，
    /// 用户第一反应是「我是不是发了两遍」。
    ///
    /// `mailbox_id` 必须取 APPEND **实际写入**的文件夹（provider 回执里的
    /// `sent_mailbox_id`），不能拿本地占位的 `账号/Sent` 顶替——两者不是同一个箱子，
    /// 对不上就等于没对齐。`uid` 是服务端分配的 UID，两者一起构成同步用的真实主键。
    pub fn mark_sent_on_server(
        &self,
        draft_id: &str,
        mailbox_id: &str,
        mailbox_name: &str,
        uid: u32,
    ) -> MailResult<Email> {
        let mut email = self.read(draft_id)?;
        let account_id = email.account_id.clone();
        // 账号刚加、还没同步过时，本地库没有这个文件夹的行；但邮件确实已经落进去了。
        // 不补这行，`email_mailboxes` 的外键当场失败，整次发送会被报成「发送失败」——
        // 而邮件其实早就发出去了，用户一重发就是两封。
        self.ensure_mailbox_row(&account_id, mailbox_id, mailbox_name, MailboxRole::Sent)?;

        let new_id = format!("{account_id}/{mailbox_id}/{uid}");
        if new_id != draft_id {
            // 复用「移动后校准」：换主键、换 UID（新文件夹的 UID 空间独立，可能撞车），
            // 并改写离线队列里引用旧 id 的后续操作。
            lock_db(&self.db)?.remap_email(&account_id, draft_id, &new_id, mailbox_id)?;
            email = self.read(&new_id)?;
        }
        email.set_flag(Flag::Draft, false);
        email.set_flag(Flag::Seen, true);
        email.sent_at = Some(Utc::now());
        email.received_at = Utc::now();
        email.mailbox_ids = vec![mailbox_id.to_string()];
        lock_db(&self.db)?.upsert_email(&email)?;
        Ok(email)
    }

    /// 按 **id** 保证文件夹元数据存在（`ensure_mailbox` 是按角色找，这里是按服务端 id）。
    fn ensure_mailbox_row(
        &self,
        account_id: &str,
        mailbox_id: &str,
        name: &str,
        role: MailboxRole,
    ) -> MailResult<()> {
        let db = lock_db(&self.db)?;
        if db
            .list_mailboxes(account_id)?
            .iter()
            .any(|m| m.id == mailbox_id)
        {
            return Ok(());
        }
        let mut mailbox = Mailbox::new(mailbox_id, account_id, name);
        mailbox.role = role;
        db.upsert_mailbox(&mailbox)
    }

    pub fn account_address(&self, account_id: &str) -> MailResult<Address> {
        let account = lock_db(&self.db)?
            .get_account(account_id)?
            .ok_or_else(|| MailError::NotFound(format!("账号不存在: {account_id}")))?;
        Ok(Address {
            name: account.display_name.clone(),
            email: account.email_address,
        })
    }

    fn enqueue(&self, account_id: &str, kind: OperationKind) -> MailResult<()> {
        let op = OfflineOperation::new(Uuid::new_v4().to_string(), account_id, kind);
        lock_db(&self.db)?.enqueue(&op)
    }

    /// 待发队列（UI 可展示"待同步 n 条"）。
    pub fn pending_count(&self, account_id: &str) -> MailResult<usize> {
        Ok(lock_db(&self.db)?.pending_operations(account_id)?.len())
    }
}

/// 删除结果：区分「进回收站」与「直接删掉」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedEmail {
    pub email_id: Id,
    pub trashed: bool,
}
