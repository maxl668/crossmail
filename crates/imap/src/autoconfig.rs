//! 自动配置（需求 I-002）：内置模板 + Thunderbird autoconfig + Outlook Autodiscover + SRV。

use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::SecurityMode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub host: String,
    pub port: u16,
    pub security: SecurityMode,
    pub username: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Builtin,
    Autoconfig,
    Autodiscover,
    Srv,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoConfig {
    pub imap: ServerSpec,
    pub smtp: ServerSpec,
    pub source: ConfigSource,
}

impl AutoConfig {
    fn from_hosts(
        domain: &str,
        email: &str,
        imap_port: u16,
        smtp_port: u16,
        source: ConfigSource,
    ) -> Self {
        Self {
            imap: ServerSpec {
                host: format!("imap.{domain}"),
                port: imap_port,
                security: if imap_port == 993 {
                    SecurityMode::ImplicitTls
                } else {
                    SecurityMode::StartTls
                },
                username: email.to_string(),
            },
            smtp: ServerSpec {
                host: format!("smtp.{domain}"),
                port: smtp_port,
                security: if smtp_port == 465 {
                    SecurityMode::ImplicitTls
                } else {
                    SecurityMode::StartTls
                },
                username: email.to_string(),
            },
            source,
        }
    }
}

fn split_email(email: &str) -> Option<(String, String)> {
    let (local, domain) = email.trim().split_once('@')?;
    if local.is_empty() || !domain.contains('.') {
        return None;
    }
    Some((local.to_string(), domain.to_ascii_lowercase()))
}

/// 内置主流服务商模板（国内优先：QQ / 163 / 126 / 阿里 / 新浪 / 腾讯企业邮）。
pub fn from_builtin(email: &str) -> Option<AutoConfig> {
    let (_, domain) = split_email(email)?;
    let src = ConfigSource::Builtin;
    let cfg = match domain.as_str() {
        "gmail.com" | "googlemail.com" => AutoConfig::from_hosts("gmail.com", email, 993, 465, src),
        "qq.com" | "foxmail.com" => AutoConfig::from_hosts("qq.com", email, 993, 465, src),
        "163.com" => AutoConfig::from_hosts("163.com", email, 993, 465, src),
        "126.com" => AutoConfig::from_hosts("126.com", email, 993, 465, src),
        "sina.com" | "sina.cn" => AutoConfig::from_hosts("sina.com", email, 993, 465, src),
        "aliyun.com" => AutoConfig::from_hosts("aliyun.com", email, 993, 465, src),
        "outlook.com" | "hotmail.com" | "live.com" => {
            AutoConfig::from_hosts("outlook.com", email, 993, 587, src)
        }
        "icloud.com" | "me.com" => AutoConfig::from_hosts("icloud.com", email, 993, 587, src),
        "yahoo.com" => AutoConfig::from_hosts("yahoo.com", email, 993, 465, src),
        _ => return None,
    };
    Some(cfg)
}

/// Thunderbird autoconfig 探测地址。
pub fn autoconfig_url(email: &str) -> Option<String> {
    let (_, domain) = split_email(email)?;
    Some(format!(
        "https://autoconfig.{domain}/.well-known/autoconfig/mail/config-v1.1.xml?emailaddress={email}"
    ))
}

/// Outlook Autodiscover 探测地址。
pub fn autodiscover_url(email: &str) -> Option<String> {
    let (_, domain) = split_email(email)?;
    Some(format!(
        "https://autodiscover.{domain}/autodiscover/autodiscover.xml?emailaddress={email}"
    ))
}

/// SRV 记录名称（RFC 6186）。
pub fn srv_records(domain: &str) -> Vec<String> {
    vec![
        format!("_imaps._tcp.{domain}"),
        format!("_submission._tcp.{domain}"),
    ]
}

/// 探测顺序：内置模板 → autoconfig → autodiscover → SRV。
pub fn candidates(email: &str) -> Vec<String> {
    let mut out = Vec::new();
    if from_builtin(email).is_some() {
        out.push("builtin".to_string());
    }
    if let Some(u) = autoconfig_url(email) {
        out.push(u);
    }
    if let Some(u) = autodiscover_url(email) {
        out.push(u);
    }
    if let Some((_, domain)) = split_email(email) {
        out.extend(srv_records(&domain));
    }
    out
}

fn tag_value(block: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(block[start..end].trim().to_string())
}

fn spec_from_block(block: &str, email: &str) -> Option<ServerSpec> {
    let host = tag_value(block, "hostname")?;
    let port: u16 = tag_value(block, "port")?.parse().ok()?;
    let socket = tag_value(block, "socketType").unwrap_or_else(|| "SSL".into());
    let security = match socket.to_ascii_uppercase().as_str() {
        "SSL" | "TLS" => SecurityMode::ImplicitTls,
        "STARTTLS" => SecurityMode::StartTls,
        other => {
            return none_or_unsupported(other);
        }
    };
    let username = tag_value(block, "username")
        .unwrap_or_else(|| "%EMAILADDRESS%".into())
        .replace("%EMAILADDRESS%", email);
    Some(ServerSpec {
        host,
        port,
        security,
        username,
    })
}

fn none_or_unsupported(_reason: &str) -> Option<ServerSpec> {
    // 明文 socketType 一律拒绝（SEC-001）
    None
}

/// 解析 Thunderbird autoconfig XML（轻量扫描，不引入 XML 依赖）。
pub fn parse_autoconfig_xml(xml: &str, email: &str) -> MailResult<AutoConfig> {
    let imap_block = extract_server_block(xml, "imap")
        .ok_or_else(|| MailError::Parse("autoconfig 缺少 incomingServer(imap)".into()))?;
    let smtp_block = extract_server_block(xml, "smtp")
        .ok_or_else(|| MailError::Parse("autoconfig 缺少 outgoingServer(smtp)".into()))?;
    let imap = spec_from_block(&imap_block, email)
        .ok_or_else(|| MailError::Unsupported("imap 配置不含 SSL/STARTTLS，已拒绝明文".into()))?;
    let smtp = spec_from_block(&smtp_block, email)
        .ok_or_else(|| MailError::Unsupported("smtp 配置不含 SSL/STARTTLS，已拒绝明文".into()))?;
    Ok(AutoConfig {
        imap,
        smtp,
        source: ConfigSource::Autoconfig,
    })
}

fn extract_server_block(xml: &str, kind: &str) -> Option<String> {
    let tag = if kind == "imap" {
        "incomingServer"
    } else {
        "outgoingServer"
    };
    let marker = format!("type=\"{kind}\"");
    let mut search_from = 0usize;
    while let Some(rel) = xml[search_from..].find(&format!("<{tag}")) {
        let start = search_from + rel;
        let end = xml[start..].find(&format!("</{tag}>"))? + start;
        let block = &xml[start..end];
        if block.contains(&marker) {
            return Some(block.to_string());
        }
        search_from = end;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="example.com">
    <domain>example.com</domain>
    <incomingServer type="imap">
      <hostname>imap.example.com</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <authentication>password-cleartext</authentication>
      <username>%EMAILADDRESS%</username>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILADDRESS%</username>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    #[test]
    fn builtin_qq() {
        let c = from_builtin("12345@qq.com").unwrap();
        assert_eq!(c.imap.host, "imap.qq.com");
        assert_eq!(c.imap.port, 993);
        assert_eq!(c.smtp.port, 465);
        assert_eq!(c.source, ConfigSource::Builtin);
    }

    #[test]
    fn builtin_outlook_uses_starttls_smtp() {
        let c = from_builtin("a@outlook.com").unwrap();
        assert_eq!(c.smtp.port, 587);
        assert_eq!(c.smtp.security, SecurityMode::StartTls);
    }

    #[test]
    fn unknown_domain_returns_none() {
        assert!(from_builtin("a@unknown-corp.cn").is_none());
        assert!(from_builtin("not-an-email").is_none());
    }

    #[test]
    fn parse_thunderbird_xml() {
        let c = parse_autoconfig_xml(SAMPLE, "bob@example.com").unwrap();
        assert_eq!(c.imap.host, "imap.example.com");
        assert_eq!(c.imap.security, SecurityMode::ImplicitTls);
        assert_eq!(c.smtp.security, SecurityMode::StartTls);
        assert_eq!(c.imap.username, "bob@example.com");
    }

    #[test]
    fn plaintext_socket_type_rejected() {
        let xml = SAMPLE.replace(
            "<socketType>SSL</socketType>",
            "<socketType>plain</socketType>",
        );
        let err = parse_autoconfig_xml(&xml, "bob@example.com").unwrap_err();
        assert_eq!(err.kind(), crossmail_core::error::ErrorKind::Unsupported);
    }

    #[test]
    fn candidate_order() {
        let list = candidates("x@gmail.com");
        assert_eq!(list[0], "builtin");
        assert!(list[1].contains("autoconfig.gmail.com"));
        assert!(list.iter().any(|s| s.starts_with("_imaps._tcp")));
    }
}
