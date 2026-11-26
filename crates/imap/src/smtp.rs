//! SMTP 发送（需求 I-009）：465 SMTPS / 587 STARTTLS，AUTH 优先级 XOAUTH2 > PLAIN > LOGIN。

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Address, OutgoingMessage, SecurityMode};
use std::time::Duration;
use uuid::Uuid;

use crate::transport::{self, StreamIo};

/// SMTP 通道抽象（便于离线测试）。
#[async_trait]
pub trait SmtpIo: Send + Sync {
    async fn write_line(&mut self, line: &str) -> MailResult<()>;
    async fn read_line(&mut self) -> MailResult<String>;
}

#[async_trait]
impl SmtpIo for StreamIo {
    async fn write_line(&mut self, line: &str) -> MailResult<()> {
        crate::transport::ImapIo::write_line(self, line).await
    }
    async fn read_line(&mut self) -> MailResult<String> {
        crate::transport::ImapIo::read_line(self).await
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmtpCapabilities {
    pub auth: Vec<String>,
    pub starttls: bool,
    pub size: Option<u64>,
    pub eight_bit: bool,
}

impl SmtpCapabilities {
    /// 解析 EHLO 多行响应体（不含状态码）。
    pub fn parse_ehlo(lines: &[String]) -> Self {
        let mut caps = Self::default();
        for raw in lines {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let key = it.next().unwrap_or("").to_ascii_uppercase();
            match key.as_str() {
                "AUTH" => {
                    for m in it {
                        caps.auth.push(m.to_ascii_uppercase());
                    }
                }
                "STARTTLS" => caps.starttls = true,
                "SIZE" => caps.size = it.next().and_then(|v| v.parse().ok()),
                "8BITMIME" => caps.eight_bit = true,
                _ => {}
            }
        }
        caps
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMech {
    XOAuth2,
    Plain,
    Login,
}

impl AuthMech {
    pub fn name(self) -> &'static str {
        match self {
            AuthMech::XOAuth2 => "XOAUTH2",
            AuthMech::Plain => "PLAIN",
            AuthMech::Login => "LOGIN",
        }
    }

    /// 按 I-009 挑选机制；都不可用时返回 None。
    ///
    /// 优先级 PLAIN > LOGIN > XOAUTH2（S6-18 修订，原为 XOAUTH2 最优先）：
    /// QQ/163 等授权码服务器在 EHLO 里广告 XOAUTH2，但授权码只对
    /// PLAIN/LOGIN 有效（实测 XOAUTH2 被拒后服务端直接断连，SASL 取消流程
    /// 都走不完）；而 OAuth token 恰好也兼容 PLAIN（Gmail 实测）。因此
    /// PLAIN 优先对「授权码」与「OAuth token」两类 secret 通吃；XOAUTH2
    /// 仅在服务端唯一提供它时使用（OAuth-only 服务器不受影响）。
    pub fn select(caps: &SmtpCapabilities) -> Option<AuthMech> {
        let has = |m: &str| caps.auth.iter().any(|x| x == m);
        if has("PLAIN") {
            Some(AuthMech::Plain)
        } else if has("LOGIN") {
            Some(AuthMech::Login)
        } else if has("XOAUTH2") {
            Some(AuthMech::XOAuth2)
        } else {
            None
        }
    }
}

/// 465 隐式 TLS，587 STARTTLS（I-009）。
pub fn port_for(security: SecurityMode) -> u16 {
    match security {
        SecurityMode::ImplicitTls => 465,
        SecurityMode::StartTls => 587,
    }
}

/// 建立 SMTP 通道并按需完成 STARTTLS。
pub async fn connect(
    host: &str,
    security: SecurityMode,
    timeout: Duration,
) -> MailResult<StreamIo> {
    let port = port_for(security);
    match security {
        SecurityMode::ImplicitTls => transport::connect_tls(host, port, timeout).await,
        SecurityMode::StartTls => {
            let mut io = transport::connect_plain(host, port, timeout).await?;
            // 等待 220 问候
            let (code, _) = read_reply_boxed(&mut io).await?;
            expect(code, 220, "EHLO 问候")?;
            io.write_line("EHLO crossmail-client").await?;
            let (code, lines) = read_reply_boxed(&mut io).await?;
            expect(code, 250, "EHLO")?;
            let caps = SmtpCapabilities::parse_ehlo(&lines);
            if !caps.starttls {
                return Err(MailError::Tls(
                    "服务端未提供 STARTTLS，拒绝降级为明文".into(),
                ));
            }
            io.write_line("STARTTLS").await?;
            let (code, _) = read_reply_boxed(&mut io).await?;
            expect(code, 220, "STARTTLS")?;
            let connector = transport::tls_connector()?;
            let name = rustls_pki_types::ServerName::try_from(host.to_string())
                .map_err(|_| MailError::InvalidInput(format!("非法主机名: {host}")))?;
            io.upgrade_tls(connector, name).await
        }
    }
}

async fn read_reply_boxed<I: SmtpIo + ?Sized>(io: &mut I) -> MailResult<(u16, Vec<String>)> {
    let mut lines = Vec::new();
    loop {
        let line = io.read_line().await?;
        if line.len() < 3 {
            return Err(MailError::Protocol(format!("SMTP 响应过短: {line}")));
        }
        let code: u16 = line[..3]
            .parse()
            .map_err(|_| MailError::Protocol(format!("SMTP 状态码非法: {line}")))?;
        let multiline = line.as_bytes().get(3) == Some(&b'-');
        lines.push(line.get(4..).unwrap_or("").to_string());
        if !multiline {
            return Ok((code, lines));
        }
    }
}

fn expect(code: u16, want: u16, stage: &str) -> MailResult<()> {
    if code == want {
        return Ok(());
    }
    let msg = format!("{stage} 失败，服务端返回 {code}");
    Err(match code {
        552 => MailError::Quota(msg),
        421 | 450 | 451 | 452 => MailError::Network(msg),
        // 334 只在 AUTH 阶段作为 SASL 挑战出现；期望终态却收到它 = 凭据被拒
        530 | 534 | 535 | 334 => MailError::Authentication(msg),
        _ => MailError::Protocol(msg),
    })
}

/// SMTP 会话客户端。构造时传入已加密完成的通道（secure by construction）。
pub struct SmtpClient<I: SmtpIo + ?Sized> {
    io: Box<I>,
    caps: SmtpCapabilities,
    helo_name: String,
}

impl SmtpClient<dyn SmtpIo> {
    pub fn new(io: Box<dyn SmtpIo>) -> Self {
        Self {
            io,
            caps: SmtpCapabilities::default(),
            helo_name: "crossmail-client".into(),
        }
    }

    /// 握手：读问候 + EHLO。
    pub async fn greet(&mut self) -> MailResult<&SmtpCapabilities> {
        let (code, _) = read_reply_boxed(&mut *self.io).await?;
        expect(code, 220, "连接问候")?;
        self.ehlo().await
    }

    async fn ehlo(&mut self) -> MailResult<&SmtpCapabilities> {
        self.io
            .write_line(&format!("EHLO {}", self.helo_name))
            .await?;
        let (code, lines) = read_reply_boxed(&mut *self.io).await?;
        expect(code, 250, "EHLO")?;
        self.caps = SmtpCapabilities::parse_ehlo(&lines);
        Ok(&self.caps)
    }

    pub fn capabilities(&self) -> &SmtpCapabilities {
        &self.caps
    }

    /// 认证：密码模式或 OAuth2（I-009）。
    pub async fn authenticate(&mut self, user: &str, secret: &str) -> MailResult<AuthMech> {
        let mech = AuthMech::select(&self.caps)
            .ok_or_else(|| MailError::Unsupported("服务端未提供可用的 AUTH 机制".into()))?;
        match mech {
            AuthMech::XOAuth2 => {
                let token = STANDARD.encode(format!("user={user}\x01auth=Bearer {secret}\x01\x01"));
                self.io.write_line(&format!("AUTH XOAUTH2 {token}")).await?;
                let (code, _) = read_reply_boxed(&mut *self.io).await?;
                expect(code, 235, "XOAUTH2 认证")?;
                Ok(AuthMech::XOAuth2)
            }
            AuthMech::Plain => {
                self.auth_plain(user, secret).await?;
                Ok(AuthMech::Plain)
            }
            AuthMech::Login => {
                self.auth_login(user, secret).await?;
                Ok(AuthMech::Login)
            }
        }
    }

    async fn auth_plain(&mut self, user: &str, secret: &str) -> MailResult<()> {
        let token = STANDARD.encode(format!("\0{user}\0{secret}"));
        self.io.write_line(&format!("AUTH PLAIN {token}")).await?;
        let (code, _) = read_reply_boxed(&mut *self.io).await?;
        expect(code, 235, "PLAIN 认证")
    }

    async fn auth_login(&mut self, user: &str, secret: &str) -> MailResult<()> {
        self.io.write_line("AUTH LOGIN").await?;
        let (code, _) = read_reply_boxed(&mut *self.io).await?;
        expect(code, 334, "AUTH LOGIN")?;
        self.io.write_line(&STANDARD.encode(user)).await?;
        let (code, _) = read_reply_boxed(&mut *self.io).await?;
        expect(code, 334, "AUTH LOGIN 用户名")?;
        self.io.write_line(&STANDARD.encode(secret)).await?;
        let (code, _) = read_reply_boxed(&mut *self.io).await?;
        expect(code, 235, "AUTH LOGIN 密码")
    }

    /// 发送整封邮件（含 DATA 阶段）。
    pub async fn send_message(&mut self, msg: &OutgoingMessage) -> MailResult<()> {
        msg.validate().map_err(MailError::InvalidInput)?;
        let raw = build_message(msg)?;
        self.send_raw(msg, &raw).await
    }

    /// 发送已构建好的 RFC 5322 报文。
    ///
    /// 与 `send_message` 分离，便于上层（Provider 适配层）复用同一份原始报文
    /// 提取 Message-ID，避免报文与回执不一致。
    pub async fn send_raw(&mut self, msg: &OutgoingMessage, raw: &[u8]) -> MailResult<()> {
        msg.validate().map_err(MailError::InvalidInput)?;
        if let Some(limit) = self.caps.size {
            if raw.len() as u64 > limit {
                return Err(MailError::Quota(format!(
                    "邮件大小 {} 超过服务端限制 {limit}",
                    raw.len()
                )));
            }
        }

        let from = msg.from[0].email.clone();
        self.io.write_line(&format!("MAIL FROM:<{from}>")).await?;
        let (code, _) = read_reply_boxed(&mut *self.io).await?;
        expect(code, 250, "MAIL FROM")?;

        for rcpt in msg.recipients() {
            self.io
                .write_line(&format!("RCPT TO:<{}>", rcpt.email))
                .await?;
            let (code, lines) = read_reply_boxed(&mut *self.io).await?;
            if code != 250 && code != 251 {
                return Err(MailError::Protocol(format!(
                    "收件人 {} 被拒绝: {}",
                    rcpt.email,
                    lines.join(" ")
                )));
            }
        }

        self.io.write_line("DATA").await?;
        let (code, _) = read_reply_boxed(&mut *self.io).await?;
        expect(code, 354, "DATA")?;
        for line in dot_stuffed_lines(raw) {
            self.io.write_line(&line).await?;
        }
        self.io.write_line(".").await?;
        let (code, _) = read_reply_boxed(&mut *self.io).await?;
        expect(code, 250, "邮件投递")?;
        Ok(())
    }

    pub async fn quit(&mut self) -> MailResult<()> {
        self.io.write_line("QUIT").await?;
        let _ = read_reply_boxed(&mut *self.io).await;
        Ok(())
    }
}

/// DATA 阶段的 dot-stuffing（RFC 5321 §4.5.2）。
pub fn dot_stuffed_lines(raw: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let mut lines: Vec<&str> = text.split("\r\n").collect();
    // 只裁掉「尾部 CRLF 产生的那个空串」。
    //
    // 中间的**空行必须保留**：头部与正文之间那一个空行是 MIME 的硬分隔符，
    // 早期实现把所有空行都跳过，结果服务器把 base64 正文当成头部续行吃掉，
    // 整封信解析出来正文 0 字节（Stalwart 实测：BODY[TEXT] 返回 0，
    // BODYSTRUCTURE 报 `"base64" 0 0`），收件人看到的是空邮件。
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    let mut out: Vec<String> = lines
        .into_iter()
        .map(|line| {
            if line.starts_with('.') {
                format!(".{line}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn encode_header_value(value: &str) -> String {
    if value.is_ascii() && !value.chars().any(|c| c == '\r' || c == '\n') {
        return value.to_string();
    }
    format!("=?UTF-8?B?{}?=", STANDARD.encode(value))
}

fn format_address(a: &Address) -> String {
    // 去掉可能来自用户输入的换行，防头部注入
    let email = a.email.replace(['\r', '\n'], "");
    match &a.name {
        Some(name) if !name.trim().is_empty() => {
            let clean = name.replace(['\r', '\n'], "").replace('"', "");
            format!("\"{clean}\" <{email}>")
        }
        _ => format!("<{email}>"),
    }
}

fn wrap_base64(data: &[u8]) -> String {
    let encoded = STANDARD.encode(data);
    encoded
        .as_bytes()
        .chunks(76)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// 构建 RFC 5322 邮件（MIME 多段，附件走 base64）。
pub fn build_message(msg: &OutgoingMessage) -> MailResult<Vec<u8>> {
    let boundary_mixed = format!("--=_mixed_{}", Uuid::new_v4().simple());
    let boundary_alt = format!("--=_alt_{}", Uuid::new_v4().simple());

    let has_html = msg.html_body.is_some();
    let has_attach = !msg.attachments.is_empty();

    let mut out = String::new();
    out.push_str(&format!("From: {}\r\n", format_address(&msg.from[0])));
    let join = |list: &[Address]| {
        list.iter()
            .map(format_address)
            .collect::<Vec<_>>()
            .join(", ")
    };
    if !msg.to.is_empty() {
        out.push_str(&format!("To: {}\r\n", join(&msg.to)));
    }
    if !msg.cc.is_empty() {
        out.push_str(&format!("Cc: {}\r\n", join(&msg.cc)));
    }
    out.push_str(&format!(
        "Subject: {}\r\n",
        encode_header_value(&msg.subject)
    ));
    out.push_str(&format!(
        "Date: {}\r\n",
        chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S +0000")
    ));
    let domain = msg.from[0].domain();
    let domain = if domain.is_empty() {
        "crossmail.local"
    } else {
        domain
    };
    out.push_str(&format!(
        "Message-ID: <{}@{}>\r\n",
        Uuid::new_v4().simple(),
        domain
    ));
    if let Some(irt) = &msg.in_reply_to {
        out.push_str(&format!("In-Reply-To: {irt}\r\n"));
    }
    if !msg.references.is_empty() {
        out.push_str(&format!("References: {}\r\n", msg.references.join(" ")));
    }
    out.push_str("MIME-Version: 1.0\r\n");

    if has_attach {
        out.push_str(&format!(
            "Content-Type: multipart/mixed; boundary=\"{boundary_mixed}\"\r\n\r\n"
        ));
        out.push_str(&format!("--{boundary_mixed}\r\n"));
    }

    if has_html {
        out.push_str(&format!(
            "Content-Type: multipart/alternative; boundary=\"{boundary_alt}\"\r\n\r\n"
        ));
        out.push_str(&format!("--{boundary_alt}\r\n"));
        out.push_str("Content-Type: text/plain; charset=UTF-8\r\n");
        out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        out.push_str(&wrap_base64(
            msg.text_body.as_deref().unwrap_or("").as_bytes(),
        ));
        out.push_str("\r\n\r\n");
        out.push_str(&format!("--{boundary_alt}\r\n"));
        out.push_str("Content-Type: text/html; charset=UTF-8\r\n");
        out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        out.push_str(&wrap_base64(
            msg.html_body.as_deref().unwrap_or("").as_bytes(),
        ));
        out.push_str(&format!("\r\n\r\n--{boundary_alt}--\r\n"));
    } else {
        out.push_str("Content-Type: text/plain; charset=UTF-8\r\n");
        out.push_str("Content-Transfer-Encoding: base64\r\n\r\n");
        out.push_str(&wrap_base64(
            msg.text_body.as_deref().unwrap_or("").as_bytes(),
        ));
        out.push_str("\r\n");
    }

    if has_attach {
        for att in &msg.attachments {
            let name = att.name.replace(['"', '\r', '\n'], "");
            out.push_str(&format!("\r\n--{boundary_mixed}\r\n"));
            out.push_str(&format!(
                "Content-Type: {}; name=\"{name}\"\r\n",
                att.content_type
            ));
            out.push_str("Content-Transfer-Encoding: base64\r\n");
            out.push_str(&format!(
                "Content-Disposition: attachment; filename=\"{name}\"\r\n\r\n"
            ));
            out.push_str(&wrap_base64(&att.data));
        }
        out.push_str(&format!("\r\n--{boundary_mixed}--\r\n"));
    }

    Ok(out.into_bytes())
}

/// 离线可脚本化 SMTP 通道。
pub mod testing {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    pub struct MockSmtpIo {
        script: VecDeque<String>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl MockSmtpIo {
        pub fn new<I, S>(lines: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: AsRef<str>,
        {
            Self {
                script: lines.into_iter().map(|s| s.as_ref().to_string()).collect(),
                log: Arc::new(Mutex::new(Vec::new())),
            }
        }

        pub fn log_handle(&self) -> Arc<Mutex<Vec<String>>> {
            Arc::clone(&self.log)
        }

        pub fn push_line(&mut self, line: impl Into<String>) {
            self.script.push_back(line.into());
        }

        pub fn commands(&self) -> Vec<String> {
            self.log.lock().map(|g| g.clone()).unwrap_or_default()
        }
    }

    #[async_trait]
    impl SmtpIo for MockSmtpIo {
        async fn write_line(&mut self, line: &str) -> MailResult<()> {
            if let Ok(mut g) = self.log.lock() {
                g.push(line.to_string());
            }
            Ok(())
        }

        async fn read_line(&mut self) -> MailResult<String> {
            self.script
                .pop_front()
                .ok_or_else(|| MailError::Protocol("SMTP 测试剧本已耗尽".into()))
        }
    }
}

#[cfg(test)]
mod dot_stuffing_tests {
    use super::{build_message, dot_stuffed_lines};
    use crossmail_core::model::{Address, OutgoingMessage};

    /// 头体之间的空行是 MIME 硬分隔符，被吞掉会导致正文 0 字节（真机缺陷回归）。
    #[test]
    fn keeps_blank_separator_between_headers_and_body() {
        let raw = b"Subject: x\r\nContent-Transfer-Encoding: base64\r\n\r\nQUJD\r\n";
        let lines = dot_stuffed_lines(raw);
        assert_eq!(lines.len(), 4, "头 2 行 + 空行 + 正文 1 行: {lines:?}");
        assert_eq!(lines[2], "", "MIME 分隔空行必须保留");
        assert_eq!(lines[3], "QUJD");
    }

    /// 尾部 CRLF 产生的空串要裁掉，否则 DATA 末尾会多一个空行。
    #[test]
    fn trims_trailing_crlf_only() {
        let lines = dot_stuffed_lines(b"a\r\n\r\nb\r\n\r\n");
        assert_eq!(lines, vec!["a".to_string(), String::new(), "b".to_string()]);
    }

    /// 以点开头的行仍要 RFC 5321 点填充。
    #[test]
    fn still_stuffs_leading_dots() {
        let lines = dot_stuffed_lines(b".hidden\r\n");
        assert_eq!(lines, vec!["..hidden".to_string()]);
    }

    /// 纯文本无附件时，组装结果里必须自带头体分隔空行。
    #[test]
    fn plain_message_has_header_body_separator() {
        let mut msg = OutgoingMessage::new(
            "acc",
            Address { name: None, email: "a@example.com".into() },
            vec![Address { name: None, email: "b@example.com".into() }],
            "hi",
        );
        msg.text_body = Some("hello".into());
        let raw = String::from_utf8(build_message(&msg).unwrap()).unwrap();
        assert!(
            raw.contains("Content-Transfer-Encoding: base64\r\n\r\n"),
            "缺少头体分隔空行: {raw}"
        );
    }
}
