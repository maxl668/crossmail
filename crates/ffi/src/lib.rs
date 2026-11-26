//! CrossMail 对外绑定层（S5-01）：供 Kotlin / Swift / Python 调用。

// scaffolding 由 uniffi 模板生成到 OUT_DIR，里面的排版不由我们控制，
// 对生成代码开这条 lint 豁免，避免流水线被上游模板格式卡住。
#![allow(clippy::empty_line_after_doc_comments)]
//!
//! 架构：`crossmail_ffi.udl` 是**唯一契约**，build.rs 由它生成 scaffolding，
//! 本 crate 只提供与契约同名同形的普通类型与实现。字段或签名对不上时
//! scaffolding 会在编译期报错——这是防「绑定调用不存在的符号、运行期才崩」
//! 的关键护栏，别把类型定义挪进子模块后忘了在根部 re-export。
//!
//! 线程模型：所有接口同步阻塞，UI 侧请放到后台线程/IO 调度器调用。

uniffi::include_scaffolding!("crossmail_ffi");

mod dto;
mod engine;
mod push;

pub use dto::*;
pub use engine::{open_engine, CrossMailEngine};
pub use push::{
    MailboxUnreadDto, PushEventDto, PushEventKindDto, PushObserver, PushSubscription, SyncReportDto,
};

/// 核心库版本与支持的协议（关于页/诊断包用）。
pub fn core_status() -> CoreStatus {
    CoreStatus {
        version: env!("CARGO_PKG_VERSION").to_string(),
        supported_protocols: vec!["jmap".into(), "imap+smtp".into()],
    }
}

/// 协议的展示名，未知协议原样回显而不是硬塞一个假名字。
pub fn protocol_display_name(protocol: String) -> String {
    let trimmed = protocol.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "jmap" => "JMAP (RFC 8620)".into(),
        "imap" | "imap+smtp" => "IMAP4rev1 + SMTP".into(),
        "" => "Unknown".into(),
        _ => trimmed.to_string(),
    }
}
