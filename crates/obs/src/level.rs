//! 日志级别与过滤策略（OBS-001）。
//!
//! 关键约定：**协议报文属于独立开关，不与级别混在一起**。用户把级别调到
//! Trace 是为了定位崩溃，不是为了把自己的邮件内容全量写进磁盘；反过来，
//! 排查协议问题时需要报文，但不想被上千行 Debug 噪音淹没。两个维度独立控制。

use serde::{Deserialize, Serialize};

/// 五级日志。`Error` 最严重，`Trace` 最啰嗦。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Warn => "WARN",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
            LogLevel::Trace => "TRACE",
        }
    }

    /// 解析用户输入的级别名（大小写不敏感）。
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "error" | "err" => LogLevel::Error,
            "warn" | "warning" => LogLevel::Warn,
            "info" => LogLevel::Info,
            "debug" => LogLevel::Debug,
            "trace" => LogLevel::Trace,
            _ => return None,
        })
    }

    /// 单字母，用于紧凑日志行。
    pub fn short(self) -> char {
        match self {
            LogLevel::Error => 'E',
            LogLevel::Warn => 'W',
            LogLevel::Info => 'I',
            LogLevel::Debug => 'D',
            LogLevel::Trace => 'T',
        }
    }
}

/// 过滤配置。默认 `Info` + 协议报文关闭 —— 够定位问题，又不至于把用户
/// 的邮件内容和账号信息写进日志（SEC-006 零遥测的前提）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogFilter {
    pub level: LogLevel,
    /// 是否记录协议报文（IMAP/SMTP/JMAP 的原始请求响应）
    pub protocol_payload: bool,
    /// 超过这个长度的单条消息会被截断（防一条 base64 把日志撑爆）
    pub max_message_bytes: usize,
}

impl Default for LogFilter {
    fn default() -> Self {
        Self {
            level: LogLevel::Info,
            protocol_payload: false,
            max_message_bytes: 8 * 1024,
        }
    }
}

impl LogFilter {
    pub fn new(level: LogLevel) -> Self {
        Self {
            level,
            ..Self::default()
        }
    }

    /// 是否应记录该级别
    pub fn enabled(&self, level: LogLevel) -> bool {
        level <= self.level
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_order_and_parse() {
        assert!(LogLevel::Error < LogLevel::Warn);
        assert!(LogLevel::Debug < LogLevel::Trace);
        assert_eq!(LogLevel::parse("WARN"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse(" warning "), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("verbose"), None);
    }

    #[test]
    fn default_filter_is_info_without_protocol_payload() {
        let f = LogFilter::default();
        assert_eq!(f.level, LogLevel::Info);
        assert!(!f.protocol_payload, "协议报文必须默认关闭");
        assert!(f.enabled(LogLevel::Error));
        assert!(f.enabled(LogLevel::Info));
        assert!(!f.enabled(LogLevel::Debug));
        assert!(!f.enabled(LogLevel::Trace));
    }
}
