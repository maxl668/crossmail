//! JMAP 认证：HTTP Basic、Bearer Token 与 OAuth2 授权码（J-002）。
//!
//! RFC 8620 §4.1 只要求「HTTP 层认证」，具体机制交给 RFC 7235 —— 三个都要支持：
//! * `Basic`：账号口令 / 应用专用密码。**自建服务器（Stalwart、Cyrus、Dovecot+JMAP）
//!   与多数企业网关只提供这一种**，缺了它整个 JMAP 适配层在真机上根本进不去。
//! * `Bearer`：API Token（Fastmail、Gmail 风格的静态 token）。
//! * `OAuth2`：授权码 + PKCE，带刷新。

use crate::session::percent_encode;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use crossmail_core::error::{MailError, MailResult};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuth2State {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_endpoint: String,
    pub client_id: String,
    pub expires_at: Option<DateTime<Utc>>,
}

/// HTTP Basic 凭证。
///
/// 自定义 `Debug` 是为了让密码**永远不进日志**：`AuthMethod` 会被诊断包、
/// panic 消息、`{:?}` 格式串一路带出去，默认 derive 会把明文口令原样打出来。
#[derive(Clone, PartialEq, Eq)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicAuth")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthMethod {
    Basic(BasicAuth),
    Bearer(String),
    OAuth2(OAuth2State),
}

impl AuthMethod {
    /// 账号 + 口令（或应用专用密码）走 HTTP Basic。
    pub fn basic(username: impl Into<String>, password: impl Into<String>) -> Self {
        AuthMethod::Basic(BasicAuth {
            username: username.into(),
            password: password.into(),
        })
    }

    pub fn bearer(token: impl Into<String>) -> Self {
        AuthMethod::Bearer(token.into())
    }

    pub fn oauth2(
        access_token: impl Into<String>,
        refresh_token: Option<String>,
        token_endpoint: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Self {
        AuthMethod::OAuth2(OAuth2State {
            access_token: access_token.into(),
            refresh_token,
            token_endpoint: token_endpoint.into(),
            client_id: client_id.into(),
            expires_at: None,
        })
    }

    /// 完整 Authorization 头值。
    ///
    /// Basic 的 token62 = base64(`user:pass`)，按 RFC 7617 §2 用 UTF-8 编码
    /// （RFC 7617 §2.1 明确默认字符集是 UTF-8，非 ISO-8859-1）。
    pub fn header(&self) -> String {
        match self {
            AuthMethod::Basic(b) => {
                let raw = format!("{}:{}", b.username, b.password);
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
                format!("Basic {encoded}")
            }
            _ => format!("Bearer {}", self.access_token()),
        }
    }

    pub fn access_token(&self) -> &str {
        match self {
            // Basic 没有 token 概念，返回用户名只为让日志可读；**绝不返回密码**。
            AuthMethod::Basic(b) => &b.username,
            AuthMethod::Bearer(t) => t,
            AuthMethod::OAuth2(s) => &s.access_token,
        }
    }

    pub fn is_expired(&self) -> bool {
        match self {
            // Basic 与 Bearer 都没有过期时间可判断，只能靠服务端 401 反馈
            AuthMethod::Basic(_) | AuthMethod::Bearer(_) => false,
            AuthMethod::OAuth2(s) => s.expires_at.is_some_and(|t| t <= Utc::now()),
        }
    }

    /// 凭证文件名/日志用的可读标识（永不含秘密）。
    pub fn label(&self) -> &str {
        match self {
            AuthMethod::Basic(_) => "basic",
            AuthMethod::Bearer(_) => "bearer",
            AuthMethod::OAuth2(_) => "oauth2",
        }
    }

    /// PKCE S256 授权 URL（code_challenge 由平台侧生成后传入，避免核心库引入加密依赖）。
    pub fn authorization_url(
        &self,
        auth_endpoint: &str,
        redirect_uri: &str,
        state: &str,
        code_challenge: &str,
        scopes: &[&str],
    ) -> MailResult<String> {
        let client_id = match self {
            AuthMethod::Basic(_) => {
                return Err(MailError::Unsupported("Basic 模式无需 OAuth2 授权".into()))
            }
            AuthMethod::Bearer(_) => {
                return Err(MailError::Unsupported("Bearer 模式无需 OAuth2 授权".into()))
            }
            AuthMethod::OAuth2(s) => &s.client_id,
        };
        let sep = if auth_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        Ok(format!(
            "{auth_endpoint}{sep}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
            percent_encode(client_id),
            percent_encode(redirect_uri),
            percent_encode(&scopes.join(" ")),
            percent_encode(state),
            percent_encode(code_challenge),
        ))
    }

    /// RFC 6749 §6 刷新令牌请求表单。
    pub fn refresh_form(&self) -> MailResult<Vec<(String, String)>> {
        match self {
            AuthMethod::Basic(_) => Err(MailError::Unsupported(
                "Basic 凭证不支持刷新，口令失效需重新输入".into(),
            )),
            AuthMethod::Bearer(_) => Err(MailError::Unsupported("Bearer Token 不支持刷新".into())),
            AuthMethod::OAuth2(s) => {
                let refresh = s.refresh_token.clone().ok_or_else(|| {
                    MailError::Authentication("缺少 refresh_token，需重新授权".into())
                })?;
                Ok(vec![
                    ("grant_type".into(), "refresh_token".into()),
                    ("refresh_token".into(), refresh),
                    ("client_id".into(), s.client_id.clone()),
                ])
            }
        }
    }

    pub fn token_endpoint(&self) -> MailResult<&str> {
        match self {
            AuthMethod::Basic(_) => {
                Err(MailError::Unsupported("Basic 模式无 token endpoint".into()))
            }
            AuthMethod::Bearer(_) => Err(MailError::Unsupported(
                "Bearer 模式无 token endpoint".into(),
            )),
            AuthMethod::OAuth2(s) => Ok(&s.token_endpoint),
        }
    }

    /// 应用 token 响应；`expires_in` 按秒换算为绝对过期时间。
    pub fn apply_token_response(&mut self, v: &Value) -> MailResult<()> {
        let state = match self {
            AuthMethod::Basic(_) => {
                return Err(MailError::Unsupported("Basic 凭证不接受 token 响应".into()))
            }
            AuthMethod::Bearer(t) => {
                let token = v
                    .get("access_token")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        MailError::Authentication("token 响应缺少 access_token".into())
                    })?;
                *t = token.to_string();
                return Ok(());
            }
            AuthMethod::OAuth2(s) => s,
        };
        let access = v
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| MailError::Authentication("token 响应缺少 access_token".into()))?
            .to_string();
        state.access_token = access;
        if let Some(refresh) = v.get("refresh_token").and_then(Value::as_str) {
            state.refresh_token = Some(refresh.to_string());
        }
        state.expires_at = v
            .get("expires_in")
            .and_then(Value::as_i64)
            .map(|secs| Utc::now() + chrono::Duration::seconds(secs.max(0)));
        Ok(())
    }
}
