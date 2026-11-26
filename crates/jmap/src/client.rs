//! JMAP 客户端：方法调用编排（J-001 ~ J-009）。
//!
//! 单个请求遵循 RFC 8620：`{"using": [...], "methodCalls": [[name, args, id], ...]}`。
//! 响应按 call id 精确匹配；`error` 响应统一映射到 `MailError`。

use crate::auth::AuthMethod;
use crate::parse::{
    build_email_create, parse_email, parse_identities, parse_mailbox, EmailBlob, Identity,
};
use crate::push::{PushConfig, PushReader};
use crate::session::{
    well_known_candidates, Session, BLOB_CAPABILITY, CORE_CAPABILITY, MAIL_CAPABILITY,
    SUBMISSION_CAPABILITY,
};
use crate::sync::{parse_changes, parse_query, parse_set, Changes, QueryPage, SetOutcome};
use crate::transport::{ReqwestTransport, Transport};
use chrono::{DateTime, Utc};
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Address, Email, Flag, Id, Mailbox, MailboxRole, OutgoingMessage};
use crossmail_core::provider::{BlobRef, ProviderCapabilities, SentReceipt};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::RwLock;
use tokio::sync::OnceCell;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MailboxGetOutcome {
    pub mailboxes: Vec<Mailbox>,
    pub state: String,
    pub not_found: Vec<Id>,
}

pub struct JmapClient<T: Transport = ReqwestTransport> {
    transport: T,
    endpoint: String,
    auth: RwLock<AuthMethod>,
    session: OnceCell<Session>,
}

impl<T: Transport> JmapClient<T> {
    pub fn new(transport: T, endpoint: impl Into<String>, auth: AuthMethod) -> Self {
        Self {
            transport,
            endpoint: endpoint.into(),
            auth: RwLock::new(auth),
            session: OnceCell::new(),
        }
    }

    /// 注入已获取的 Session，供离线测试与上层缓存复用。
    pub fn with_session(transport: T, session: Session, auth: AuthMethod) -> Self {
        let client = Self::new(transport, session.api_url.clone(), auth);
        let _ = client.session.set(session);
        client
    }

    /// .well-known/jmap 自动发现（J-001），按候选顺序尝试，任一返回合法 Session 即成功。
    pub async fn discover(
        transport: T,
        email: &str,
        auth: AuthMethod,
    ) -> MailResult<(Self, String)> {
        let header = auth.header();
        let mut last_error = MailError::Unsupported("无可用 JMAP 服务发现地址".into());
        let mut found: Option<(Session, String)> = None;
        for candidate in well_known_candidates(email)? {
            match transport.get_json(&candidate, Some(&header)).await {
                Ok(raw) => match Session::from_value(&raw) {
                    Ok(session) => {
                        found = Some((session, candidate));
                        break;
                    }
                    Err(e) => last_error = e,
                },
                Err(e) => last_error = e,
            }
        }
        let (session, candidate) = found.ok_or(last_error)?;
        Ok((Self::with_session(transport, session, auth), candidate))
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn auth_header(&self) -> String {
        self.auth.read().map(|a| a.header()).unwrap_or_default()
    }

    /// 当前生效的 Authorization 头（供上层诊断与测试断言，不含敏感日志输出）。
    pub fn auth_header_value(&self) -> String {
        self.auth_header()
    }

    pub async fn session(&self) -> MailResult<&Session> {
        self.session
            .get_or_try_init(|| async {
                let raw = self
                    .transport
                    .get_json(&self.endpoint, Some(&self.auth_header()))
                    .await?;
                Session::from_value(&raw)
            })
            .await
    }

    /// 强制刷新 Session（能力变更后重新协商）。
    pub async fn refresh_session(&mut self) -> MailResult<()> {
        let raw = self
            .transport
            .get_json(&self.endpoint, Some(&self.auth_header()))
            .await?;
        let session = Session::from_value(&raw)?;
        self.session = OnceCell::new();
        let _ = self.session.set(session);
        Ok(())
    }

    pub async fn account_id(&self) -> MailResult<Id> {
        self.session().await?.primary_mail_account()
    }

    pub async fn capabilities(&self) -> MailResult<ProviderCapabilities> {
        Ok(self.session().await?.provider_capabilities())
    }

    /// OAuth2 刷新令牌并原地更新 Authorization 头（J-002）。
    pub async fn refresh_oauth2(&self) -> MailResult<()> {
        let (endpoint, form) = {
            let guard = self
                .auth
                .read()
                .map_err(|_| MailError::Protocol("认证状态锁已损坏".into()))?;
            (guard.token_endpoint()?.to_string(), guard.refresh_form()?)
        };
        let response = self.transport.post_form(&endpoint, &form).await?;
        let mut guard = self
            .auth
            .write()
            .map_err(|_| MailError::Protocol("认证状态锁已损坏".into()))?;
        guard.apply_token_response(&response)
    }

    fn using(&self, session: &Session) -> Vec<&'static str> {
        let mut using = vec![CORE_CAPABILITY, MAIL_CAPABILITY];
        if session.has_capability(SUBMISSION_CAPABILITY) {
            using.push(SUBMISSION_CAPABILITY);
        }
        if session.has_capability(BLOB_CAPABILITY) {
            using.push(BLOB_CAPABILITY);
        }
        using
    }

    /// 单方法调用，返回该方法参数对象。
    pub async fn call(&self, method: &str, args: Value) -> MailResult<Value> {
        let mut results = self.call_batch(vec![(method, args)]).await?;
        results
            .pop()
            .map(|(_, args)| args)
            .ok_or_else(|| MailError::Protocol("JMAP 响应为空".into()))
    }

    /// 批量调用；可在 args 里使用 `#creationId` 结果引用保证原子性（J-008）。
    pub async fn call_batch(&self, calls: Vec<(&str, Value)>) -> MailResult<Vec<(String, Value)>> {
        let session = self.session().await?;
        let api_url = session.api_url.clone();
        let using = self.using(session);
        let method_calls: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (name, args))| json!([name, args, format!("c{i}")]))
            .collect();
        let body = json!({
            "using": using,
            "methodCalls": method_calls,
        });
        let response = self
            .transport
            .post_json(&api_url, Some(&self.auth_header()), &body)
            .await?;
        let responses = response
            .get("methodResponses")
            .and_then(Value::as_array)
            .ok_or_else(|| MailError::Protocol("JMAP 响应缺少 methodResponses".into()))?;

        // 同一个 call id 可能对应**多条**响应：RFC 8620 §5.3 的
        // `onSuccessUpdateEmail` / `onSuccessDestroyEmail` 会让服务端在
        // `EmailSubmission/set` 之后再补一条隐式 `Email/set`，而 Stalwart 把它
        // 挂在**同一个 call id** 上返回。用 `insert` 存会互相覆盖，导致
        // 「期望 EmailSubmission/set，实际 Email/set」这种误判——真机发信就卡在这里。
        let mut by_id: BTreeMap<String, Vec<(String, Value)>> = BTreeMap::new();
        for entry in responses {
            let tuple = entry
                .as_array()
                .ok_or_else(|| MailError::Protocol("methodResponse 非数组".into()))?;
            if tuple.len() < 3 {
                return Err(MailError::Protocol("methodResponse 元素不足".into()));
            }
            let name = tuple[0].as_str().unwrap_or("").to_string();
            let args = tuple[1].clone();
            let id = tuple[2].as_str().unwrap_or("").to_string();
            if name == "error" {
                return Err(map_jmap_error(&args));
            }
            by_id.entry(id).or_default().push((name, args));
        }

        calls
            .iter()
            .enumerate()
            .map(|(i, (expected, _))| {
                let id = format!("c{i}");
                let Some(entries) = by_id.get_mut(&id) else {
                    return Err(MailError::Protocol(format!("call {id} 无响应")));
                };
                // 按方法名挑出本次调用真正要的那条，隐式附加响应原样忽略。
                match entries.iter().position(|(name, _)| name == *expected) {
                    Some(pos) => Ok(entries.remove(pos)),
                    None => {
                        let actual: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
                        Err(MailError::Protocol(format!(
                            "call {id} 返回方法不匹配: 期望 {expected}, 实际 {}",
                            actual.join("/")
                        )))
                    }
                }
            })
            .collect()
    }

    // ---- Mailbox (J-003) ----

    pub async fn mailbox_get(&self, ids: Option<&[Id]>) -> MailResult<MailboxGetOutcome> {
        let account_id = self.account_id().await?;
        let args = json!({
            "accountId": account_id,
            "ids": ids,
        });
        let result = self.call("Mailbox/get", args).await?;
        let account_id2 = account_id.clone();
        let list: Vec<Mailbox> = result
            .get("list")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| parse_mailbox(v, &account_id2).ok())
                    .collect()
            })
            .unwrap_or_default();
        Ok(MailboxGetOutcome {
            mailboxes: list,
            state: result
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            not_found: string_array(result.get("notFound")),
        })
    }

    pub async fn mailbox_query(&self, filter: Option<Value>, limit: u32) -> MailResult<Vec<Id>> {
        let account_id = self.account_id().await?;
        let mut args = json!({ "accountId": account_id, "limit": limit });
        if let Some(f) = filter {
            args["filter"] = f;
        }
        let result = self.call("Mailbox/query", args).await?;
        Ok(string_array(result.get("ids")))
    }

    pub async fn mailbox_set(
        &self,
        create: BTreeMap<String, Value>,
        update: BTreeMap<String, Value>,
        destroy: &[Id],
    ) -> MailResult<SetOutcome> {
        let account_id = self.account_id().await?;
        let args = json!({
            "accountId": account_id,
            "create": create,
            "update": update,
            "destroy": destroy,
        });
        let result = self.call("Mailbox/set", args).await?;
        parse_set(&result)
    }

    pub async fn create_mailbox(&self, name: &str, parent_id: Option<&str>) -> MailResult<Id> {
        let mut draft = json!({ "name": name });
        if let Some(parent) = parent_id {
            draft["parentId"] = json!(parent);
        }
        let mut create = BTreeMap::new();
        create.insert("mb".to_string(), draft);
        let outcome = self.mailbox_set(create, BTreeMap::new(), &[]).await?;
        outcome
            .created
            .get("mb")
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| MailError::Protocol("Mailbox/set 未返回新建 id".into()))
    }

    // ---- Email (J-004 / J-005 / J-007 / J-008) ----

    pub async fn email_query(
        &self,
        filter: Option<Value>,
        limit: u32,
        position: u32,
    ) -> MailResult<QueryPage> {
        let account_id = self.account_id().await?;
        let mut args = json!({
            "accountId": account_id,
            "position": position,
            "limit": limit,
        });
        if let Some(f) = filter {
            args["filter"] = f;
        }
        let result = self.call("Email/query", args).await?;
        Ok(parse_query(&result))
    }

    pub async fn email_get(&self, ids: &[Id], include_body: bool) -> MailResult<Vec<Email>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let account_id = self.account_id().await?;
        let mut properties = vec![
            "id",
            "blobId",
            "threadId",
            "mailboxIds",
            "keywords",
            "size",
            "receivedAt",
            "sentAt",
            "subject",
            "from",
            "to",
            "cc",
            "bcc",
            "replyTo",
            "messageId",
            "inReplyTo",
            "references",
            "preview",
            "attachments",
        ];
        if include_body {
            properties.extend(["textBody", "htmlBody", "bodyValues", "headers"]);
        }
        let mut args = json!({
            "accountId": account_id,
            "ids": ids,
            "properties": properties,
        });
        if include_body {
            args["fetchAllBodyValues"] = json!(true);
            args["maxBodyValueBytes"] = json!(1_000_000);
        }
        let result = self.call("Email/get", args).await?;
        let state = result
            .get("state")
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut emails: Vec<Email> = result
            .get("list")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| parse_email(v, &account_id).ok())
                    .collect()
            })
            .unwrap_or_default();
        if let Some(s) = state {
            for email in &mut emails {
                email.state = Some(s.clone());
            }
        }
        Ok(emails)
    }

    pub async fn email_changes(&self, since_state: &str, max_changes: u32) -> MailResult<Changes> {
        let account_id = self.account_id().await?;
        let args = json!({
            "accountId": account_id,
            "sinceState": since_state,
            "maxChanges": max_changes,
        });
        let result = self.call("Email/changes", args).await?;
        Ok(parse_changes(&result))
    }

    pub async fn email_set(
        &self,
        create: BTreeMap<String, Value>,
        update: BTreeMap<String, Value>,
        destroy: &[Id],
    ) -> MailResult<SetOutcome> {
        let account_id = self.account_id().await?;
        let args = json!({
            "accountId": account_id,
            "create": create,
            "update": update,
            "destroy": destroy,
        });
        let result = self.call("Email/set", args).await?;
        parse_set(&result)
    }

    /// 关键词双向同步：JMAP patch 语法 `keywords/<keyword>`（J-007）。
    pub async fn set_keywords(
        &self,
        ids: &[Id],
        flag: &Flag,
        enabled: bool,
    ) -> MailResult<SetOutcome> {
        let key = format!("keywords/{}", flag.as_jmap_keyword());
        let value = if enabled {
            Value::Bool(true)
        } else {
            Value::Null
        };
        let update = ids
            .iter()
            .map(|id| {
                let mut patch = Map::new();
                patch.insert(key.clone(), value.clone());
                (id.clone(), Value::Object(patch))
            })
            .collect();
        self.email_set(BTreeMap::new(), update, &[]).await
    }

    pub async fn move_emails(&self, ids: &[Id], from: &str, to: &str) -> MailResult<SetOutcome> {
        let update = ids
            .iter()
            .map(|id| {
                let mut patch = Map::new();
                patch.insert(format!("mailboxIds/{from}"), Value::Null);
                patch.insert(format!("mailboxIds/{to}"), Value::Bool(true));
                (id.clone(), Value::Object(patch))
            })
            .collect();
        self.email_set(BTreeMap::new(), update, &[]).await
    }

    pub async fn copy_emails(&self, ids: &[Id], to: &str) -> MailResult<SetOutcome> {
        let update = ids
            .iter()
            .map(|id| {
                let mut patch = Map::new();
                patch.insert(format!("mailboxIds/{to}"), Value::Bool(true));
                (id.clone(), Value::Object(patch))
            })
            .collect();
        self.email_set(BTreeMap::new(), update, &[]).await
    }

    /// 有回收站则「移动」进回收站（整体替换 `mailboxIds`），无则直接销毁。
    ///
    /// 这里**必须整体替换** `mailboxIds`，不能像 `copy_emails` 那样只加一条。
    /// 只加不删 = 邮件同时属于原文件夹和回收站，用户看到的是「点了删除，东西还在原处」——
    /// 真机上就是这么表现的：回读 `mailboxIds` 是 `{草稿箱: true, 回收站: true}`。
    /// IMAP 侧的 `delete_emails` 曾有过一模一样的缺陷（往所在文件夹自己 MOVE）。
    ///
    /// 整体赋值天然幂等：邮件已经在回收站时再删一次，结果仍是「只属于回收站」。
    pub async fn delete_emails(
        &self,
        ids: &[Id],
        trash_id: Option<&str>,
    ) -> MailResult<SetOutcome> {
        match trash_id {
            Some(trash) => {
                let update = ids
                    .iter()
                    .map(|id| {
                        let mut patch = Map::new();
                        patch.insert("mailboxIds".to_string(), json!({ trash: true }));
                        (id.clone(), Value::Object(patch))
                    })
                    .collect();
                self.email_set(BTreeMap::new(), update, &[]).await
            }
            None => self.email_set(BTreeMap::new(), BTreeMap::new(), ids).await,
        }
    }

    // ---- Blob (J-009) ----

    pub async fn upload_blob(&self, data: &[u8], content_type: &str) -> MailResult<BlobRef> {
        let session = self.session().await?;
        if !session.provider_capabilities().blob_upload {
            return Err(MailError::Unsupported("服务端未提供 Blob 上传能力".into()));
        }
        let account_id = session.primary_mail_account()?;
        let url = session.upload_url_for(&account_id);
        let result = self
            .transport
            .upload_bytes(&url, Some(&self.auth_header()), content_type, data)
            .await?;
        Ok(BlobRef {
            id: result
                .get("blobId")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            size: result
                .get("size")
                .and_then(Value::as_u64)
                .unwrap_or(data.len() as u64),
            content_type: result
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or(content_type)
                .to_string(),
        })
    }

    pub async fn download_blob(
        &self,
        blob_id: &str,
        name: &str,
        content_type: &str,
    ) -> MailResult<Vec<u8>> {
        let session = self.session().await?;
        let account_id = session.primary_mail_account()?;
        let url = session.download_url_for(&account_id, blob_id, name, content_type);
        self.transport
            .download_bytes(&url, Some(&self.auth_header()))
            .await
    }

    // ---- EventSource 推送（RFC 8620 §7，J-012） ----

    /// 打开一条 EventSource 推送长连接。
    ///
    /// 只负责「建一条连接」；断线重连与缺口对齐由 `PushSession` 负责。
    pub async fn open_push(&self, cfg: &PushConfig) -> MailResult<PushReader> {
        let session = self.session().await?;
        if !session.provider_capabilities().push {
            return Err(MailError::Unsupported(
                "服务端未提供 eventSourceUrl，无法订阅推送".into(),
            ));
        }
        let url = session.event_source_endpoint(&cfg.types, cfg.close_after.as_str(), cfg.ping)?;
        let stream = self
            .transport
            .open_event_stream(&url, Some(&self.auth_header()), None)
            .await?;
        Ok(PushReader::new(stream, cfg.idle_timeout))
    }

    // ---- WebSocket 推送（RFC 8887，J-010） ----

    /// 打开一条 WebSocket 推送长连接。
    ///
    /// 只负责「建一条通道」；注册帧（`WebSocketPushEnable`）、断线重连与
    /// 缺口对齐由 `ws::WsPushSession` 负责。认证走握手 HTTP 头（RFC 8887 §4.1）。
    pub async fn connect_ws_push(&self, url: &str) -> MailResult<crate::ws::WsStream> {
        self.transport
            .connect_ws(url, Some(&self.auth_header()))
            .await
    }

    // ---- Identity（RFC 8621 §6，发送身份） ----

    /// 拉取账号下的全部发送身份。
    pub async fn identity_get(&self) -> MailResult<Vec<Identity>> {
        let session = self.session().await?;
        if !session.has_capability(SUBMISSION_CAPABILITY) {
            return Err(MailError::Unsupported(
                "服务端未提供 urn:ietf:params:jmap:submission，无法查询发送身份".into(),
            ));
        }
        let account_id = self.account_id().await?;
        let result = self
            .call("Identity/get", json!({ "accountId": account_id }))
            .await?;
        let list = result
            .get("list")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or(&[]);
        Ok(parse_identities(list))
    }

    /// 把「邮箱地址」解析成服务端认可的 `identityId`。
    ///
    /// 优先精确匹配 `email`（大小写不敏感）；匹配不上时，只有**唯一一个**身份才敢直接用，
    /// 多个身份一律报错——猜错身份的后果是「以自己的名义替别人发信」，不能靠运气。
    pub async fn resolve_identity_id(&self, email: &str) -> MailResult<String> {
        let list = self.identity_get().await?;
        if list.is_empty() {
            return Err(MailError::Protocol(
                "服务端 Identity/get 返回空列表：账号没有可用发送身份".into(),
            ));
        }
        if let Some(found) = list
            .iter()
            .find(|i| !i.email.is_empty() && i.email.eq_ignore_ascii_case(email))
        {
            return Ok(found.id.clone());
        }
        if list.len() == 1 {
            return Ok(list[0].id.clone());
        }
        let available: Vec<String> = list
            .iter()
            .map(|i| format!("{}<{}>", i.id, i.email))
            .collect();
        Err(MailError::InvalidInput(format!(
            "服务端有 {} 个发送身份但无一个匹配 {email}（可用: {}），请显式指定 identityId",
            list.len(),
            available.join(", ")
        )))
    }

    // ---- EmailSubmission (J-006) ----

    /// 保存草稿并发送：单个批量请求完成 create + submission + onSuccessUpdateEmail，
    /// 由服务端保证发送成功后自动移入「已发送」并清理 $draft。
    pub async fn submit_email(
        &self,
        msg: &OutgoingMessage,
        identity_id: &str,
    ) -> MailResult<SentReceipt> {
        msg.validate().map_err(MailError::InvalidInput)?;
        let mailboxes = self.mailbox_get(None).await?.mailboxes;
        let draft_id = find_role(&mailboxes, MailboxRole::Drafts)
            .ok_or_else(|| MailError::Unsupported("服务端缺少草稿文件夹".into()))?;
        let sent_id = find_role(&mailboxes, MailboxRole::Sent)
            .ok_or_else(|| MailError::Unsupported("服务端缺少已发送文件夹".into()))?;

        let mut blobs = Vec::new();
        for attachment in &msg.attachments {
            let uploaded = self
                .upload_blob(&attachment.data, &attachment.content_type)
                .await?;
            blobs.push(EmailBlob {
                blob_id: uploaded.id,
                name: attachment.name.clone(),
                content_type: uploaded.content_type,
                size: uploaded.size,
            });
        }

        let from = &msg.from[0];
        let message_id = format!("<{}@{}>", Uuid::new_v4(), from.domain());
        let draft = build_email_create(msg, &draft_id, &message_id, &blobs);
        let mut create = BTreeMap::new();
        create.insert("draft".to_string(), draft);

        let rcpt_to: Vec<Value> = msg
            .recipients()
            .iter()
            .map(|a| json!({ "email": a.email }))
            .collect();
        let mut on_success = Map::new();
        on_success.insert(format!("mailboxIds/{draft_id}"), Value::Null);
        on_success.insert(format!("mailboxIds/{sent_id}"), Value::Bool(true));
        on_success.insert("keywords/$draft".into(), Value::Null);
        on_success.insert("keywords/$seen".into(), Value::Bool(true));

        let account_id = self.account_id().await?;
        let results = self
            .call_batch(vec![
                (
                    "Email/set",
                    json!({
                        "accountId": account_id,
                        "create": create,
                    }),
                ),
                (
                    "EmailSubmission/set",
                    json!({
                        "accountId": account_id,
                        "create": {
                            "sub": {
                                "identityId": identity_id,
                                "emailId": "#draft",
                                "envelope": {
                                    "mailFrom": { "email": from.email },
                                    "rcptTo": rcpt_to,
                                }
                            }
                        },
                        "onSuccessUpdateEmail": { "#sub": on_success },
                    }),
                ),
            ])
            .await?;

        let email_id = results
            .first()
            .map(|(_, v)| parse_set(v))
            .transpose()?
            .and_then(|set| set.created.get("draft").cloned())
            .and_then(|d| d.get("id").and_then(Value::as_str).map(str::to_string))
            .unwrap_or_default();
        if email_id.is_empty() {
            return Err(MailError::Protocol(
                "Email/set 未返回草稿 id（create 被服务端忽略）".into(),
            ));
        }
        // 部分失败（notCreated/notUpdated）由 parse_set 提升为可读错误：
        // 身份 id 不合法时服务端给的是 invalidProperties[identityId]，只报数量
        // 会让人误以为网络或权限问题，实际是身份解析选错了。
        let submission = results
            .get(1)
            .map(|(_, v)| parse_set(v))
            .transpose()?
            .ok_or_else(|| MailError::Protocol("JMAP 响应缺少 EmailSubmission/set 结果".into()))?;
        let (server_id, sent_at) = submission
            .created
            .get("sub")
            .map(|s| {
                (
                    s.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    s.get("sendAt")
                        .and_then(Value::as_str)
                        .and_then(parse_rfc3339),
                )
            })
            .unwrap_or_default();

        Ok(SentReceipt {
            message_id,
            server_id: Some(if server_id.is_empty() {
                email_id
            } else {
                server_id
            }),
            sent_mailbox_id: Some(sent_id),
            sent_at: sent_at.unwrap_or_else(Utc::now),
        })
    }
}

fn parse_rfc3339(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn find_role(mailboxes: &[Mailbox], role: MailboxRole) -> Option<Id> {
    mailboxes
        .iter()
        .find(|m| m.role == role)
        .map(|m| m.id.clone())
}

fn string_array(v: Option<&Value>) -> Vec<Id> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// JMAP 方法级 error -> 统一错误（J-011）。状态类错误映射到 Conflict，
/// 使 `MailError::requires_full_resync()` 能触发全量校准（A-005）。
pub fn map_jmap_error(v: &Value) -> MailError {
    let kind = v.get("type").and_then(Value::as_str).unwrap_or("unknown");
    let description = v
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or(kind)
        .to_string();
    match kind {
        "unauthorized" | "forbidden" | "invalidClient" => MailError::Authentication(description),
        "accountNotFound" | "notFound" => MailError::NotFound(description),
        "stateMismatch"
        | "cannotCalculateChanges"
        | "invalidResultReference"
        | "tooManyChanges" => MailError::Conflict(description),
        "overQuota" | "quotaExceeded" | "tooLarge" | "maxSizeRequest" => {
            MailError::Quota(description)
        }
        "rateLimit" => MailError::RateLimited(description),
        "serverFail" | "serverUnavailable" | "serverPartialFail" => MailError::Server {
            status: 503,
            message: description,
        },
        "unknownMethod" | "unknownCapability" | "unsupported" => {
            MailError::Unsupported(description)
        }
        "invalidArguments" | "invalidPatch" | "forbiddenToCreate" => {
            MailError::InvalidInput(description)
        }
        _ => MailError::Protocol(format!("{kind}: {description}")),
    }
}

/// 构造地址列表 JSON 的便捷入口（供上层组装自定义 Email/set 用）。
pub fn address_list_to_json(list: &[Address]) -> Value {
    Value::Array(
        list.iter()
            .map(|a| json!({ "name": a.name, "email": a.email }))
            .collect(),
    )
}
