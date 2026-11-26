//! JMAP Session 与能力协商（J-001 / J-002 / J-010）。

use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Address, Id};
use crossmail_core::provider::ProviderCapabilities;
use serde_json::Value;
use std::collections::BTreeMap;

pub const CORE_CAPABILITY: &str = "urn:ietf:params:jmap:core";
pub const MAIL_CAPABILITY: &str = "urn:ietf:params:jmap:mail";
pub const SUBMISSION_CAPABILITY: &str = "urn:ietf:params:jmap:submission";
pub const BLOB_CAPABILITY: &str = "urn:ietf:params:jmap:blob";
/// RFC 8887 WebSocket 推送能力（真机声明，本版本未实现）。
pub const WEBSOCKET_CAPABILITY: &str = "urn:ietf:params:jmap:websocket";

/// 账号摘要（Session.accounts 的成员）。
#[derive(Debug, Clone, PartialEq)]
pub struct JmapAccount {
    pub name: String,
    pub is_personal: bool,
    pub is_read_only: bool,
    pub account_capabilities: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub username: String,
    pub api_url: String,
    pub download_url: String,
    pub upload_url: String,
    pub event_source_url: Option<String>,
    pub state: String,
    pub primary_accounts: BTreeMap<String, Id>,
    pub accounts: BTreeMap<Id, JmapAccount>,
    pub capabilities: BTreeMap<String, Value>,
}

impl Session {
    pub fn from_value(v: &Value) -> MailResult<Self> {
        let api_url = require_string(v, "apiUrl")?;
        let capabilities = as_object(v, "capabilities")?;
        let primary_accounts: BTreeMap<String, Id> = as_object(v, "primaryAccounts")?
            .iter()
            .filter_map(|(k, val)| val.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();

        let accounts = as_object(v, "accounts")?
            .iter()
            .map(|(id, val)| {
                let caps = val
                    .get("accountCapabilities")
                    .and_then(Value::as_object)
                    .map(|m| m.iter().map(|(k, x)| (k.clone(), x.clone())).collect())
                    .unwrap_or_default();
                (
                    id.clone(),
                    JmapAccount {
                        name: val
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        is_personal: val
                            .get("isPersonal")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        is_read_only: val
                            .get("isReadOnly")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        account_capabilities: caps,
                    },
                )
            })
            .collect();

        Ok(Self {
            username: v
                .get("username")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            api_url,
            download_url: v
                .get("downloadUrl")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            upload_url: v
                .get("uploadUrl")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            event_source_url: v
                .get("eventSourceUrl")
                .and_then(Value::as_str)
                .map(str::to_string),
            state: v
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            primary_accounts,
            accounts,
            capabilities,
        })
    }

    pub fn has_capability(&self, urn: &str) -> bool {
        self.capabilities.contains_key(urn)
    }

    /// 主邮件账号；缺失 mail 能力时明确报错，由上层回退到 IMAP+SMTP。
    pub fn primary_mail_account(&self) -> MailResult<Id> {
        self.primary_accounts
            .get(MAIL_CAPABILITY)
            .cloned()
            .ok_or_else(|| {
                MailError::Unsupported("服务端未提供 urn:ietf:params:jmap:mail 主账号".into())
            })
    }

    pub fn account(&self, id: &str) -> Option<&JmapAccount> {
        self.accounts.get(id)
    }

    /// 能力位 -> 统一能力集（不支持的能力自动降级，J-001）。
    /// 有 mail 能力但没有主账号时同样视为不可用，避免上层误判后调用才报错。
    pub fn provider_capabilities(&self) -> ProviderCapabilities {
        let has_mail_account = self.primary_accounts.contains_key(MAIL_CAPABILITY);
        ProviderCapabilities {
            jmap_mail: self.has_capability(MAIL_CAPABILITY) && has_mail_account,
            blob_upload: !self.upload_url.is_empty() && has_mail_account,
            push: self
                .event_source_url
                .as_deref()
                .is_some_and(|u| !u.is_empty()),
            ..ProviderCapabilities::default()
        }
    }

    /// RFC 8887 WebSocket 推送地址（`urn:ietf:params:jmap:websocket`）。
    ///
    /// 真机声明了该能力（`supportsPush: true`），但本版本未实现——客户端只有 HTTP
    /// 通道，故推送统一走 EventSource。此访问器供上层探测与未来扩展。
    pub fn websocket_url(&self) -> Option<String> {
        self.capabilities
            .get(WEBSOCKET_CAPABILITY)
            .and_then(|c| c.get("url"))
            .and_then(Value::as_str)
            .filter(|u| !u.is_empty())
            .map(str::to_string)
    }

    /// 构造 EventSource 请求地址（RFC 8620 §7.3）。
    ///
    /// 真机的 `eventSourceUrl` 是**模板**：
    /// `.../jmap/eventsource/?types={types}&closeafter={closeafter}&ping={ping}`。
    /// 实测原样请求会被服务端以 `400` 拒绝，`types=` 留空同样 `400`——必须显式替换，
    /// 且空类型集要落到通配 `*`。
    ///
    /// 对不含占位符的服务器，参数按查询串追加（这是 RFC 允许的另一种形态）。
    pub fn event_source_endpoint(
        &self,
        types: &[String],
        close_after: &str,
        ping: Option<u32>,
    ) -> MailResult<String> {
        let template = self
            .event_source_url
            .as_deref()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| MailError::Unsupported("服务端未提供 eventSourceUrl".into()))?;
        if !matches!(close_after, "no" | "state") {
            return Err(MailError::InvalidInput(format!(
                "closeafter 只能是 no 或 state，收到 {close_after}"
            )));
        }
        // 空集合在真机上是 400，语义上的「全都要」是通配 `*`。
        let types_value = if types.is_empty() {
            "*".to_string()
        } else {
            for t in types {
                if t.is_empty() || t.contains(['&', '#', '?', ' ', '=']) {
                    return Err(MailError::InvalidInput(format!("非法的推送类型名: {t:?}")));
                }
            }
            types.join(",")
        };

        let mut url = template
            .replace("{types}", &types_value)
            .replace("{closeafter}", close_after);
        if let Some(secs) = ping {
            url = url.replace("{ping}", &secs.to_string());
        } else {
            // 不关心心跳时也要清掉占位符，否则仍是 400。
            url = url.replace("{ping}", "0");
        }
        if url.contains('{') || url.contains('}') {
            return Err(MailError::Protocol(format!(
                "eventSourceUrl 模板仍有未替换的占位符: {url}"
            )));
        }
        if !template.contains('{') {
            let sep = if url.contains('?') { '&' } else { '?' };
            url.push(sep);
            url.push_str(&format!("types={types_value}&closeafter={close_after}"));
            if let Some(secs) = ping {
                url.push_str(&format!("&ping={secs}"));
            }
        }
        Ok(url)
    }

    fn number(&self, urn: &str, key: &str, default: u64) -> u64 {
        self.capabilities
            .get(urn)
            .and_then(|c| c.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(default)
    }

    pub fn max_size_request(&self) -> u64 {
        self.number(CORE_CAPABILITY, "maxSizeRequest", 10_000_000)
    }

    pub fn max_objects_in_get(&self) -> u64 {
        self.number(CORE_CAPABILITY, "maxObjectsInGet", 500)
    }

    pub fn max_size_attachments_per_email(&self) -> u64 {
        self.number(MAIL_CAPABILITY, "maxSizeAttachmentsPerEmail", 50_000_000)
    }

    /// 下载 URL 模板展开（RFC 8620 §3.3）。
    pub fn download_url_for(
        &self,
        account_id: &str,
        blob_id: &str,
        name: &str,
        mime: &str,
    ) -> String {
        self.download_url
            .replace("{accountId}", account_id)
            .replace("{blobId}", blob_id)
            .replace("{name}", &percent_encode(name))
            .replace("{type}", &percent_encode(mime))
    }

    /// 上传 URL 模板展开（RFC 8620 §3.4）。
    pub fn upload_url_for(&self, account_id: &str) -> String {
        self.upload_url.replace("{accountId}", account_id)
    }
}

fn require_string(v: &Value, key: &str) -> MailResult<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| MailError::Protocol(format!("Session 缺少或非法字段 {key}")))
}

fn as_object(v: &Value, key: &str) -> MailResult<BTreeMap<String, Value>> {
    let obj = v
        .get(key)
        .and_then(Value::as_object)
        .ok_or_else(|| MailError::Protocol(format!("Session 缺少对象字段 {key}")))?;
    Ok(obj.iter().map(|(k, x)| (k.clone(), x.clone())).collect())
}

/// RFC 8620 §3.1 自动发现候选地址；调用方按序尝试直到拿到 Session。
pub fn well_known_candidates(email: &str) -> MailResult<Vec<String>> {
    let address = Address::new(email);
    if !address.is_valid() {
        return Err(MailError::InvalidInput(format!("邮箱地址无效: {email}")));
    }
    let domain = address.domain();
    Ok(vec![
        format!("https://{domain}/.well-known/jmap"),
        format!("https://jmap.{domain}/.well-known/jmap"),
    ])
}

/// RFC 3986 unreserved 之外的字符做百分号编码（纯函数，无额外依赖）。
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        let c = *byte;
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}
