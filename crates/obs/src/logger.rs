//! 日志门面（OBS-001）：五级过滤、协议报文独立开关、环形缓冲供诊断页读取。

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use crate::level::{LogFilter, LogLevel};
use crate::rotate::{LogSink, RotateConfig};

/// 环形缓冲默认容量：够还原「崩溃前发生了什么」，又不至于吃内存。
pub const DEFAULT_RECENT_CAPACITY: usize = 512;

/// 一条日志记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogRecord {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    /// 模块名，例如 `sync::engine`
    pub target: String,
    pub message: String,
}

impl LogRecord {
    /// 单行文本：`2026-11-13T22:10:00.123Z I sync::engine 消息`
    pub fn to_line(&self) -> String {
        format!(
            "{} {} {} {}\n",
            self.timestamp.to_rfc3339_opts(SecondsFormat::Millis, true),
            self.level.short(),
            self.target,
            self.message.replace('\n', "\\n")
        )
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // 日志锁中毒不应让整个进程崩：日志是旁路，坏了自己继续跑
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// 线程安全的日志器。
pub struct Logger {
    filter: Mutex<LogFilter>,
    sink: Mutex<Option<LogSink>>,
    recent: Mutex<VecDeque<LogRecord>>,
    capacity: usize,
}

impl Logger {
    /// 仅内存模式（无落盘）：测试、单元调试、UI 预览用。
    pub fn in_memory(filter: LogFilter) -> Self {
        Self {
            filter: Mutex::new(filter),
            sink: Mutex::new(None),
            recent: Mutex::new(VecDeque::with_capacity(DEFAULT_RECENT_CAPACITY)),
            capacity: DEFAULT_RECENT_CAPACITY,
        }
    }

    /// 落盘模式：打开日志目录并启用轮转。
    pub fn to_dir(
        dir: impl AsRef<Path>,
        rotate: RotateConfig,
        filter: LogFilter,
    ) -> io::Result<Self> {
        let sink = LogSink::open(dir, rotate)?;
        Ok(Self {
            filter: Mutex::new(filter),
            sink: Mutex::new(Some(sink)),
            recent: Mutex::new(VecDeque::with_capacity(DEFAULT_RECENT_CAPACITY)),
            capacity: DEFAULT_RECENT_CAPACITY,
        })
    }

    /// 默认配置（Info 级别、协议报文关闭、10MB / 5 份轮转）。
    pub fn with_defaults(dir: impl AsRef<Path>) -> io::Result<Self> {
        Self::to_dir(dir, RotateConfig::default(), LogFilter::default())
    }

    pub fn filter(&self) -> LogFilter {
        lock(&self.filter).clone()
    }

    pub fn set_filter(&self, filter: LogFilter) {
        *lock(&self.filter) = filter;
    }

    pub fn set_level(&self, level: LogLevel) {
        lock(&self.filter).level = level;
    }

    /// 协议报文开关（默认关闭，只在用户明确开启调试时打开）。
    pub fn set_protocol_payload(&self, enabled: bool) {
        lock(&self.filter).protocol_payload = enabled;
    }

    /// 主入口：按级别过滤后写盘 + 进环形缓冲。
    pub fn log(&self, level: LogLevel, target: impl Into<String>, message: impl Into<String>) {
        let filter = lock(&self.filter).clone();
        if !filter.enabled(level) {
            return;
        }
        let mut message = message.into();
        if message.len() > filter.max_message_bytes {
            let mut cut = filter.max_message_bytes;
            while cut > 0 && !message.is_char_boundary(cut) {
                cut -= 1;
            }
            message.truncate(cut);
            message.push_str("…(已截断)");
        }
        let record = LogRecord {
            timestamp: Utc::now(),
            level,
            target: target.into(),
            message,
        };
        self.push(record);
    }

    /// 协议报文：仅当 `protocol_payload` 打开时才落盘，且**永不**进入环形缓冲
    /// （避免诊断包里带出用户邮件原文）。
    pub fn protocol(&self, target: impl Into<String>, payload: impl Into<String>) {
        if !lock(&self.filter).protocol_payload {
            return;
        }
        let record = LogRecord {
            timestamp: Utc::now(),
            level: LogLevel::Trace,
            target: target.into(),
            message: payload.into(),
        };
        if let Some(sink) = lock(&self.sink).as_mut() {
            // 写失败不能反向打断业务，静默丢弃
            let _ = sink.write_line(&record.to_line());
        }
    }

    pub fn error(&self, target: impl Into<String>, msg: impl Into<String>) {
        self.log(LogLevel::Error, target, msg);
    }
    pub fn warn(&self, target: impl Into<String>, msg: impl Into<String>) {
        self.log(LogLevel::Warn, target, msg);
    }
    pub fn info(&self, target: impl Into<String>, msg: impl Into<String>) {
        self.log(LogLevel::Info, target, msg);
    }
    pub fn debug(&self, target: impl Into<String>, msg: impl Into<String>) {
        self.log(LogLevel::Debug, target, msg);
    }
    pub fn trace(&self, target: impl Into<String>, msg: impl Into<String>) {
        self.log(LogLevel::Trace, target, msg);
    }

    /// 最近的日志（时间正序），供诊断页 / 诊断包使用。
    pub fn recent(&self, limit: usize) -> Vec<LogRecord> {
        let ring = lock(&self.recent);
        let skip = ring.len().saturating_sub(limit);
        ring.iter().skip(skip).cloned().collect()
    }

    /// 当前日志目录（未落盘时为 None）。
    pub fn log_dir(&self) -> Option<PathBuf> {
        lock(&self.sink).as_ref().map(|s| s.dir().to_path_buf())
    }

    /// 目录下所有日志文件（活动 + 归档）。
    pub fn log_files(&self) -> Vec<PathBuf> {
        lock(&self.sink)
            .as_ref()
            .and_then(|s| s.list_files().ok())
            .unwrap_or_default()
    }

    /// 立即归档一次（诊断页「压缩旧日志」按钮）。
    pub fn rotate_now(&self) -> io::Result<bool> {
        match lock(&self.sink).as_mut() {
            Some(sink) => {
                sink.rotate()?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn push(&self, record: LogRecord) {
        if let Some(sink) = lock(&self.sink).as_mut() {
            let _ = sink.write_line(&record.to_line());
        }
        let mut ring = lock(&self.recent);
        if ring.len() == self.capacity {
            ring.pop_front();
        }
        ring.push_back(record);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::temp_dir;

    #[test]
    fn filters_by_level() {
        let log = Logger::in_memory(LogFilter::new(LogLevel::Warn));
        log.info("t", "不该出现");
        log.warn("t", "该出现");
        log.error("t", "该出现2");
        let recent = log.recent(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].level, LogLevel::Warn);
    }

    #[test]
    fn protocol_payload_is_off_by_default() {
        let dir = temp_dir("logger-protocol");
        let log = Logger::with_defaults(&dir).unwrap();
        log.protocol("imap::conn", "a1 LOGIN user pass");
        log.info("imap::conn", "已连接");
        let text = std::fs::read_to_string(dir.join("crossmail.log")).unwrap();
        assert!(!text.contains("a1 LOGIN"), "协议报文默认不得落盘：{text}");
        log.set_protocol_payload(true);
        log.protocol("imap::conn", "a1 LOGIN user pass");
        let text = std::fs::read_to_string(dir.join("crossmail.log")).unwrap();
        assert!(text.contains("a1 LOGIN"), "开启后应记录：{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn protocol_payload_never_enters_ring_buffer() {
        let log = Logger::in_memory(LogFilter {
            protocol_payload: true,
            ..LogFilter::default()
        });
        log.protocol("smtp", "MAIL FROM:<a@b>");
        assert!(log.recent(10).is_empty(), "报文不得进入诊断环形缓冲");
    }

    #[test]
    fn truncates_oversized_message() {
        let log = Logger::in_memory(LogFilter {
            max_message_bytes: 16,
            ..LogFilter::default()
        });
        log.info("t", "x".repeat(100));
        let recent = log.recent(1);
        assert!(recent[0].message.len() <= 16 + "…(已截断)".len());
        assert!(recent[0].message.contains("已截断"));
    }

    #[test]
    fn ring_buffer_keeps_latest() {
        let mut log = Logger::in_memory(LogFilter::new(LogLevel::Trace));
        log.capacity = 3;
        for i in 0..10 {
            log.info("t", format!("m{i}"));
        }
        let recent = log.recent(10);
        assert_eq!(recent.len(), 3);
        assert!(recent[2].message.ends_with("m9"));
        assert_eq!(log.recent(2).len(), 2);
    }

    #[test]
    fn writes_multiline_message_as_single_line() {
        let log = Logger::in_memory(LogFilter::default());
        log.info("t", "a\nb");
        let line = log.recent(1)[0].to_line();
        assert_eq!(line.matches('\n').count(), 1, "只允许行尾那一个换行");
        assert!(line.contains("a\\nb"));
    }

    #[test]
    fn line_format_contains_level_and_target() {
        let log = Logger::in_memory(LogFilter::default());
        log.warn("sync::engine", "冲突已裁决");
        let line = log.recent(1)[0].to_line();
        assert!(line.contains(" W "));
        assert!(line.contains("sync::engine"));
        assert!(line.ends_with('\n'));
    }
}
