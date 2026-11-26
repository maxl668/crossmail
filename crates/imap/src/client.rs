//! IMAP 客户端：命令-响应循环、登录、文件夹、增量 FETCH、标记、MOVE 与降级、APPEND。

use chrono::{DateTime, Utc};
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Email, Flag, Id, Mailbox, MailboxRole};
use crossmail_core::provider::ProviderCapabilities;

use crate::capability::Capabilities;
use crate::response::{self, FetchItem, ListEntry, ResponseCode, Status, TaggedResponse, Untagged};

/// SELECT 之后的会话状态。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SelectedState {
    pub mailbox: Option<String>,
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub highest_modseq: Option<u64>,
    pub exists: u32,
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveStrategy {
    /// RFC 6851 MOVE
    Move,
    /// COPY + STORE \Deleted + `UID EXPUNGE`（需要 UIDPLUS，只删本批）
    CopyStoreExpunge,
    /// COPY + STORE \Deleted，**不 EXPUNGE**（延迟删除）。
    ///
    /// 两种情况走这条路：
    /// - Gmail：`\Deleted` 只等于打标签，显式 EXPUNGE 会被用户当成误删（I-011）；
    /// - 服务器不支持 UIDPLUS：此时唯一可用的 `EXPUNGE` 会连带清掉本箱**其他**
    ///   \Deleted 邮件，属于破坏性副作用，宁可留着 `\Deleted` 让后续客户端清理。
    CopyStore,
}

/// `EXPUNGE` 的实际粒度。
///
/// 不加区分地报"删了"会掩盖副作用：无 UIDPLUS 时 `EXPUNGE` 清的是整箱的
/// `\Deleted`，不只是这一批。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpungeStrategy {
    /// `UID EXPUNGE <set>`：只删本批（UIDPLUS / IMAP4rev2）
    Precise,
    /// `EXPUNGE`：本箱所有 \Deleted 一并清除
    MailboxWide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutcome {
    pub uids: Vec<u32>,
    pub strategy: MoveStrategy,
    /// `COPYUID` 给出的「旧 UID → 新 UID」（UIDPLUS / IMAP4rev2 才有）。
    ///
    /// 服务器 MOVE 会在目标文件夹重新分配 UID，不回传这个映射，上层就只能继续用旧 UID ——
    /// 对旧 UID 的任何后续操作都会被服务端静默忽略。
    pub copied: Vec<(u32, u32)>,
}

/// 从响应里取 `COPYUID`：先看 tagged OK，再看 untagged `* OK`（两种服务器都有）。
fn copy_uid_of(tagged: &TaggedResponse, untagged: &[Untagged]) -> Vec<(u32, u32)> {
    let direct = copy_uid_pairs(tagged.code.as_ref());
    if !direct.is_empty() {
        return direct;
    }
    for u in untagged {
        if let Untagged::Ok(code, _) = u {
            let pairs = copy_uid_pairs(code.as_ref());
            if !pairs.is_empty() {
                return pairs;
            }
        }
    }
    Vec::new()
}

/// 把 `COPYUID <validity> <src-set> <dst-set>` 展开成「旧→新」逐封映射。
///
/// 两个集合按位置一一对应（RFC 4315）：`1:3` ↔ `10:12` 表示 1→10、2→11、3→12。
fn copy_uid_pairs(code: Option<&ResponseCode>) -> Vec<(u32, u32)> {
    let Some(ResponseCode::CopyUid { src, dst, .. }) = code else {
        return Vec::new();
    };
    let expand = |ranges: &[crate::uid::UidRange]| -> Vec<u32> {
        let mut out = Vec::new();
        for r in ranges {
            for uid in r.start..=r.end {
                out.push(uid);
            }
        }
        out
    };
    let (src, dst) = (expand(src), expand(dst));
    if src.len() != dst.len() {
        // 数量对不上宁可不用，也不要错位映射——错位比没有更危险
        return Vec::new();
    }
    src.into_iter().zip(dst).collect()
}

/// IMAP 协议客户端。
pub struct ImapClient {
    io: Box<dyn crate::transport::ImapIo>,
    tag_seq: u32,
    caps: Capabilities,
    state: SelectedState,
}

impl ImapClient {
    pub fn new(io: Box<dyn crate::transport::ImapIo>) -> Self {
        Self {
            io,
            tag_seq: 0,
            caps: Capabilities::default(),
            state: SelectedState::default(),
        }
    }

    pub fn with_capabilities(io: Box<dyn crate::transport::ImapIo>, caps: Capabilities) -> Self {
        let mut c = Self::new(io);
        c.caps = caps;
        c
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    pub fn provider_capabilities(&self) -> ProviderCapabilities {
        self.caps.to_provider_capabilities()
    }

    pub fn state(&self) -> &SelectedState {
        &self.state
    }

    fn next_tag(&mut self) -> String {
        self.tag_seq += 1;
        format!("A{}", self.tag_seq)
    }

    /// 读取服务器问候语并提取能力（`* OK [CAPABILITY ...]` / `* PREAUTH`）。
    pub async fn read_greeting(&mut self) -> MailResult<()> {
        let line = self.io.read_line().await?;
        match response::parse_untagged(&line)? {
            Untagged::Ok(code, text) => {
                if let Some(ResponseCode::Capability(c)) = code {
                    self.caps.extend(&c);
                }
                let upper = text.to_ascii_uppercase();
                if upper.contains("BYE") {
                    return Err(MailError::Network(format!("服务端拒绝连接: {text}")));
                }
                Ok(())
            }
            Untagged::Bye(text) => Err(MailError::Network(format!("服务端拒绝连接: {text}"))),
            _ => Ok(()),
        }
    }

    /// 发送一条命令并收集 untagged 响应，直到匹配 tag 的完成响应。
    pub async fn cmd(&mut self, command: &str) -> MailResult<(TaggedResponse, Vec<Untagged>)> {
        let tag = self.next_tag();
        self.io.write_line(&format!("{tag} {command}")).await?;
        let mut untagged = Vec::new();
        loop {
            let line = self.io.read_line().await?;
            if let Some(t) = response::parse_tagged(&line)? {
                if t.tag != tag {
                    continue;
                }
                if !t.is_ok() {
                    return Err(map_status_error(&t));
                }
                // RFC 3501 §7.1：服务器可以在**任何** OK 响应里携带 `[CAPABILITY ...]`，
                // 登录成功响应是最常见的一处（此时能力集会明显变大）。
                // 不吸收它，后续所有能力判断就永远停留在「登录前」那一套——
                // 真机实测：登录前 9 项、登录后 52 项，MOVE/UIDPLUS/CONDSTORE/IDLE
                // 全部只在登录后才出现，漏了就等于把高级特性全当不支持、静默降级。
                if let Some(ResponseCode::Capability(c)) = &t.code {
                    self.caps.extend(c);
                }
                return Ok((t, untagged));
            }
            if line.starts_with('+') {
                return Err(MailError::Protocol(
                    "服务器请求 continuation，但该命令未携带 literal".into(),
                ));
            }
            if !line.starts_with('*') {
                continue;
            }
            let parsed = response::parse_untagged(&line)?;
            if let Untagged::Bye(text) = &parsed {
                return Err(MailError::Network(format!("连接被服务端关闭: {text}")));
            }
            untagged.push(parsed);
        }
    }

    /// 取 CAPABILITY（问候语未携带时主动查询）。
    pub async fn fetch_capabilities(&mut self) -> MailResult<Capabilities> {
        let (_, untagged) = self.cmd("CAPABILITY").await?;
        for u in untagged {
            if let Untagged::Capability(c) = u {
                self.caps.extend(&c);
            }
        }
        Ok(self.caps.clone())
    }

    /// 原样发送**一整行**（含 tag），不等待完成响应。
    ///
    /// 只为 IDLE 这类「协议本身就是两阶段」的命令存在：服务端先回 `+ ` 表示进入
    /// idling，之后长时间静默，直到客户端自己发 `DONE` 才给 tagged 响应。
    /// 用 [`ImapClient::cmd`] 发 IDLE 会一直等那条永远不会主动到来的完成响应。
    ///
    /// tag 由调用方给：IDLE 阶段会绕开本客户端直接操作写半边，两条路径必须用同一个
    /// tag 计数，否则完不成「谁发的谁收」的对应关系。
    pub async fn send_raw_line(&mut self, line: &str) -> MailResult<()> {
        self.io.write_line(line).await
    }

    /// 登录。优先 LOGIN，服务端拒绝时由调用方改用 `authenticate_plain`。
    pub async fn login(&mut self, user: &str, password: &str) -> MailResult<()> {
        if user.trim().is_empty() {
            return Err(MailError::InvalidInput("账号不能为空".into()));
        }
        self.cmd(&format!("LOGIN {} {}", quote(user), quote(password)))
            .await
            .map(|_| ())
    }

    /// AUTHENTICATE PLAIN（RFC 4616）。
    pub async fn authenticate_plain(&mut self, user: &str, password: &str) -> MailResult<()> {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine;
        let token = STANDARD.encode(format!("\0{user}\0{password}"));
        self.cmd(&format!("AUTHENTICATE PLAIN {token}"))
            .await
            .map(|_| ())
    }

    /// ENABLE CONDSTORE（RFC 5161），使 MODSEQ 生效。
    pub async fn enable(&mut self, capability: &str) -> MailResult<()> {
        if !self.caps.supports_enable() {
            return Ok(());
        }
        self.cmd(&format!("ENABLE {}", capability.to_ascii_uppercase()))
            .await
            .map(|_| ())
    }

    /// LIST 文件夹并识别 special-use（I-003）。
    pub async fn list_folders(&mut self, account_id: &str) -> MailResult<Vec<Mailbox>> {
        let (_, untagged) = self.cmd(r#"LIST "" "*""#).await?;
        let mut out = Vec::new();
        for u in untagged {
            if let Untagged::List(entry) = u {
                out.push(list_entry_to_mailbox(&entry, account_id));
            }
        }
        // 必须在这里补角色：不少服务器（Stalwart 就是这样）对 INBOX 什么都不标，
        // 返回的是裸 `() "/" "INBOX"`。不按名字兜底的话，整条业务链都会认为
        // 「账号没有收件箱 / 没有回收站」，归档和删除直接报 Unsupported。
        fill_missing_roles(&mut out);
        Ok(out)
    }

    /// SELECT 并记录 UIDVALIDITY / UIDNEXT / HIGHESTMODSEQ（I-004）。
    pub async fn select(&mut self, mailbox: &str) -> MailResult<SelectedState> {
        let (tagged, untagged) = self.cmd(&format!("SELECT {}", quote(mailbox))).await?;
        let mut st = SelectedState {
            mailbox: Some(mailbox.to_string()),
            ..SelectedState::default()
        };
        apply_select_codes(&mut st, tagged.code.as_ref());
        for u in untagged {
            match u {
                Untagged::Exists(n) => st.exists = n,
                Untagged::Ok(code, _) => apply_select_codes(&mut st, code.as_ref()),
                _ => {}
            }
        }
        self.state = st.clone();
        Ok(st)
    }

    /// 增量拉取邮件头部+F 结构（不下载正文，I-005）。
    pub async fn fetch_metadata(
        &mut self,
        clause: &str,
        account_id: &str,
        mailbox_id: &str,
    ) -> MailResult<Vec<Email>> {
        // 修饰符位置是有坑的：计划层产出 `100:* (CHANGEDSINCE 7)`，插进
        // `UID FETCH {clause} (UID FLAGS ...)` 后会变成属性位上的 `(CHANGEDSINCE 7)`，
        // Stalwart 直接回 `BAD Invalid attribute "CHANGEDSINCE"`（12s 的 BAD 实测：
        // 属性位必失败、属性之后必成功，且 `(CHANGEDSINCE 999999)` 返回 0 条
        // 证明过滤是真执行而非被忽略）。所以这里把修饰符拆出来，统一补在末尾。
        let (uid_set, modifier) = split_fetch_modifier(clause);
        let cmd = format!(
            "UID FETCH {uid_set} (UID FLAGS RFC822.SIZE INTERNALDATE MODSEQ ENVELOPE BODYSTRUCTURE){modifier}"
        );
        let (_, untagged) = self.cmd(&cmd).await?;
        let mut emails = Vec::new();
        for u in untagged {
            if let Untagged::Fetch(item) = u {
                emails.push(fetch_item_to_email(&item, account_id, mailbox_id));
            }
        }
        Ok(emails)
    }

    /// 查询指定部件的传输编码（v0.8 新增，纯增量）：取 BODYSTRUCTURE 后按
    /// part_id 匹配叶子节点。附件下载（download_blob）需要它把
    /// `BODY[part]` 返回的传输编码原始字节解码回二进制原样。
    pub async fn fetch_part_encoding(
        &mut self,
        uid: u32,
        part: &str,
    ) -> MailResult<crate::cte::TransferEncoding> {
        let (_, untagged) = self
            .cmd(&format!("UID FETCH {uid} (BODYSTRUCTURE)"))
            .await?;
        for u in &untagged {
            if let Untagged::Fetch(item) = u {
                if let Some(body) = &item.body {
                    for leaf in body.leaves() {
                        if leaf.part_id == part {
                            return Ok(crate::cte::TransferEncoding::parse(&leaf.encoding));
                        }
                    }
                }
            }
        }
        Err(MailError::NotFound(format!("UID {uid} 无部件 {part} 的编码信息")))
    }

    /// 拉取指定部件正文（literal 读取）。
    pub async fn fetch_part(&mut self, uid: u32, part: &str) -> MailResult<Vec<u8>> {
        let tag = self.next_tag();
        self.io
            .write_line(&format!("{tag} UID FETCH {uid} (BODY.PEEK[{part}])"))
            .await?;
        let mut data: Option<Vec<u8>> = None;
        loop {
            let line = self.io.read_line().await?;
            if let Some(t) = response::parse_tagged(&line)? {
                if t.tag != tag {
                    continue;
                }
                if !t.is_ok() {
                    return Err(map_status_error(&t));
                }
                break;
            }
            if line.starts_with('*') {
                if let Some(len) = literal_length(&line) {
                    data = Some(self.io.read_literal(len).await?);
                    // 消费该响应剩余的收尾部分
                    let _ = self.io.read_line().await?;
                }
            }
        }
        data.ok_or_else(|| MailError::NotFound(format!("UID {uid} 的部件 {part} 无内容")))
    }

    /// 标记同步：`+FLAGS` / `-FLAGS`（I-006），同一批内合并为一条命令。
    pub async fn store_flags(
        &mut self,
        uid_set: &str,
        add: &[Flag],
        remove: &[Flag],
    ) -> MailResult<()> {
        if add.is_empty() && remove.is_empty() {
            return Ok(());
        }
        for (op, flags) in [("+", add), ("-", remove)] {
            if flags.is_empty() {
                continue;
            }
            let list = flags
                .iter()
                .map(|f| f.as_imap())
                .collect::<Vec<_>>()
                .join(" ");
            self.cmd(&format!("UID STORE {uid_set} {op}FLAGS.SILENT ({list})"))
                .await?;
        }
        Ok(())
    }

    /// MOVE 优先，不支持降级 COPY+STORE(+EXPUNGE)；Gmail 永不 EXPUNGE（I-007 / I-011）。
    pub async fn move_messages(
        &mut self,
        uids: &[u32],
        destination: &str,
    ) -> MailResult<MoveOutcome> {
        if uids.is_empty() {
            return Err(MailError::InvalidInput("未选择邮件".into()));
        }
        let set = crate::uid::UidRange::format_set(
            &uids
                .iter()
                .map(|u| crate::uid::UidRange::single(*u))
                .collect::<Vec<_>>(),
        );

        if self.caps.supports_move() {
            let (tagged, untagged) = self
                .cmd(&format!("UID MOVE {set} {}", quote(destination)))
                .await?;
            let copied = copy_uid_of(&tagged, &untagged);
            return Ok(MoveOutcome {
                uids: uids.to_vec(),
                strategy: MoveStrategy::Move,
                copied,
            });
        }

        let (tagged, untagged) = self
            .cmd(&format!("UID COPY {set} {}", quote(destination)))
            .await?;
        let copied = copy_uid_of(&tagged, &untagged);
        self.cmd(&format!("UID STORE {set} +FLAGS.SILENT (\\Deleted)"))
            .await?;

        if self.caps.is_gmail() {
            // Gmail 的 \Deleted 仅等同于打标签，显式 EXPUNGE 可能造成用户误删
            return self.finish_move(uids, MoveStrategy::CopyStore, copied);
        }

        // 精确删除依赖 UIDPLUS；没有就退到「延迟删除」，绝不用整箱 EXPUNGE 兜底
        if !self.caps.supports_uidplus() {
            return self.finish_move(uids, MoveStrategy::CopyStore, copied);
        }

        self.cmd(&format!("UID EXPUNGE {set}")).await?;
        self.finish_move(uids, MoveStrategy::CopyStoreExpunge, copied)
    }

    fn finish_move(
        &self,
        uids: &[u32],
        strategy: MoveStrategy,
        copied: Vec<(u32, u32)>,
    ) -> MailResult<MoveOutcome> {
        Ok(MoveOutcome {
            uids: uids.to_vec(),
            strategy,
            copied,
        })
    }

    /// 物理删除：`+FLAGS \Deleted` 后立即 EXPUNGE（回收站内清理用）。
    ///
    /// 返回实际粒度，让调用方能如实告知用户"是否可能连带清掉别的邮件"。
    pub async fn expunge_uids(&mut self, uids: &[u32]) -> MailResult<ExpungeStrategy> {
        if uids.is_empty() {
            return Ok(ExpungeStrategy::Precise);
        }
        let set = crate::uid::UidRange::format_set(
            &uids
                .iter()
                .map(|u| crate::uid::UidRange::single(*u))
                .collect::<Vec<_>>(),
        );
        self.store_flags(&set, &[Flag::Deleted], &[]).await?;
        if self.caps.supports_uidplus() {
            self.cmd(&format!("UID EXPUNGE {set}")).await?;
            return Ok(ExpungeStrategy::Precise);
        }
        self.cmd("EXPUNGE").await?;
        Ok(ExpungeStrategy::MailboxWide)
    }

    /// 仅复制（保留原件），用于 S3 适配层的 `copy_emails`。
    /// 在当前已 SELECT 的文件夹里，挑出**真实存在**的那些 UID。
    ///
    /// 存在的理由只有一个，但它很致命：真机对不存在的 UID 执行 `UID COPY` / `UID MOVE` /
    /// `UID STORE` / `UID EXPUNGE` **一律回 OK**（分别是 `OK No messages were copied.`、
    /// `OK No messages were moved.`、`OK UID STORE completed`、`OK UID EXPUNGE completed`），
    /// 服务器上什么都没发生，客户端却拿到成功。
    ///
    /// 于是「本地主键里的文件夹已过期」这种常见情况（别的客户端把邮件移走了、服务器 MOVE 换了
    /// UID 但没给 COPYUID）表现为：队列清空、零报错、报告一片绿，用户的邮件纹丝不动。
    /// 写操作前用本方法确认一遍，就能把「静默空转」变成可观测的结果。
    pub async fn existing_uids(&mut self, uids: &[u32]) -> MailResult<Vec<u32>> {
        if uids.is_empty() {
            return Ok(Vec::new());
        }
        let set = crate::uid::UidRange::format_set(
            &uids
                .iter()
                .map(|u| crate::uid::UidRange::single(*u))
                .collect::<Vec<_>>(),
        );
        let (_, untagged) = self.cmd(&format!("UID FETCH {set} (UID)")).await?;
        let mut present = Vec::new();
        for item in untagged {
            if let Untagged::Fetch(f) = item {
                if let Some(uid) = f.uid {
                    present.push(uid);
                }
            }
        }
        Ok(present)
    }

    /// UID SEARCH（v0.7 新增，纯增量）：按 IMAP SEARCH 准则在当前已 SELECT 的
    /// 文件夹里过滤 UID。
    ///
    /// `criteria` 必须是合法的 SEARCH 准则串（不含 `SEARCH` 关键字本身），
    /// 例如 `OR OR SUBJECT "foo" FROM "foo" TO "foo"`。传统 `* SEARCH n...` 与
    /// ESEARCH（RFC 4731，`* ESEARCH (TAG "x") UID ALL 1:5`）两种响应形态都认。
    pub async fn uid_search(&mut self, criteria: &str) -> MailResult<Vec<u32>> {
        let (_, untagged) = self.cmd(&format!("UID SEARCH {criteria}")).await?;
        Ok(parse_search_uids(&untagged))
    }

    pub async fn copy_messages(&mut self, uids: &[u32], destination: &str) -> MailResult<()> {
        if uids.is_empty() {
            return Err(MailError::InvalidInput("未选择邮件".into()));
        }
        let set = crate::uid::UidRange::format_set(
            &uids
                .iter()
                .map(|u| crate::uid::UidRange::single(*u))
                .collect::<Vec<_>>(),
        );
        self.cmd(&format!("UID COPY {set} {}", quote(destination)))
            .await
            .map(|_| ())
    }

    /// 删除：Gmail 走「移至废件箱」，其他服务商加 \Deleted 后 EXPUNGE。
    pub async fn delete_messages(&mut self, uids: &[u32], trash: &str) -> MailResult<MoveOutcome> {
        self.move_messages(uids, trash).await
    }

    /// APPEND 草稿，返回服务端分配的 UID（UIDPLUS 可用时）。
    pub async fn append_message(
        &mut self,
        mailbox: &str,
        flags: &[Flag],
        message: &[u8],
    ) -> MailResult<Option<u32>> {
        let tag = self.next_tag();
        let flag_str = flags
            .iter()
            .map(|f| f.as_imap())
            .collect::<Vec<_>>()
            .join(" ");
        self.io
            .write_line(&format!(
                "{tag} APPEND {} ({flag_str}) {{{}}}",
                quote(mailbox),
                message.len()
            ))
            .await?;

        // 等待 continuation
        loop {
            let line = self.io.read_line().await?;
            if line.starts_with('+') {
                break;
            }
            if let Some(t) = response::parse_tagged(&line)? {
                if t.tag == tag {
                    return Err(map_status_error(&t));
                }
            }
        }

        self.io.write_bytes(message).await?;
        self.io.write_line("").await?;

        loop {
            let line = self.io.read_line().await?;
            if let Some(t) = response::parse_tagged(&line)? {
                if t.tag != tag {
                    continue;
                }
                if !t.is_ok() {
                    return Err(map_status_error(&t));
                }
                if let Some(ResponseCode::AppendUid { uid, .. }) = t.code {
                    return Ok(Some(uid));
                }
                return Ok(None);
            }
        }
    }

    pub async fn logout(&mut self) -> MailResult<()> {
        let _ = self.cmd("LOGOUT").await;
        let _ = self.io.shutdown().await;
        Ok(())
    }
}

fn apply_select_codes(st: &mut SelectedState, code: Option<&ResponseCode>) {
    match code {
        Some(ResponseCode::UidValidity(v)) => st.uid_validity = Some(*v),
        Some(ResponseCode::UidNext(v)) => st.uid_next = Some(*v),
        Some(ResponseCode::HighestModSeq(v)) => st.highest_modseq = Some(*v),
        Some(ResponseCode::ReadOnly) => st.read_only = true,
        Some(ResponseCode::ReadWrite) => st.read_only = false,
        _ => {}
    }
}

fn list_entry_to_mailbox(entry: &ListEntry, account_id: &str) -> Mailbox {
    let mut mb = Mailbox::new(
        entry.name.clone(),
        account_id.to_string(),
        entry.name.clone(),
    );
    mb.role = entry.role;
    if let Some(delim) = entry.delimiter {
        if let Some((parent, _)) = entry.name.rsplit_once(delim) {
            mb.parent_id = Some(parent.to_string());
        }
    }
    mb
}

/// 正文部件描述符：`part_id;size=<n>;cte=<编码>;charset=<字符集>`。
///
/// 上层取正文时必须知道传输编码（base64 / quoted-printable）才能正确解码，
/// 否则拿到的是一串 base64 文本 —— 对用户就是乱码。
fn part_descriptor(part: &crate::bodystructure::BodyPart) -> String {
    format!(
        "{};size={};cte={};charset={}",
        part.part_id,
        part.size,
        if part.encoding.is_empty() {
            "7bit"
        } else {
            part.encoding.as_str()
        },
        part.charset.as_deref().unwrap_or("")
    )
}

fn fetch_item_to_email(item: &FetchItem, account_id: &str, mailbox_id: &str) -> Email {
    let mut email = Email::new(
        match item.uid {
            Some(uid) => format!("uid:{uid}"),
            None => format!("seq:{}", item.seq),
        },
        account_id,
        mailbox_id,
    );
    email.uid = item.uid;
    email.flags = item.flags.clone();
    email.size = item.size.unwrap_or(0);
    email.modseq = item.modseq;
    if let Some(env) = &item.envelope {
        email.subject = env.subject.clone().unwrap_or_default();
        email.from = env.from.clone();
        email.to = env.to.clone();
        email.cc = env.cc.clone();
        email.bcc = env.bcc.clone();
        email.reply_to = env.reply_to.clone();
        email.message_id = env.message_id.clone();
        email.in_reply_to = env.in_reply_to.clone();
        if let Some(d) = env.date.as_deref().and_then(parse_rfc2822) {
            email.sent_at = Some(d);
        }
    }
    if let Some(d) = item.internal_date.as_deref().and_then(parse_internal_date) {
        email.received_at = d;
    }
    if let Some(body) = &item.body {
        email.attachments = body.to_attachments();
        if let Some(text) = body.find_body(false) {
            email
                .raw_headers
                .insert("X-Body-Part".into(), part_descriptor(text));
        }
        if let Some(html) = body.find_body(true) {
            email
                .raw_headers
                .insert("X-Body-Html-Part".into(), part_descriptor(html));
        }
    }
    email
}

pub fn parse_internal_date(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(d) = DateTime::parse_from_rfc2822(s) {
        return Some(d.with_timezone(&Utc));
    }
    // INTERNALDATE: "13-Nov-2026 10:00:00 +0800"
    DateTime::parse_from_str(s.trim(), "%d-%b-%Y %H:%M:%S %z")
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn parse_rfc2822(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc2822(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn literal_length(line: &str) -> Option<usize> {
    let start = line.rfind('{')?;
    let end = line.rfind('}')?;
    if end < start {
        return None;
    }
    line[start + 1..end].trim().parse().ok()
}

/// 把同步计划产出的 `"{uid_set} (CHANGEDSINCE n)"` 拆成（UID 集合, 修饰符）。
///
/// 存在的理由是服务器差异：RFC 7162 允许修饰符紧跟 sequence-set，但 Stalwart 只认
/// 「属性之后」这一种写法，属性位会被判 `Invalid attribute "CHANGEDSINCE"`。
/// 拆开后由 [`ImapClient::fetch_metadata`] 统一补到末尾，两种服务器都能跑。
fn split_fetch_modifier(clause: &str) -> (&str, String) {
    match clause.find(" (CHANGEDSINCE ") {
        Some(idx) => {
            let (set, modifier) = clause.split_at(idx);
            (set, modifier.to_string())
        }
        None => (clause, String::new()),
    }
}

/// IMAP 字符串引用（转义反斜杠与引号）。
pub fn quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn map_status_error(t: &TaggedResponse) -> MailError {
    let text = t.text.clone();
    let upper = text.to_ascii_uppercase();
    if matches!(t.code, Some(ResponseCode::TryCreate)) {
        return MailError::NotFound(text);
    }
    match t.status {
        Status::Bad => MailError::Protocol(format!("命令被拒绝: {text}")),
        Status::No => {
            if upper.contains("AUTHENTICATIONFAILED")
                || upper.contains("INVALID CREDENTIALS")
                || upper.contains("AUTHENTICATE")
                || upper.contains("LOGIN FAILED")
                || upper.contains("AUTHORIZATIONFAILED")
            {
                MailError::Authentication(text)
            } else if upper.contains("QUOTA") || upper.contains("OVER LIMIT") {
                MailError::Quota(text)
            } else if upper.contains("NOT FOUND") || upper.contains("NONEXISTENT") {
                MailError::NotFound(text)
            } else {
                MailError::Protocol(format!("服务端拒绝: {text}"))
            }
        }
        Status::Bye => MailError::Network(format!("连接被关闭: {text}")),
        _ => MailError::Protocol(format!("未预期的响应: {text}")),
    }
}

/// 文件夹角色兜底推断：服务端未提供 SPECIAL-USE 时按名称猜测（I-003）。
pub fn guess_role(name: &str) -> MailboxRole {
    let lower = name.to_ascii_lowercase();
    let last = lower.rsplit('/').next().unwrap_or(&lower);
    match last {
        "inbox" => MailboxRole::Inbox,
        "sent" | "sent items" | "sent messages" | "已发送" => MailboxRole::Sent,
        // "deleted messages" 是 QQ 邮箱实测名（S6-18 live 踩到）
        "trash" | "deleted" | "deleted items" | "deleted messages" | "已删除" => {
            MailboxRole::Trash
        }
        "drafts" | "草稿" | "草稿箱" => MailboxRole::Drafts,
        "junk" | "spam" | "垃圾邮件" => MailboxRole::Junk,
        "archive" | "all mail" | "归档" => MailboxRole::Archive,
        _ => MailboxRole::Other,
    }
}

/// 为未标注 special-use 的文件夹补角色。
pub fn fill_missing_roles(folders: &mut [Mailbox]) {
    let claimed: Vec<MailboxRole> = folders
        .iter()
        .map(|f| f.role)
        .filter(|r| *r != MailboxRole::Other)
        .collect();
    for f in folders.iter_mut() {
        if f.role == MailboxRole::Other {
            let guessed = guess_role(&f.name);
            if guessed != MailboxRole::Other && !claimed.contains(&guessed) {
                f.role = guessed;
            }
        }
    }
}

/// 生成本地缓存主键（文件夹 + UID），避免跨账号/跨文件夹冲突。
pub fn local_email_id(account_id: &str, mailbox_id: &str, uid: u32) -> Id {
    format!("{account_id}/{mailbox_id}/{uid}")
}

/// 从 SEARCH 命令的 untagged 响应里提取 UID（v0.7 新增，纯解析、无 IO）。
///
/// 两种服务器形态都兼容：
/// * 传统形态：`* SEARCH 3 5 9`（结果为空时是裸 `* SEARCH`，无数字）；
/// * ESEARCH（RFC 4731）：`* ESEARCH (TAG "A2") UID ALL 1:5,7`。
///   ESEARCH 的非 UID 变体（没有 `UID` 键）直接忽略，避免把邮件序号当 UID。
fn parse_search_uids(untagged: &[Untagged]) -> Vec<u32> {
    let mut uids = Vec::new();
    for u in untagged {
        let Untagged::Other(body) = u else { continue };
        let body = body.trim();
        // 传统 `SEARCH ...` 与 ESEARCH（RFC 4731，`ESEARCH (TAG "x") UID ALL ...`）
        let rest = body
            .strip_prefix("SEARCH")
            .or_else(|| body.strip_prefix("ESEARCH"));
        let Some(rest) = rest else { continue };
        // 剩余部分：传统形态是空白分隔的数字；ESEARCH 是 "(TAG ..) UID ALL set"
        let rest = rest.trim_start();
        let rest_upper = rest.to_ascii_uppercase();
        if rest_upper.starts_with("(TAG") {
            // ESEARCH：UID 集合取 ALL/MIN/MAX；RFC 4731 规定只有出现 `UID` 键后
            // 这些集合才是 UID 语义，否则是邮件序号，绝不能混当 UID。
            let mut uid_seen = false;
            let mut collect = false;
            for tok in rest.split_whitespace() {
                let up = tok.to_ascii_uppercase();
                match up.as_str() {
                    "UID" => {
                        uid_seen = true;
                        collect = false;
                    }
                    "ALL" | "MIN" | "MAX" => collect = uid_seen,
                    ")" => collect = false,
                    _ if collect && tok.contains(|c: char| c.is_ascii_digit()) => {
                        if let Ok(set) = crate::uid::UidRange::parse_set(tok.trim_matches(',')) {
                            for r in set {
                                if r.start <= r.end {
                                    uids.extend(r.start..=r.end);
                                }
                            }
                        }
                        collect = false;
                    }
                    _ => {}
                }
            }
        } else {
            for tok in rest.split_whitespace() {
                if let Ok(uid) = tok.parse::<u32>() {
                    uids.push(uid);
                }
            }
        }
    }
    uids
}

#[cfg(test)]
mod search_uid_tests {
    use super::parse_search_uids;
    use crate::response::Untagged;

    #[test]
    fn parses_classic_search_response() {
        let lines = vec![
            Untagged::Other("SEARCH 3 5 9".into()),
            Untagged::Ok(None, "done".into()),
        ];
        assert_eq!(parse_search_uids(&lines), vec![3, 5, 9]);
    }

    /// 结果为空时服务器只回裸 `* SEARCH`（无数字），必须返回空列表而不是报错。
    #[test]
    fn empty_search_yields_no_uids() {
        let lines = vec![Untagged::Other("SEARCH".into())];
        assert!(parse_search_uids(&lines).is_empty());
    }

    #[test]
    fn parses_esearch_response_with_uid_set() {
        let lines = vec![Untagged::Other(r#"ESEARCH (TAG "A2") UID ALL 1:3,7"#.into())];
        assert_eq!(parse_search_uids(&lines), vec![1, 2, 3, 7]);
    }

    /// 没有 `UID` 键的 ESEARCH 是邮件**序号**语义，绝不能混当 UID。
    #[test]
    fn ignores_non_uid_esearch() {
        let lines = vec![Untagged::Other(r#"ESEARCH (TAG "A3") ALL 2,4"#.into())];
        assert!(parse_search_uids(&lines).is_empty());
    }

    #[test]
    fn ignores_non_search_lines() {
        let lines = vec![Untagged::Other("12 EXISTS".into()), Untagged::Other("FETCH".into())];
        assert!(parse_search_uids(&lines).is_empty());
    }
}

#[cfg(test)]
mod fetch_modifier_tests {
    use super::split_fetch_modifier;

    /// Stalwart 只接受「属性之后」的修饰符，拆分必须无损：集合原样、修饰符连前导空格。
    #[test]
    fn splits_changedsince_out_of_uid_set() {
        assert_eq!(
            split_fetch_modifier("100:* (CHANGEDSINCE 7)"),
            ("100:*", " (CHANGEDSINCE 7)".to_string())
        );
    }

    #[test]
    fn leaves_plain_uid_sets_untouched() {
        assert_eq!(split_fetch_modifier("1:*"), ("1:*", String::new()));
        assert_eq!(split_fetch_modifier("42"), ("42", String::new()));
    }

    /// STORE/COPY 这类不带修饰符的集合里出现括号也不能被误拆。
    #[test]
    fn ignores_non_modifier_parentheses() {
        assert_eq!(split_fetch_modifier("1,2,3"), ("1,2,3", String::new()));
    }
}

