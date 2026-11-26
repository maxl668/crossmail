//! 正文 / 附件文件缓存（S4-03）：落盘 + 登记 + 配额 + LRU 淘汰。
//!
//! 三条硬规则：
//! 1. 缓存目录之外的文件一律不删（只把登记清掉），防止脏登记误删用户文件；
//! 2. 删除前先核对登记表，账本与磁盘不漂移；
//! 3. 淘汰按 `last_access` 从旧到新，直到低于配额。

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Utc};
use crossmail_core::{MailError, MailResult};
use crossmail_storage::Database;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::lock_db;

/// 缓存对象类型，决定落地子目录与扩展名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    TextBody,
    HtmlBody,
    Attachment,
}

impl CacheKind {
    fn dir(&self) -> &'static str {
        match self {
            CacheKind::TextBody => "text",
            CacheKind::HtmlBody => "html",
            CacheKind::Attachment => "att",
        }
    }

    fn ext(&self) -> &'static str {
        match self {
            CacheKind::TextBody => "txt",
            CacheKind::HtmlBody => "html",
            CacheKind::Attachment => "bin",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub root: PathBuf,
    /// 磁盘配额（字节）
    pub quota_bytes: u64,
}

impl CacheConfig {
    pub fn new(root: impl Into<PathBuf>, quota_bytes: u64) -> Self {
        Self {
            root: root.into(),
            quota_bytes,
        }
    }

    /// 默认 512MB 配额。
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self::new(root, 512 * 1024 * 1024)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvictionReport {
    pub removed: Vec<String>,
    /// 从磁盘真正删掉的字节数
    pub freed_bytes: u64,
    /// 登记在册但不在缓存目录内的路径（只清登记，不动文件）
    pub foreign_entries: usize,
    pub usage_after: u64,
}

/// 文件缓存管理器。
pub struct BodyCache {
    db: Arc<Mutex<Database>>,
    cfg: CacheConfig,
}

impl BodyCache {
    pub fn new(db: Arc<Mutex<Database>>, cfg: CacheConfig) -> MailResult<Self> {
        std::fs::create_dir_all(&cfg.root)
            .map_err(|e| MailError::Storage(format!("创建缓存目录失败: {e}")))?;
        Ok(Self { db, cfg })
    }

    pub fn config(&self) -> &CacheConfig {
        &self.cfg
    }

    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    /// 写正文/附件进缓存，返回落盘路径；写入后立即检查配额。
    pub fn store(
        &self,
        email_id: &str,
        kind: CacheKind,
        part: Option<&str>,
        data: &[u8],
    ) -> MailResult<PathBuf> {
        let path = self.path_for(email_id, kind, part);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| MailError::Storage(format!("创建缓存子目录失败: {e}")))?;
        }
        std::fs::write(&path, data)
            .map_err(|e| MailError::Storage(format!("写缓存文件失败: {e}")))?;
        let size = data.len() as u64;
        let path_str = path.to_string_lossy().to_string();
        lock_db(&self.db)?.record_cache_file(&path_str, size)?;
        Ok(path)
    }

    /// 读缓存；命中即刷新 LRU 时间。
    pub fn load(
        &self,
        email_id: &str,
        kind: CacheKind,
        part: Option<&str>,
    ) -> MailResult<Option<Vec<u8>>> {
        let path = self.path_for(email_id, kind, part);
        let path_str = path.to_string_lossy().to_string();
        let size = lock_db(&self.db)?.cache_file_size(&path_str)?;
        if size.is_none() {
            // 未登记即视为不在缓存（哪怕磁盘上有残留，也不认）
            return Ok(None);
        }
        match std::fs::read(&path) {
            Ok(bytes) => {
                lock_db(&self.db)?.touch_cache_file(&path_str, Utc::now())?;
                Ok(Some(bytes))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                // 账本有、磁盘无 → 清掉脏登记，当作未命中
                lock_db(&self.db)?.forget_cache_file(&path_str)?;
                Ok(None)
            }
            Err(e) => Err(MailError::Storage(format!("读缓存文件失败: {e}"))),
        }
    }

    pub fn load_text(&self, email_id: &str, kind: CacheKind) -> MailResult<Option<String>> {
        let bytes = self.load(email_id, kind, None)?;
        match bytes {
            None => Ok(None),
            Some(b) => String::from_utf8(b)
                .map(Some)
                .map_err(|_| MailError::Storage("缓存正文不是合法 UTF-8".into())),
        }
    }

    pub fn usage(&self) -> MailResult<u64> {
        lock_db(&self.db)?.cache_usage()
    }

    pub fn is_over_quota(&self) -> MailResult<bool> {
        Ok(self.usage()? > self.cfg.quota_bytes)
    }

    /// 超配额则按 LRU 淘汰，返回本次淘汰详情。
    pub fn enforce_quota(&self) -> MailResult<EvictionReport> {
        self.enforce_quota_at(self.cfg.quota_bytes, Utc::now())
    }

    /// 同上，但时间与配额可注入（便于测试确定性）。
    pub fn enforce_quota_at(&self, quota: u64, _now: DateTime<Utc>) -> MailResult<EvictionReport> {
        let candidates = lock_db(&self.db)?.evict_lru(quota)?;
        let mut report = EvictionReport {
            removed: Vec::new(),
            freed_bytes: 0,
            foreign_entries: 0,
            usage_after: 0,
        };
        for path_str in candidates {
            let path = PathBuf::from(&path_str);
            if self.owns(&path) {
                if let Ok(meta) = std::fs::metadata(&path) {
                    report.freed_bytes = report.freed_bytes.saturating_add(meta.len());
                }
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        // 文件删不掉时把登记补回去，避免账实不一致
                        lock_db(&self.db)?.record_cache_file(
                            &path_str,
                            std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0),
                        )?;
                        return Err(MailError::Storage(format!("删除缓存文件失败: {e}")));
                    }
                }
            } else {
                // 不在缓存目录内：只清登记，绝不碰文件
                report.foreign_entries += 1;
            }
            report.removed.push(path_str);
        }
        report.usage_after = self.usage()?;
        Ok(report)
    }

    /// 清空整个缓存（退出登录 / 用户手动清理）。
    pub fn clear(&self) -> MailResult<usize> {
        let removed = self.enforce_quota_at(0, Utc::now())?;
        Ok(removed.removed.len())
    }

    /// 路径是否落在缓存根目录内（防止越界删除）。
    pub fn owns(&self, path: &Path) -> bool {
        let Ok(root) = self.cfg.root.canonicalize() else {
            return false;
        };
        // 文件可能已不存在，退化为对父目录做规范化
        let candidate = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => match path.parent().and_then(|p| p.canonicalize().ok()) {
                Some(parent) => parent.join(path.file_name().unwrap_or_default()),
                None => return false,
            },
        };
        candidate.starts_with(&root)
    }

    fn path_for(&self, email_id: &str, kind: CacheKind, part: Option<&str>) -> PathBuf {
        let mut key = email_id.to_string();
        if let Some(p) = part {
            key.push('#');
            key.push_str(p);
        }
        self.cfg
            .root
            .join(kind.dir())
            .join(format!("{}.{}", safe_name(&key), kind.ext()))
    }
}

/// 把业务 id 变成定长、无路径语义的文件名。
///
/// 直接拼接 server 给的 id 会把 `/`、`..` 带进路径，必须编码；超长再退化为
/// 稳定哈希，避免文件名超过文件系统限制。
fn safe_name(key: &str) -> String {
    let encoded = URL_SAFE_NO_PAD.encode(key.as_bytes());
    if encoded.len() <= 100 {
        return encoded;
    }
    format!("h{:016x}", fnv1a64(key.as_bytes()))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
