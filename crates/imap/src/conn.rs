//! 连接管理（需求 I-001 / I-012）：TLS 策略、超时、指数退避重连。

use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::SecurityMode;
use std::time::Duration;

/// 指数退避参数。
#[derive(Debug, Clone, PartialEq)]
pub struct Backoff {
    pub initial_ms: u64,
    pub max_ms: u64,
    pub factor: f64,
    pub max_retries: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial_ms: 1_000,
            max_ms: 60_000,
            factor: 2.0,
            max_retries: 5,
        }
    }
}

impl Backoff {
    /// 第 `attempt` 次重试前的等待时间（attempt 从 0 开始）。
    pub fn delay(&self, attempt: u32) -> Duration {
        let raw = self.initial_ms as f64 * self.factor.powi(attempt as i32);
        let capped = raw.min(self.max_ms as f64);
        Duration::from_millis(capped.max(0.0) as u64)
    }

    /// 只对可重试错误重试，且不超过上限（A-006）。
    pub fn should_retry(&self, err: &MailError, attempt: u32) -> bool {
        attempt < self.max_retries && err.is_retryable()
    }
}

/// 连接策略。
#[derive(Debug, Clone, PartialEq)]
pub struct ConnectionPolicy {
    pub connect_timeout: Duration,
    pub command_timeout: Duration,
    /// IDLE 不可用时的轮询间隔（I-008 降级）
    pub poll_interval: Duration,
    pub backoff: Backoff,
}

impl Default for ConnectionPolicy {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(20),
            command_timeout: Duration::from_secs(60),
            poll_interval: Duration::from_secs(300),
            backoff: Backoff::default(),
        }
    }
}

impl ConnectionPolicy {
    pub fn quick_for_test() -> Self {
        Self {
            connect_timeout: Duration::from_millis(200),
            command_timeout: Duration::from_millis(500),
            poll_interval: Duration::from_millis(10),
            backoff: Backoff {
                initial_ms: 1,
                max_ms: 5,
                factor: 1.0,
                max_retries: 3,
            },
        }
    }
}

/// 端口 -> 传输安全模式推断（993/465 隐式 TLS，143/587 STARTTLS）。
pub fn security_for_port(port: u16) -> MailResult<SecurityMode> {
    match port {
        993 | 465 => Ok(SecurityMode::ImplicitTls),
        143 | 587 => Ok(SecurityMode::StartTls),
        other => Err(MailError::Unsupported(format!(
            "不支持明文端口 {other}，仅允许 993/465（TLS）或 143/587（STARTTLS）"
        ))),
    }
}

/// 目标校验：空主机名与非法端口一律拒绝（SEC-001）。
pub fn validate_target(host: &str, port: u16) -> MailResult<SecurityMode> {
    if host.trim().is_empty() {
        return Err(MailError::InvalidInput("服务器地址不能为空".into()));
    }
    if host.contains(char::is_whitespace) {
        return Err(MailError::InvalidInput("服务器地址包含空白字符".into()));
    }
    security_for_port(port)
}

/// 带指数退避的重连执行器（I-012）。
pub async fn with_retry<F, Fut, T>(policy: &ConnectionPolicy, mut op: F) -> MailResult<T>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = MailResult<T>>,
{
    let mut attempt = 0u32;
    loop {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !policy.backoff.should_retry(&e, attempt) {
                    return Err(e);
                }
                tokio::time::sleep(policy.backoff.delay(attempt)).await;
                attempt += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn backoff_grows_and_caps() {
        let b = Backoff {
            initial_ms: 100,
            max_ms: 1000,
            factor: 2.0,
            max_retries: 10,
        };
        assert_eq!(b.delay(0), Duration::from_millis(100));
        assert_eq!(b.delay(1), Duration::from_millis(200));
        assert_eq!(b.delay(3), Duration::from_millis(800));
        assert_eq!(b.delay(9), Duration::from_millis(1000));
    }

    #[test]
    fn only_retryable_errors_retried() {
        let b = Backoff::default();
        assert!(b.should_retry(&MailError::Network("reset".into()), 0));
        assert!(!b.should_retry(&MailError::Authentication("bad".into()), 0));
        assert!(!b.should_retry(&MailError::Network("reset".into()), 5));
    }

    #[test]
    fn security_inferred_from_port() {
        assert_eq!(security_for_port(993).unwrap(), SecurityMode::ImplicitTls);
        assert_eq!(security_for_port(465).unwrap(), SecurityMode::ImplicitTls);
        assert_eq!(security_for_port(143).unwrap(), SecurityMode::StartTls);
        assert_eq!(security_for_port(587).unwrap(), SecurityMode::StartTls);
    }

    #[test]
    fn plaintext_port_rejected() {
        let err = security_for_port(110).unwrap_err();
        assert_eq!(err.kind(), crossmail_core::error::ErrorKind::Unsupported);
    }

    #[test]
    fn empty_host_rejected() {
        assert!(validate_target("  ", 993).is_err());
        assert!(validate_target("imap.example.com", 993).is_ok());
    }

    #[tokio::test]
    async fn retry_until_success() {
        let policy = ConnectionPolicy::quick_for_test();
        let count = AtomicU32::new(0);
        let out = with_retry(&policy, |_attempt| {
            let n = count.fetch_add(1, Ordering::SeqCst);
            async move {
                if n < 2 {
                    Err(MailError::Network("flaky".into()))
                } else {
                    Ok(42)
                }
            }
        })
        .await;
        assert_eq!(out.unwrap(), 42);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn fatal_error_not_retried() {
        let policy = ConnectionPolicy::quick_for_test();
        let count = AtomicU32::new(0);
        let out: MailResult<()> = with_retry(&policy, |_| {
            count.fetch_add(1, Ordering::SeqCst);
            async { Err(MailError::Authentication("denied".into())) }
        })
        .await;
        assert!(out.is_err());
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
