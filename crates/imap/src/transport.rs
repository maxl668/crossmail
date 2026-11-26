//! IMAP/SMTP 传输层：TLS 建连、行缓冲读写、STARTTLS 升级，以及可脚本化的测试通道。

use async_trait::async_trait;
use crossmail_core::error::{MailError, MailResult};
use rustls::RootCertStore;
use rustls_pki_types::ServerName;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

/// 统一的异步字节流（TCP 或 TLS）。
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> AsyncStream for T {}

/// IMAP 传输抽象：便于用脚本化通道离线测试完整命令-响应流程。
#[async_trait]
pub trait ImapIo: Send + Sync {
    async fn write_line(&mut self, line: &str) -> MailResult<()>;
    async fn write_bytes(&mut self, data: &[u8]) -> MailResult<()>;
    async fn read_line(&mut self) -> MailResult<String>;
    async fn read_literal(&mut self, len: usize) -> MailResult<Vec<u8>>;
    async fn shutdown(&mut self) -> MailResult<()> {
        Ok(())
    }
}

/// 带行缓冲的连接，支持 STARTTLS 原地升级。
pub struct StreamIo {
    stream: Box<dyn AsyncStream>,
    buf: Vec<u8>,
}

impl StreamIo {
    pub fn new<S: AsyncStream + 'static>(stream: S) -> Self {
        Self {
            stream: Box::new(stream),
            buf: Vec::new(),
        }
    }

    async fn fill(&mut self) -> MailResult<usize> {
        let mut tmp = [0u8; 8192];
        let n = self
            .stream
            .read(&mut tmp)
            .await
            .map_err(|e| MailError::Network(format!("读取失败: {e}")))?;
        if n == 0 {
            return Err(MailError::Network("连接已被服务端关闭".into()));
        }
        self.buf.extend_from_slice(&tmp[..n]);
        Ok(n)
    }

    /// 原地 STARTTLS 升级，保留已缓冲但未消费的字节。
    pub async fn upgrade_tls(
        self,
        connector: TlsConnector,
        server_name: ServerName<'static>,
    ) -> MailResult<StreamIo> {
        let StreamIo { stream, buf } = self;
        let tls = connector
            .connect(server_name, stream)
            .await
            .map_err(|e| MailError::Tls(format!("TLS 握手失败: {e}")))?;
        Ok(StreamIo {
            stream: Box::new(tls),
            buf,
        })
    }

    /// 取出底层流与尚未消费的缓冲。
    ///
    /// 只给「需要把连接拆成读写两半」的场景（IDLE：等待推送与发送 `DONE` 必须并行）。
    /// 缓冲原样返回，**绝不静默丢弃**——丢弃会让调用方从第一帧起就错位，而这种错位
    /// 表现为「偶尔收到莫名其妙的一行」，极难定位。
    pub fn into_inner(self) -> (Box<dyn AsyncStream>, Vec<u8>) {
        (self.stream, self.buf)
    }
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

#[async_trait]
impl ImapIo for StreamIo {
    async fn write_line(&mut self, line: &str) -> MailResult<()> {
        let mut data = line.as_bytes().to_vec();
        data.extend_from_slice(b"\r\n");
        self.stream
            .write_all(&data)
            .await
            .map_err(|e| MailError::Network(format!("写入失败: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| MailError::Network(format!("刷新失败: {e}")))
    }

    async fn write_bytes(&mut self, data: &[u8]) -> MailResult<()> {
        self.stream
            .write_all(data)
            .await
            .map_err(|e| MailError::Network(format!("写入失败: {e}")))?;
        self.stream
            .flush()
            .await
            .map_err(|e| MailError::Network(format!("刷新失败: {e}")))
    }

    async fn read_line(&mut self) -> MailResult<String> {
        loop {
            if let Some(pos) = find_crlf(&self.buf) {
                let line: Vec<u8> = self.buf.drain(..pos + 2).collect();
                return Ok(String::from_utf8_lossy(&line[..pos]).to_string());
            }
            if self.buf.len() > 4 * 1024 * 1024 {
                return Err(MailError::Protocol("响应行过长，已中止".into()));
            }
            self.fill().await?;
        }
    }

    async fn read_literal(&mut self, len: usize) -> MailResult<Vec<u8>> {
        while self.buf.len() < len {
            self.fill().await?;
        }
        Ok(self.buf.drain(..len).collect())
    }

    async fn shutdown(&mut self) -> MailResult<()> {
        let _ = self.stream.shutdown().await;
        Ok(())
    }
}

/// 构造严格校验系统根证书的 TLS 连接器（需求 I-001 / SEC-001：禁止明文与跳过校验）。
pub fn tls_connector() -> MailResult<TlsConnector> {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if roots.roots.is_empty() {
        return Err(MailError::Tls("系统根证书为空，无法建立安全连接".into()));
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| MailError::Tls(e.to_string()))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(Arc::new(config)))
}

fn server_name(host: &str) -> MailResult<ServerName<'static>> {
    ServerName::try_from(host.to_string())
        .map_err(|_| MailError::InvalidInput(format!("非法主机名: {host}")))
}

/// 连接端点：把「连到哪个 IP:端口」与「按哪个主机名校验证书」分开。
///
/// 为什么需要分开——以下都是真实部署里的常态，不是测试专用的口子：
/// * **内外网同服**：局域网内直连 `192.0.2.10:993`，但证书签给公网域名，
///   按 IP 校验必然失败，而用户实际是安全的（同一台机器、同一张证书）。
/// * **负载均衡 / 反向代理**：连到 VIP 或代理地址，证书属于后端真实域名。
/// * **hosts 覆盖 / 分域名解析**：运维把域名指到内网地址做灰度。
///
/// 关键约束：**这不是「跳过校验」开关**。`tls_server_name` 只替换校验目标，
/// 校验强度（链路信任、主机名匹配、有效期）完全不变；它没有 `None` 就放过这种语义，
/// 不填则回落到 `connect_host`。SEC-001 的「禁止跳过证书校验」不受影响。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// TCP 实际连接的地址（IP 或域名）。
    pub connect_host: String,
    pub port: u16,
    /// 证书校验与 SNI 使用的主机名；`None` 表示与 `connect_host` 相同。
    pub tls_server_name: Option<String>,
    pub timeout: Duration,
}

impl Endpoint {
    /// 常规用法：连到哪就按哪校验。
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            connect_host: host.into(),
            port,
            tls_server_name: None,
            timeout: Duration::from_secs(30),
        }
    }

    /// 显式指定证书校验目标（见类型文档说明的三种真实场景）。
    pub fn with_tls_name(mut self, name: impl Into<String>) -> Self {
        self.tls_server_name = Some(name.into());
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 校验时真正使用的主机名。
    pub fn verify_name(&self) -> &str {
        self.tls_server_name
            .as_deref()
            .unwrap_or(&self.connect_host)
    }
}

async fn tcp_connect(host: &str, port: u16, timeout: Duration) -> MailResult<TcpStream> {
    if host.trim().is_empty() {
        return Err(MailError::InvalidInput("服务器地址为空".into()));
    }
    // 解析全部地址并 IPv4 优先：部分环境（如本机）DNS 返回 AAAA 优先，
    // 但 v6 出口可能黑洞（丢包不拒绝）——串行尝试会把整个超时预算耗在
    // v6 上（QQ SMTP 实测踩到）。v4 优先不影响纯 v6 目标；同族保持原序。
    let mut addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| MailError::Network(format!("无法解析 {host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(MailError::Network(format!("无法解析 {host}: 无可用地址")));
    }
    addrs.sort_by_key(|a| !a.is_ipv4());
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_err = format!("连接 {host}:{port} 超时（{}s）", timeout.as_secs());
    for addr in addrs {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match tokio::time::timeout(left, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last_err = format!("无法连接 {addr}: {e}"),
            Err(_) => last_err = format!("连接 {addr} 超时"),
        }
    }
    Err(MailError::Network(last_err))
}

/// 隐式 TLS 建连（993 / 465）。
pub async fn connect_tls(host: &str, port: u16, timeout: Duration) -> MailResult<StreamIo> {
    connect_tls_endpoint(&Endpoint::new(host, port).with_timeout(timeout)).await
}

/// 按端点建连：TCP 走 `connect_host`，TLS 校验与 SNI 走 `verify_name()`。
pub async fn connect_tls_endpoint(ep: &Endpoint) -> MailResult<StreamIo> {
    let tcp = tcp_connect(&ep.connect_host, ep.port, ep.timeout).await?;
    let connector = tls_connector()?;
    let io = StreamIo::new(tcp);
    io.upgrade_tls(connector, server_name(ep.verify_name())?)
        .await
}

/// 明文建连，仅用于后续 STARTTLS 升级（绝不直接用于认证，SEC-001）。
pub async fn connect_plain(host: &str, port: u16, timeout: Duration) -> MailResult<StreamIo> {
    Ok(StreamIo::new(tcp_connect(host, port, timeout).await?))
}

/// 显式释放 TLS 连接类型引用，避免未使用告警。
pub type TlsIo = StreamIo;
#[allow(dead_code)]
fn _assert_tls_stream_type(_s: TlsStream<Box<dyn AsyncStream>>) {}

/// 离线可脚本化通道：按预设剧本返回响应，并记录所有发出的命令。
pub mod testing {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    pub enum ScriptItem {
        Line(String),
        Bytes(Vec<u8>),
    }

    /// 脚本化 IMAP 通道。
    #[derive(Default)]
    pub struct MockIo {
        script: VecDeque<ScriptItem>,
        log: Arc<Mutex<Vec<String>>>,
        pub literals_written: Vec<Vec<u8>>,
    }

    impl MockIo {
        pub fn new<I, S>(lines: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: AsRef<str>,
        {
            let mut script = VecDeque::new();
            for l in lines {
                script.push_back(ScriptItem::Line(l.as_ref().to_string()));
            }
            Self {
                script,
                log: Arc::new(Mutex::new(Vec::new())),
                literals_written: Vec::new(),
            }
        }

        /// 在把通道移交给客户端之前取一份命令日志句柄。
        pub fn log_handle(&self) -> Arc<Mutex<Vec<String>>> {
            Arc::clone(&self.log)
        }

        pub fn written(&self) -> Vec<String> {
            self.log.lock().map(|g| g.clone()).unwrap_or_default()
        }

        pub fn push_line(&mut self, line: impl Into<String>) {
            self.script.push_back(ScriptItem::Line(line.into()));
        }

        pub fn push_bytes(&mut self, bytes: impl Into<Vec<u8>>) {
            self.script.push_back(ScriptItem::Bytes(bytes.into()));
        }

        pub fn last_command(&self) -> String {
            self.log
                .lock()
                .ok()
                .and_then(|g| g.last().cloned())
                .unwrap_or_default()
        }
    }

    #[async_trait]
    impl ImapIo for MockIo {
        async fn write_line(&mut self, line: &str) -> MailResult<()> {
            if let Ok(mut g) = self.log.lock() {
                g.push(line.to_string());
            }
            Ok(())
        }

        async fn write_bytes(&mut self, data: &[u8]) -> MailResult<()> {
            self.literals_written.push(data.to_vec());
            Ok(())
        }

        async fn read_line(&mut self) -> MailResult<String> {
            match self.script.pop_front() {
                Some(ScriptItem::Line(l)) => Ok(l),
                Some(ScriptItem::Bytes(b)) => Ok(String::from_utf8_lossy(&b).to_string()),
                None => Err(MailError::Protocol("测试通道剧本已耗尽".into())),
            }
        }

        async fn read_literal(&mut self, len: usize) -> MailResult<Vec<u8>> {
            match self.script.pop_front() {
                Some(ScriptItem::Bytes(b)) => {
                    if b.len() < len {
                        return Err(MailError::Protocol("literal 长度不足".into()));
                    }
                    Ok(b[..len].to_vec())
                }
                _ => Err(MailError::Protocol("期望 literal 但剧本是行".into())),
            }
        }
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn verify_name_defaults_to_connect_host() {
        let ep = Endpoint::new("192.0.2.10", 993);
        assert_eq!(ep.verify_name(), "192.0.2.10");
        assert!(ep.tls_server_name.is_none());
    }

    #[test]
    fn verify_name_can_differ_from_connect_host() {
        // 内外网同服：连内网 IP，按公网域名校验证书
        let ep = Endpoint::new("192.0.2.10", 993).with_tls_name("mail.example.com");
        assert_eq!(ep.connect_host, "192.0.2.10");
        assert_eq!(ep.verify_name(), "mail.example.com");
    }

    #[test]
    fn endpoint_rejects_illegal_tls_name() {
        // 非法主机名要在建连前就拒绝，不能等到握手才报个看不懂的错
        let ep = Endpoint::new("127.0.0.1", 993).with_tls_name("bad name with spaces");
        assert!(server_name(ep.verify_name()).is_err());
    }

    #[tokio::test]
    async fn connect_to_closed_port_is_network_error() {
        // 连不上必须是可重试的网络错误（回放层据此延后而非判死）
        let ep = Endpoint::new("127.0.0.1", 1).with_timeout(Duration::from_millis(500));
        let err = match connect_tls_endpoint(&ep).await {
            Ok(_) => panic!("端口 1 不该连上"),
            Err(e) => e,
        };
        assert!(err.is_retryable(), "连接失败应可重试，实际: {err:?}");
    }
}
