//! IMAP + SMTP Provider 适配层（S3-03）。
//!
//! 把 S2 的命令级客户端包装成统一 `MailProvider`：
//! * 连接是独占的（IMAP 单连接有状态），因此用 `tokio::sync::Mutex` 串行化；
//! * 本地邮件主键采用 `{account}/{mailbox}/{uid}`，可从 id 反解出 UID；
//! * 凭证只驻留内存，永不落盘（ACC-005）。

use crate::client::{local_email_id, ImapClient, SelectedState};
use crate::cte::{self, TransferEncoding};
use crate::smtp::{self, SmtpClient, SmtpIo};
use crate::sync::{advance_state, is_newer_than_cursor, plan_sync, ServerState, SyncState};
use async_trait::async_trait;
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{
    Email, Flag, Id, Mailbox, MailboxRole, OutgoingMessage, ProtocolKind, SecurityMode,
};
use crossmail_core::provider::{
    AccountInfo, BlobRef, CancellationToken, IdRemap, MailProvider, MailboxDraft, Progress,
    ProgressReporter, ProviderCapabilities, SentReceipt, SyncPage, SyncPhase, SyncRequest,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::sync::Mutex;

/// SMTP 通道建立方式：真实实现走 TCP+TLS，测试注入脚本化通道。
#[async_trait]
pub trait SmtpConnector: Send + Sync {
    async fn connect(&self) -> MailResult<Box<dyn SmtpIo>>;
}

/// 生产用连接器：仅允许 465 / 587（S2-01 端口白名单，禁止明文）。
pub struct TcpSmtpConnector {
    pub host: String,
    pub security: SecurityMode,
    pub timeout: Duration,
}

impl TcpSmtpConnector {
    pub fn new(host: impl Into<String>, security: SecurityMode) -> Self {
        Self {
            host: host.into(),
            security,
            timeout: Duration::from_secs(30),
        }
    }
}

#[async_trait]
impl SmtpConnector for TcpSmtpConnector {
    async fn connect(&self) -> MailResult<Box<dyn SmtpIo>> {
        let io = smtp::connect(&self.host, self.security, self.timeout).await?;
        Ok(Box::new(io))
    }
}

/// 发信所需的运行时凭证与通道（仅内存）。
pub struct SmtpAuth {
    pub connector: Box<dyn SmtpConnector>,
    pub username: String,
    pub secret: String,
}

pub struct ImapProviderConfig {
    pub account_id: Id,
    pub email_address: String,
    /// IMAP 登录名（通常与邮箱地址一致）
    pub username: String,
    /// 仅内存持有
    pub password: String,
}

impl ImapProviderConfig {
    pub fn new(
        account_id: impl Into<Id>,
        email_address: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Self {
        Self {
            account_id: account_id.into(),
            email_address: email_address.into(),
            username: username.into(),
            password: password.into(),
        }
    }
}

/// 认证方式（`ImapProvider::connect` 的降级链用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthMethod {
    Login,
    AuthenticatePlain,
}

impl AuthMethod {
    fn name(self) -> &'static str {
        match self {
            AuthMethod::Login => "LOGIN",
            AuthMethod::AuthenticatePlain => "AUTHENTICATE PLAIN",
        }
    }

    async fn run(self, client: &mut ImapClient, username: &str, password: &str) -> MailResult<()> {
        match self {
            AuthMethod::Login => client.login(username, password).await,
            AuthMethod::AuthenticatePlain => client.authenticate_plain(username, password).await,
        }
    }
}

/// 正文部件描述符：解析 `X-Body-Part` / `X-Body-Html-Part` 这类伪头。
///
/// 形如 `1;size=1234;cte=base64;charset=utf-8`。字段都按 key 取值而不是按位置，
/// 这样以后加字段不会把老逻辑搞乱。
#[derive(Debug, Clone, PartialEq, Eq)]
struct BodyDescriptor {
    part_id: String,
    encoding: TransferEncoding,
    charset: Option<String>,
}

impl BodyDescriptor {
    fn parse(raw: &str) -> Self {
        let mut part_id = String::new();
        let mut encoding = TransferEncoding::SevenBit;
        let mut charset = None;
        for (idx, field) in raw.split(';').enumerate() {
            let field = field.trim();
            if idx == 0 {
                // 老格式就是裸 part-id，必须占住第一段
                part_id = field.to_string();
                continue;
            }
            match field.split_once('=') {
                Some(("cte", v)) => encoding = TransferEncoding::parse(v),
                Some(("charset", v)) if !v.trim().is_empty() => {
                    charset = Some(v.trim().to_string())
                }
                _ => {}
            }
        }
        if part_id.is_empty() {
            part_id = "1".to_string();
        }
        Self {
            part_id,
            encoding,
            charset,
        }
    }
}

/// IMAP 增量游标在 `SyncRequest::since_state` / `SyncPage::new_state` 中的序列化形式。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct ImapCursor {
    uid_validity: Option<u32>,
    last_uid: Option<u32>,
    highest_modseq: Option<u64>,
}

impl ImapCursor {
    fn decode(raw: Option<&str>) -> Self {
        raw.and_then(|s| serde_json::from_str::<ImapCursor>(s).ok())
            .unwrap_or_default()
    }

    fn encode(c: &SyncState) -> String {
        serde_json::to_string(&ImapCursor {
            uid_validity: c.uid_validity,
            last_uid: c.last_uid,
            highest_modseq: c.highest_modseq,
        })
        .unwrap_or_default()
    }

    fn to_state(&self) -> SyncState {
        SyncState {
            uid_validity: self.uid_validity,
            last_uid: self.last_uid,
            highest_modseq: self.highest_modseq,
        }
    }
}

pub struct ImapProvider {
    client: Mutex<ImapClient>,
    config: ImapProviderConfig,
    caps: ProviderCapabilities,
    smtp: Option<SmtpAuth>,
    /// 建连时用过的端点；IDLE 监视要自己开一条独立连接（IDLE 会独占会话），
    /// 没有它就无从重连。用 `new()` 手工构造的会话没有端点，`watch()` 会明确报错。
    endpoint: Option<crate::transport::Endpoint>,
}

impl ImapProvider {
    /// `client` 应当是已建立 TLS 且已登录的会话（连接由上层按 I-001 决定）。
    pub fn new(client: ImapClient, config: ImapProviderConfig) -> Self {
        let caps = client.provider_capabilities();
        Self {
            client: Mutex::new(client),
            config,
            caps,
            smtp: None,
            endpoint: None,
        }
    }

    pub fn with_smtp(mut self, smtp: SmtpAuth) -> Self {
        self.smtp = Some(smtp);
        self
    }

    /// 记录建连端点，供 [`ImapProvider::watch`] 开独立 IDLE 连接。
    pub fn with_endpoint(mut self, ep: crate::transport::Endpoint) -> Self {
        self.endpoint = Some(ep);
        self
    }

    /// 建一条独立连接做 IDLE 推送（I-008）。
    ///
    /// 为什么必须另开连接：IDLE 期间会话被占死，除了 `DONE` 之外的命令都会被服务端
    /// **静默丢弃**，所以这条会话不能再承载任何同步请求。原会话继续用于收发，
    /// 两条连接各司其职。
    pub async fn watch(
        &self,
        cfg: crate::idle::IdleConfig,
        cancel: CancellationToken,
    ) -> MailResult<crate::idle::ImapWatcher> {
        let ep = self.endpoint.as_ref().ok_or_else(|| {
            MailError::Unsupported(
                "该会话未记录端点，无法建立独立 IDLE 连接（请用 ImapProvider::connect 建立会话）"
                    .into(),
            )
        })?;
        crate::idle::ImapWatcher::subscribe(
            std::sync::Arc::new(crate::idle::TlsIdleTransport::new(ep.clone())),
            &self.config.username,
            &self.config.password,
            self.config.account_id.clone(),
            cfg,
            cancel,
        )
        .await
    }

    /// 真实服务器入口：建 TLS → 读问候 → 认证 → 返回可用 provider。
    ///
    /// 认证方式按服务端能力择优，而不是硬编码 LOGIN：
    /// * 服务端声明 `SASL-IR` + `AUTH=PLAIN` ⇒ 用 `AUTHENTICATE PLAIN <ir>`，
    ///   密码不落在命令行明文里（多一层保护，也兼容禁用 LOGIN 的服务器）；
    /// * 否则用 `LOGIN`。
    ///
    /// 首选方式失败后会**再试另一种**，两条路都失败才报错，并把两次原因一起返回 ——
    /// 只报第一次失败会把「服务器禁 LOGIN 但支持 PLAIN」这类情况误判成密码错误。
    pub async fn connect(
        ep: &crate::transport::Endpoint,
        username: &str,
        password: &str,
        config: ImapProviderConfig,
    ) -> MailResult<Self> {
        let io = crate::transport::connect_tls_endpoint(ep).await?;
        let mut client = ImapClient::new(Box::new(io));
        client.read_greeting().await?;
        // 有的服务器问候语不带 CAPABILITY（RFC 3501 允许），此时必须主动查
        if client.capabilities().is_empty() {
            client.fetch_capabilities().await?;
        }
        Self::authenticate(&mut client, username, password).await?;
        // 认证后必须重新取能力：真机上登录前只有 9 项（够认证用），登录后才是完整 52 项。
        // 光靠 LOGIN 响应里的 `[CAPABILITY ...]` 不够保险——RFC 只要求「能力变化时」才带，
        // 不带的服务器同样合规。一次额外往返换确定性，在建连阶段这成本可以忽略。
        client.fetch_capabilities().await?;
        Ok(Self::new(client, config).with_endpoint(ep.clone()))
    }

    /// 认证降级链：优先 `AUTHENTICATE PLAIN <ir>`，失败再试 `LOGIN`（或反之）。
    ///
    /// 公开为 `pub(crate)` 是因为 IDLE 监视需要独立连接（IDLE 会独占会话），
    /// 那条连接必须走完全相同的认证与降级逻辑，否则两条链路的行为会分叉。
    pub(crate) async fn authenticate_client(
        client: &mut ImapClient,
        username: &str,
        password: &str,
    ) -> MailResult<()> {
        Self::authenticate(client, username, password).await
    }

    async fn authenticate(
        client: &mut ImapClient,
        username: &str,
        password: &str,
    ) -> MailResult<()> {
        let caps = client.capabilities().clone();
        let plain_ir = caps.has("SASL-IR")
            && caps
                .auth_mechanisms()
                .iter()
                .any(|m| m.eq_ignore_ascii_case("PLAIN"));

        let (first, second): (AuthMethod, AuthMethod) = if plain_ir {
            (AuthMethod::AuthenticatePlain, AuthMethod::Login)
        } else {
            (AuthMethod::Login, AuthMethod::AuthenticatePlain)
        };
        let first_err = match first.run(client, username, password).await {
            Ok(()) => return Ok(()),
            Err(err) if !matches!(err, MailError::Authentication(_)) => {
                // 网络/协议类错误换认证方式也没用，直接抛，避免把连通性问题误报成密码错
                return Err(err);
            }
            Err(err) => err,
        };
        match second.run(client, username, password).await {
            Ok(()) => Ok(()),
            Err(second_err) => Err(MailError::Authentication(format!(
                "{} 失败（{}）；{} 失败（{}）",
                first.name(),
                first_err,
                second.name(),
                second_err
            ))),
        }
    }

    pub fn config(&self) -> &ImapProviderConfig {
        &self.config
    }

    /// 从本地主键反解文件夹与 UID（S3-03 主键约定）。
    pub fn parse_email_id(&self, id: &str) -> MailResult<(String, u32)> {
        let prefix = format!("{}/", self.config.account_id);
        let rest = id
            .strip_prefix(&prefix)
            .ok_or_else(|| MailError::InvalidInput(format!("邮件 id 与账号不匹配: {id}")))?;
        let (mailbox, uid) = rest
            .rsplit_once('/')
            .ok_or_else(|| MailError::InvalidInput(format!("邮件 id 格式非法: {id}")))?;
        let uid: u32 = uid
            .parse()
            .map_err(|_| MailError::InvalidInput(format!("邮件 id 中 UID 非法: {id}")))?;
        Ok((mailbox.to_string(), uid))
    }

    /// 按文件夹归并 UID，避免为每封邮件重复 SELECT。
    fn group_uids(&self, ids: &[Id]) -> MailResult<BTreeMap<String, Vec<u32>>> {
        let mut grouped: BTreeMap<String, Vec<u32>> = BTreeMap::new();
        for id in ids {
            let (mailbox, uid) = self.parse_email_id(id)?;
            grouped.entry(mailbox).or_default().push(uid);
        }
        Ok(grouped)
    }

    async fn folder_by_role(
        client: &mut ImapClient,
        account: &str,
        role: MailboxRole,
    ) -> MailResult<String> {
        client
            .list_folders(account)
            .await?
            .into_iter()
            .find(|m| m.role == role)
            .map(|m| m.id)
            .ok_or_else(|| MailError::Unsupported(format!("服务端缺少 {role:?} 文件夹")))
    }

    /// 把刚发出的报文 APPEND 进服务端「已发送」，返回（文件夹, UID）。
    ///
    /// 为什么必须做：IMAP 的 SMTP 提交与邮箱状态是**两条独立链路**，服务器不会替你
    /// 归档已发送。少了这一步，手机端和其他客户端里看不到自己发过的邮件，用户会
    /// 以为自己没发出去。
    ///
    /// `\Seen` 是刻意加的：自己发的邮件显示成未读没有任何意义，不标已读会让每封新
    /// 发邮件在已发送里冒出一个小红点。
    ///
    /// 服务端没有 UIDPLUS 时拿不到 UID（`APPEND` 只回 OK），此时返回 `(None, None)`：
    /// 邮件确实进箱了，但本地无法为它建出同步用的主键，只能退回本地记账——
    /// 代价是下次同步会在已发送里多插一行（服务端那一封）。Gmail / Stalwart /
    /// 主流 IMAP 都支持 UIDPLUS，这条降级路径主要防老服务端。
    async fn append_to_sent(&self, raw: &[u8]) -> MailResult<(Option<Id>, Option<Id>)> {
        let mut client = self.client.lock().await;
        let folder =
            Self::folder_by_role(&mut client, &self.config.account_id, MailboxRole::Sent).await?;
        match client.append_message(&folder, &[Flag::Seen], raw).await? {
            Some(uid) => Ok((Some(folder), Some(uid.to_string()))),
            None => Ok((None, None)),
        }
    }

    /// 服务器端搜索（v0.7 新增，纯增量）：遍历给定文件夹 `SELECT` + `UID SEARCH`，
    /// 命中的 UID 拉取元数据（信封/标志/结构，不取正文）。
    ///
    /// 关键词在主题/发件人/收件人三字段上做 OR，与 JMAP 版 `server_search` 的
    /// filter 语义对齐。SELECT 失败的文件夹（如被其他客户端移除）静默跳过，
    /// 不让单个坏文件夹毁掉整个搜索。
    ///
    /// 返回的 Email 主键沿用本地库约定 `{account_id}/{mailbox}/{uid}`，可直接
    /// 喂给 `fetch_email` / `set_flags` 做后续阅读与写操作，与 JMAP 搜索结果
    /// 在 UI 层的处理路径完全一致。
    ///
    /// `on_folder`（v0.9 新增）：可选进度回调，每开始搜一个文件夹前触发
    /// `(文件夹名, 已累计命中数)`；UI 层据此显示逐文件夹进度。
    pub async fn search_emails(
        &self,
        mailboxes: &[String],
        keyword: &str,
        limit: usize,
        on_folder: Option<&dyn Fn(&str, usize)>,
    ) -> MailResult<Vec<Email>> {
        let kw = crate::client::quote(keyword);
        let criteria = format!("OR OR SUBJECT {kw} FROM {kw} TO {kw}");
        let mut client = self.client.lock().await;
        let mut out = Vec::new();
        for mailbox in mailboxes {
            if out.len() >= limit {
                break;
            }
            if let Some(report) = on_folder {
                report(mailbox, out.len());
            }
            if client.select(mailbox).await.is_err() {
                continue;
            }
            let uids = client.uid_search(&criteria).await?;
            if uids.is_empty() {
                continue;
            }
            let set = crate::uid::UidRange::format_set(
                &uids
                    .iter()
                    .map(|u| crate::uid::UidRange::single(*u))
                    .collect::<Vec<_>>(),
            );
            let mut emails = client
                .fetch_metadata(&set, &self.config.account_id, mailbox)
                .await?;
            // fetch_metadata 的占位主键是 "uid:{n}"，归一成本地库主键约定，
            // 让搜索结果可以直接参与阅读/写标志链路
            for e in &mut emails {
                e.id = crate::client::local_email_id(
                    &self.config.account_id,
                    mailbox,
                    e.uid.unwrap_or(0),
                );
            }
            out.extend(emails);
        }
        out.truncate(limit);
        Ok(out)
    }
}

fn selected_to_server(st: &SelectedState) -> ServerState {
    ServerState {
        uid_validity: st.uid_validity.unwrap_or(0),
        uid_next: st.uid_next.unwrap_or(1),
        highest_modseq: st.highest_modseq,
        exists: st.exists,
    }
}

fn message_id_of(raw: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(raw);
    text.lines()
        .find_map(|line| line.strip_prefix("Message-ID: "))
        .map(|v| v.trim().to_string())
}

#[async_trait]
impl MailProvider for ImapProvider {
    fn protocol(&self) -> ProtocolKind {
        ProtocolKind::Imap
    }

    fn capabilities(&self) -> ProviderCapabilities {
        self.caps.clone()
    }

    /// IMAP 会话在构造时已完成登录，这里只做本地状态回报（不额外产生网络请求）。
    async fn verify_credentials(&self) -> MailResult<AccountInfo> {
        Ok(AccountInfo {
            account_id: self.config.account_id.clone(),
            email_address: self.config.email_address.clone(),
            capabilities: self.caps.clone(),
        })
    }

    async fn list_mailboxes(&self) -> MailResult<Vec<Mailbox>> {
        let mut client = self.client.lock().await;
        client.list_folders(&self.config.account_id).await
    }

    /// IMAP 通过 CREATE 建文件夹；建完回读 LIST 拿到真实属性。
    async fn create_mailbox(&self, draft: MailboxDraft) -> MailResult<Mailbox> {
        let mut client = self.client.lock().await;
        let name = match &draft.parent_id {
            Some(parent) => format!("{parent}/{}", draft.name),
            None => draft.name.clone(),
        };
        // ALREADYEXISTS 视为成功：调用方（如 ensure_system_mailboxes）语义是
        // 「确保存在」，不是「必须由我创建」。真实服务器上重试/并发都会撞这条，
        // 当成错误会让整个账号初始化直接失败。
        match client
            .cmd(&format!("CREATE {}", crate::client::quote(&name)))
            .await
        {
            Ok(_) => {}
            Err(MailError::Protocol(msg)) if msg.contains("ALREADYEXISTS") => {}
            Err(err) => return Err(err),
        }
        let mut folders = client.list_folders(&self.config.account_id).await?;
        let found = folders.drain(..).find(|m| m.name == name);
        Ok(found.unwrap_or_else(|| {
            let mut mb = Mailbox::new(name.clone(), self.config.account_id.clone(), name);
            mb.role = draft.role;
            mb
        }))
    }

    async fn rename_mailbox(&self, id: &Id, name: &str) -> MailResult<()> {
        let mut client = self.client.lock().await;
        client
            .cmd(&format!(
                "RENAME {} {}",
                crate::client::quote(id),
                crate::client::quote(name)
            ))
            .await
            .map(|_| ())
    }

    async fn delete_mailbox(&self, id: &Id) -> MailResult<()> {
        let mut client = self.client.lock().await;
        client
            .cmd(&format!("DELETE {}", crate::client::quote(id)))
            .await
            .map(|_| ())
    }

    async fn sync_emails(
        &self,
        req: SyncRequest,
        cancel: CancellationToken,
        reporter: Option<&dyn ProgressReporter>,
    ) -> MailResult<SyncPage> {
        let mailbox = req
            .mailbox_id
            .clone()
            .ok_or_else(|| MailError::InvalidInput("IMAP 增量同步必须指定文件夹".into()))?;
        cancel.check()?;

        let mut client = self.client.lock().await;
        let st = client.select(&mailbox).await?;
        let server = selected_to_server(&st);
        let local_cursor = ImapCursor::decode(req.since_state.as_deref());
        let local = local_cursor.to_state();
        let plan = plan_sync(client.capabilities(), &server, &local);

        let mut page = SyncPage {
            // UIDVALIDITY 变化 → 引擎必须清空该文件夹缓存做全量校准
            full_resync: plan.reset_cache,
            ..SyncPage::default()
        };

        let mut advanced = local.clone();
        if plan.is_noop() {
            advance_state(&mut advanced, &server, None);
            page.new_state = Some(ImapCursor::encode(&advanced));
            page.new_modseq = advanced.highest_modseq;
            return Ok(page);
        }

        cancel.check()?;
        let clause = plan.fetch_uid_clause();
        let fetched = client
            .fetch_metadata(&clause, &self.config.account_id, &mailbox)
            .await?;

        let mut max_uid = None;
        for mut email in fetched {
            cancel.check()?;
            let uid = match email.uid {
                Some(uid) => uid,
                None => continue,
            };
            // 服务端对 `{last_uid+1}:*` 的归一化会连带最后一封老邮件（反向区间），
            // 这里挡掉，否则每轮增量同步都重复下发同一封。挡掉后 max_uid 为 None，
            // `advance_state` 回落到 `UIDNEXT-1`，下一轮即收敛为 UpToDate。
            if !is_newer_than_cursor(&plan, Some(uid)) {
                continue;
            }
            email.id = local_email_id(&self.config.account_id, &mailbox, uid);
            email.account_id = self.config.account_id.clone();
            if email.mailbox_ids.is_empty() {
                email.mailbox_ids = vec![mailbox.clone()];
            }
            max_uid = Some(max_uid.unwrap_or(uid).max(uid));
            page.emails.push(email);
        }

        advance_state(&mut advanced, &server, max_uid);
        page.new_state = Some(ImapCursor::encode(&advanced));
        page.new_modseq = advanced.highest_modseq;

        if let Some(r) = reporter {
            r.report(Progress {
                phase: SyncPhase::Done,
                processed: page.emails.len() as u64,
                total: page.emails.len() as u64,
            });
        }
        Ok(page)
    }

    async fn fetch_email(&self, id: &Id, include_body: bool) -> MailResult<Email> {
        let (mailbox, uid) = self.parse_email_id(id)?;
        let mut client = self.client.lock().await;
        client.select(&mailbox).await?;
        let mut emails = client
            .fetch_metadata(&uid.to_string(), &self.config.account_id, &mailbox)
            .await?;
        let mut email = emails
            .pop()
            .ok_or_else(|| MailError::NotFound(format!("UID {uid} 不存在")))?;
        email.id = id.clone();
        if include_body {
            let desc = BodyDescriptor::parse(
                email
                    .raw_headers
                    .get("X-Body-Part")
                    .map(String::as_str)
                    .unwrap_or("1"),
            );
            match client.fetch_part(uid, &desc.part_id).await {
                Ok(bytes) => {
                    // BODY[1] 给的是传输编码后的原始字节，必须先解码再当文本用，
                    // 否则 base64 正文会原样显示成一串字母数字。
                    let (text, enc_ok, charset_ok) =
                        cte::decode_to_string(&bytes, desc.encoding, desc.charset.as_deref());
                    // 解码/字符集状况如实记进元数据，界面可以据此给出提示，
                    // 而不是让用户对着一屏乱码猜发生了什么
                    email.raw_headers.insert(
                        "X-Body-Encoding".into(),
                        if enc_ok {
                            desc.encoding.name().to_string()
                        } else {
                            format!("{} (解码失败，已按原文展示)", desc.encoding.name())
                        },
                    );
                    if !charset_ok {
                        email.raw_headers.insert(
                            "X-Body-Charset".into(),
                            format!(
                                "{} (非 UTF-8，暂按 UTF-8 呈现)",
                                desc.charset.as_deref().unwrap_or("unknown")
                            ),
                        );
                    }
                    email.text_body = Some(text);
                    email.refresh_preview();
                }
                Err(MailError::NotFound(_)) => {}
                Err(err) => return Err(err),
            }
        }
        Ok(email)
    }

    async fn set_flags(&self, ids: &[Id], flag: Flag, enabled: bool) -> MailResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let grouped = self.group_uids(ids)?;
        let mut client = self.client.lock().await;
        for (mailbox, uids) in grouped {
            client.select(&mailbox).await?;
            let set = crate::uid::UidRange::format_set(
                &uids
                    .iter()
                    .map(|u| crate::uid::UidRange::single(*u))
                    .collect::<Vec<_>>(),
            );
            if enabled {
                client
                    .store_flags(&set, std::slice::from_ref(&flag), &[])
                    .await?;
            } else {
                client
                    .store_flags(&set, &[], std::slice::from_ref(&flag))
                    .await?;
            }
        }
        Ok(())
    }

    async fn move_emails(&self, ids: &[Id], from: &Id, to: &Id) -> MailResult<Vec<IdRemap>> {
        if ids.is_empty() {
            return Err(MailError::InvalidInput("未选择邮件".into()));
        }
        if from == to {
            return Ok(Vec::new());
        }
        let grouped = self.group_uids(ids)?;
        let mut client = self.client.lock().await;
        let mut remaps = Vec::new();
        for (mailbox, uids) in grouped {
            // 源文件夹**以 `from` 参数为准**，不用 id 里内嵌的那个。
            //
            // 本地主键是 `账号/文件夹/UID`，是「上一次同步时」的位置快照。邮件被移动过一次
            // 之后这个快照就过期了（IMAP 的邮件身份是 (文件夹, UID)，服务器 MOVE 会分配新 UID），
            // 而移动的入参 `from` 恰恰是调用方（业务层/回放层）记录的当前位置，更新更及时。
            //
            // 真服务器上的表现：拿 id 里的旧文件夹当源，就会变成「同箱移动」，
            // Stalwart 直接回 `[CANNOT] Source and destination mailboxes are the same.`，
            // 整条离线操作被判定为协议错误、不再重试——用户看到的是一次静默的移动失败。
            let source = if from.trim().is_empty() {
                mailbox
            } else {
                from.clone()
            };
            client.select(&source).await?;
            let outcome = client.move_messages(&uids, to).await?;
            // 服务器换了 UID：把「旧 UID」翻译回旧本地主键，「新 UID」拼成新本地主键
            for (old_uid, new_uid) in outcome.copied {
                remaps.push(IdRemap::new(
                    local_email_id(&self.config.account_id, &source, old_uid),
                    local_email_id(&self.config.account_id, to, new_uid),
                ));
            }
        }
        Ok(remaps)
    }

    async fn copy_emails(&self, ids: &[Id], to: &Id) -> MailResult<()> {
        if ids.is_empty() {
            return Err(MailError::InvalidInput("未选择邮件".into()));
        }
        let grouped = self.group_uids(ids)?;
        let mut client = self.client.lock().await;
        let mut missing: Vec<String> = Vec::new();
        let mut copied = 0usize;
        for (mailbox, uids) in grouped {
            client.select(&mailbox).await?;
            // 先确认 UID 真在这个文件夹里。真机对不存在的 UID 回 `OK No messages were copied.`，
            // 不校验就会**静默空转**——报告一片绿，目标文件夹里什么都没有。
            let present = client.existing_uids(&uids).await?;
            missing.extend(
                uids.iter()
                    .filter(|u| !present.contains(u))
                    .map(|u| format!("{mailbox}/{u}")),
            );
            if present.is_empty() {
                continue;
            }
            client.copy_messages(&present, to).await?;
            copied += present.len();
        }
        if copied == 0 {
            // 复制是有目标状态的操作：一封都没成时不能假装成功。
            return Err(MailError::NotFound(format!(
                "这些邮件已不在其原文件夹（{}），可能是位置快照过期；请重新同步后重试",
                missing.join("、")
            )));
        }
        Ok(())
    }

    async fn delete_emails(&self, ids: &[Id]) -> MailResult<()> {
        if ids.is_empty() {
            return Err(MailError::InvalidInput("未选择邮件".into()));
        }
        let grouped = self.group_uids(ids)?;
        let mut client = self.client.lock().await;
        let trash =
            Self::folder_by_role(&mut client, &self.config.account_id, MailboxRole::Trash).await?;
        let mut missing: Vec<String> = Vec::new();
        let mut touched = 0usize;
        for (mailbox, uids) in grouped {
            client.select(&mailbox).await?;
            // 同 copy：不存在的 UID 上做 STORE / MOVE，服务端回 OK 但什么都没删。
            // 这里若放过去会变成「本地记录删了、服务器还在」——下次同步邮件原地复活，
            // 比直接报错难查得多。
            let present = client.existing_uids(&uids).await?;
            missing.extend(
                uids.iter()
                    .filter(|u| !present.contains(u))
                    .map(|u| format!("{mailbox}/{u}")),
            );
            if present.is_empty() {
                continue;
            }
            if mailbox == trash {
                // 已在回收站：删除语义是「彻底删除」，走 \Deleted + EXPUNGE。
                // 不能复用 delete_messages——那会把邮件 MOVE 到回收站自身（空操作），
                // 表现为「点了删除但邮件还在」，用户会反复点。
                client.expunge_uids(&present).await?;
            } else {
                client.delete_messages(&present, &trash).await?;
            }
            touched += present.len();
        }
        if touched == 0 {
            return Err(MailError::NotFound(format!(
                "这些邮件已不在其原文件夹（{}），可能是位置快照过期；请重新同步后重试",
                missing.join("、")
            )));
        }
        Ok(())
    }

    async fn save_draft(&self, msg: &OutgoingMessage) -> MailResult<Id> {
        msg.validate().map_err(MailError::InvalidInput)?;
        let raw = smtp::build_message(msg)?;
        let mut client = self.client.lock().await;
        let drafts =
            Self::folder_by_role(&mut client, &self.config.account_id, MailboxRole::Drafts).await?;
        let uid = client.append_message(&drafts, &[Flag::Draft], &raw).await?;
        Ok(match uid {
            Some(uid) => local_email_id(&self.config.account_id, &drafts, uid),
            None => format!(
                "{}/{}/draft-{}",
                self.config.account_id,
                drafts,
                uuid::Uuid::new_v4()
            ),
        })
    }

    async fn send_email(&self, msg: OutgoingMessage) -> MailResult<SentReceipt> {
        msg.validate().map_err(MailError::InvalidInput)?;
        let auth = self
            .smtp
            .as_ref()
            .ok_or_else(|| MailError::Unsupported("该账号未配置 SMTP 发信".into()))?;

        let raw = smtp::build_message(&msg)?;
        let message_id = message_id_of(&raw).unwrap_or_default();

        let io = auth.connector.connect().await?;
        let mut client = SmtpClient::new(io);
        client.greet().await?;
        client.authenticate(&auth.username, &auth.secret).await?;
        client.send_raw(&msg, &raw).await?;
        client.quit().await?;

        // QUIT 成功 = 邮件已经递出去了。从这一刻起**任何**后续失败都不能再返回 Err：
        // 上层会把「发送失败」显示给用户，用户重发，对方就收到两封。
        // 所以归档到服务端已发送是尽力而为，失败降级成「只本地记账」。
        let (sent_mailbox_id, server_id) = self.append_to_sent(&raw).await.unwrap_or((None, None));

        Ok(SentReceipt {
            message_id,
            server_id,
            sent_mailbox_id,
            sent_at: chrono::Utc::now(),
        })
    }

    /// IMAP 协议本身不提供 Blob 存储，按契约明确返回不支持。
    async fn upload_blob(&self, _data: &[u8], _content_type: &str) -> MailResult<BlobRef> {
        Err(MailError::Unsupported(
            "IMAP 协议无 Blob 存储，附件请随邮件正文发送".into(),
        ))
    }

    /// IMAP 附件下载（v0.8）：`id` 约定为 `{email_id}#{part_id}`，
    /// 其中 email_id 是本地库主键 `{account}/{mailbox}/{uid}`。
    ///
    /// `BODY[part]` 返回的是传输编码后的原始字节（base64 正文就是一串字母数字），
    /// 必须先取 BODYSTRUCTURE 查到该部件的 CTE，再按 CTE 解码回二进制原样。
    /// 解码失败不报错、原样返回——半途损坏的附件交给用户判断，好过整次下载作废。
    async fn download_blob(&self, id: &str) -> MailResult<Vec<u8>> {
        let (email_id, part) = id.rsplit_once('#').ok_or_else(|| {
            MailError::InvalidInput(format!(
                "IMAP blobId 约定为 `{{email_id}}#{{part_id}}`，收到: {id}"
            ))
        })?;
        let (mailbox, uid) = self.parse_email_id(email_id)?;
        let mut client = self.client.lock().await;
        client.select(&mailbox).await?;
        let bytes = client.fetch_part(uid, part).await?;
        let encoding = client
            .fetch_part_encoding(uid, part)
            .await
            .unwrap_or(crate::cte::TransferEncoding::SevenBit);
        let (data, _ok) = crate::cte::decode(&bytes, encoding);
        Ok(data)
    }
}

#[cfg(test)]
mod body_descriptor_tests {
    use super::*;

    #[test]
    fn parses_full_descriptor() {
        let d = BodyDescriptor::parse("2.1;size=1234;cte=base64;charset=utf-8");
        assert_eq!(d.part_id, "2.1");
        assert_eq!(d.encoding, TransferEncoding::Base64);
        assert_eq!(d.charset.as_deref(), Some("utf-8"));
    }

    #[test]
    fn legacy_descriptor_without_cte_defaults_to_7bit() {
        // 老格式（只有 part;size）必须还能读，否则升级后老缓存全失效
        let d = BodyDescriptor::parse("1;size=100");
        assert_eq!(d.part_id, "1");
        assert_eq!(d.encoding, TransferEncoding::SevenBit);
        assert!(d.charset.is_none());
    }

    #[test]
    fn bare_part_id_is_accepted() {
        let d = BodyDescriptor::parse("1");
        assert_eq!(d.part_id, "1");
        assert_eq!(d.encoding, TransferEncoding::SevenBit);
    }

    #[test]
    fn unknown_fields_do_not_break_parsing() {
        // 以后描述符加字段时，老版本解析不能炸也不能丢 part_id
        let d = BodyDescriptor::parse("3;size=9;cte=quoted-printable;charset=gb2312;future=x");
        assert_eq!(d.part_id, "3");
        assert_eq!(d.encoding, TransferEncoding::QuotedPrintable);
        assert_eq!(d.charset.as_deref(), Some("gb2312"));
    }

    #[test]
    fn empty_descriptor_falls_back_to_part_one() {
        let d = BodyDescriptor::parse("");
        assert_eq!(d.part_id, "1");
    }
}
