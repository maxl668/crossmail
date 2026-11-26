//! CrossMail 可观测层：分级日志（OBS-001）、轮转与压缩（OBS-002）、
//! 诊断包导出（OBS-003）。
//!
//! 设计边界：
//! * 零三方依赖 —— gzip（RFC 1951/1952）与 tar（ustar）都在本 crate 内实现，
//!   产物用系统 `gunzip` / `tar` 独立校验，不做「自己编自己解」的自证。
//! * 日志是**旁路**：任何写盘/压缩失败都不允许反向打断同步与业务。
//! * 诊断包**默认脱敏**：邮箱掩码、密钥替换、不含正文附件。

pub mod diagnostics;
pub mod gzip;
pub mod level;
pub mod logger;
pub mod redact;
pub mod rotate;

#[cfg(test)]
pub(crate) mod testing;

pub use diagnostics::{export, export_bytes, DiagnosticAccount, DiagnosticsInput};
pub use gzip::{crc32, gzip_compress};
pub use level::{LogFilter, LogLevel};
pub use logger::{LogRecord, Logger, DEFAULT_RECENT_CAPACITY};
pub use redact::{redact_email, redact_json, redact_text};
pub use rotate::{LogSink, RotateConfig, RotateReport, ACTIVE_LOG};
