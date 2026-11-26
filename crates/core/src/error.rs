//! 统一错误体系（需求 A-003 / J-011）：UI 层可按类型展示对应提示。

use thiserror::Error;

pub type MailResult<T> = Result<T, MailError>;

/// 错误分类，供 UI 层做分支提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Authentication,
    Network,
    Protocol,
    Quota,
    Conflict,
    NotFound,
    InvalidInput,
    Storage,
    Cancelled,
    Unsupported,
    RateLimited,
    Tls,
    Parse,
    Server,
}

impl ErrorKind {
    /// 面向用户的分类提示（不含技术细节，避免泄露服务端信息）
    pub fn user_hint(self) -> &'static str {
        match self {
            ErrorKind::Authentication => "登录状态失效，请重新登录账号",
            ErrorKind::Network => "网络连接异常，恢复后将自动重试",
            ErrorKind::Protocol => "邮件服务器返回了无法处理的响应",
            ErrorKind::Quota => "邮箱容量或请求次数已达上限",
            ErrorKind::Conflict => "邮件状态已在其他设备变更，正在重新校准",
            ErrorKind::NotFound => "该邮件或文件夹已不存在",
            ErrorKind::InvalidInput => "填写内容有误，请检查后重试",
            ErrorKind::Storage => "本地数据读写失败",
            ErrorKind::Cancelled => "操作已取消",
            ErrorKind::Unsupported => "当前邮件服务器不支持此功能",
            ErrorKind::RateLimited => "操作过于频繁，请稍后再试",
            ErrorKind::Tls => "无法验证服务器证书，连接已中止",
            ErrorKind::Parse => "邮件内容格式异常",
            ErrorKind::Server => "邮件服务器暂时不可用",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MailError {
    #[error("认证失败: {0}")]
    Authentication(String),
    #[error("网络错误: {0}")]
    Network(String),
    #[error("协议错误: {0}")]
    Protocol(String),
    #[error("配额超限: {0}")]
    Quota(String),
    #[error("状态冲突: {0}")]
    Conflict(String),
    #[error("资源不存在: {0}")]
    NotFound(String),
    #[error("输入无效: {0}")]
    InvalidInput(String),
    #[error("本地存储错误: {0}")]
    Storage(String),
    #[error("操作已取消")]
    Cancelled,
    #[error("不支持的能力: {0}")]
    Unsupported(String),
    #[error("请求过于频繁: {0}")]
    RateLimited(String),
    #[error("TLS 校验失败: {0}")]
    Tls(String),
    #[error("解析失败: {0}")]
    Parse(String),
    #[error("服务端错误 {status}: {message}")]
    Server { status: u16, message: String },
}

impl MailError {
    pub fn kind(&self) -> ErrorKind {
        match self {
            MailError::Authentication(_) => ErrorKind::Authentication,
            MailError::Network(_) => ErrorKind::Network,
            MailError::Protocol(_) => ErrorKind::Protocol,
            MailError::Quota(_) => ErrorKind::Quota,
            MailError::Conflict(_) => ErrorKind::Conflict,
            MailError::NotFound(_) => ErrorKind::NotFound,
            MailError::InvalidInput(_) => ErrorKind::InvalidInput,
            MailError::Storage(_) => ErrorKind::Storage,
            MailError::Cancelled => ErrorKind::Cancelled,
            MailError::Unsupported(_) => ErrorKind::Unsupported,
            MailError::RateLimited(_) => ErrorKind::RateLimited,
            MailError::Tls(_) => ErrorKind::Tls,
            MailError::Parse(_) => ErrorKind::Parse,
            MailError::Server { .. } => ErrorKind::Server,
        }
    }

    pub fn user_hint(&self) -> &'static str {
        self.kind().user_hint()
    }

    /// J-011 / A-005：状态冲突必须触发全量校准，而不是简单重试。
    pub fn requires_full_resync(&self) -> bool {
        matches!(self, MailError::Conflict(_))
            || matches!(self, MailError::Server { status: 409, .. })
    }

    /// A-006 离线队列：网络错误重试，协议错误终止。
    pub fn is_retryable(&self) -> bool {
        match self {
            MailError::Network(_) | MailError::RateLimited(_) => true,
            MailError::Server { status, .. } if *status >= 500 => true,
            MailError::Conflict(_) => true,
            _ => false,
        }
    }

    /// J-011：HTTP 状态码统一映射。
    pub fn from_http(status: u16, body: &str) -> Self {
        let message = body.chars().take(200).collect::<String>();
        match status {
            401 | 403 => MailError::Authentication(message),
            404 => MailError::NotFound(message),
            409 => MailError::Conflict(message),
            413 | 429 | 507 => {
                if status == 429 {
                    MailError::RateLimited(message)
                } else {
                    MailError::Quota(message)
                }
            }
            400 | 422 => MailError::Protocol(message),
            400..=499 => MailError::Protocol(message),
            _ => MailError::Server { status, message },
        }
    }
}

impl From<serde_json::Error> for MailError {
    fn from(e: serde_json::Error) -> Self {
        MailError::Parse(e.to_string())
    }
}
