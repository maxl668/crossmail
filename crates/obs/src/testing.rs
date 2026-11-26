//! 测试辅助：自建临时目录（不引入 tempfile —— 本环境无法下载新 crate）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// 生成一个可写的独立临时目录，进程号 + 自增序号保证不冲突。
pub fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "crossmail-obs-{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst),
        tag
    ));
    std::fs::create_dir_all(&dir).expect("创建临时目录失败");
    dir
}
