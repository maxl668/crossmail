//! 日志文件轮转与归档（OBS-002）：单文件上限 10MB、最多保留 5 份、旧文件压缩。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::gzip::gzip_compress;

/// 当前活动日志文件名（固定，便于用户/支持人员直接找到）。
pub const ACTIVE_LOG: &str = "crossmail.log";

/// 轮转配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotateConfig {
    /// 单个活动文件最大字节数，默认 10MB
    pub max_bytes: u64,
    /// 总保留份数（含当前活动文件），默认 5
    pub max_files: usize,
    /// 归档是否 gzip 压缩
    pub compress: bool,
}

impl Default for RotateConfig {
    fn default() -> Self {
        Self {
            max_bytes: 10 * 1024 * 1024,
            max_files: 5,
            compress: true,
        }
    }
}

impl RotateConfig {
    /// 允许保留的归档份数（当前活动文件占掉一份）。
    pub fn archive_slots(&self) -> usize {
        self.max_files.saturating_sub(1)
    }
}

/// 轮转结果，供日志/诊断页展示。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RotateReport {
    pub archives: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub archived_bytes: u64,
    pub compressed_bytes: u64,
}

/// 文件日志沉降器：单文件追加 + 超限轮转 + 压缩归档。
///
/// 注意：本类型**不保证**并发安全，由上层 [`crate::Logger`] 持锁串行化。
pub struct LogSink {
    dir: PathBuf,
    cfg: RotateConfig,
    file: File,
    written: u64,
}

impl LogSink {
    /// 打开（或创建）目录下的活动日志。已存在时按追加模式续写，
    /// 并把现有大小作为轮转基准 —— 重启不应该把计数清零。
    pub fn open(dir: impl AsRef<Path>, cfg: RotateConfig) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let path = dir.join(ACTIVE_LOG);
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            dir,
            cfg,
            file,
            written,
        })
    }

    pub fn config(&self) -> &RotateConfig {
        &self.cfg
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 当前活动文件已写字节数。
    pub fn written(&self) -> u64 {
        self.written
    }

    /// 追加一行（调用方负责带换行）。写入前检查是否会超限并触发轮转。
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let bytes = line.as_bytes();
        if self.written > 0 && self.written + bytes.len() as u64 > self.cfg.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(bytes)?;
        self.file.flush()?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// 强制轮转（诊断页「立即归档」按钮用；也用于测试）。
    pub fn rotate(&mut self) -> io::Result<RotateReport> {
        let mut report = RotateReport::default();
        if self.written == 0 {
            return Ok(report);
        }
        self.file.flush()?;
        let active = self.dir.join(ACTIVE_LOG);
        let mut raw = Vec::new();
        File::open(&active)?.read_to_end(&mut raw)?;
        report.archived_bytes = raw.len() as u64;

        // 先腾出最老的槽位，再从大到小搬移
        let slots = self.cfg.archive_slots();
        if slots == 0 {
            // 只保留活动文件：直接清空
            OpenOptions::new().write(true).open(&active)?.set_len(0)?;
            self.written = 0;
            return Ok(report);
        }
        let oldest = self.archive_path(slots);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
            report.deleted.push(oldest);
        }
        for idx in (1..slots).rev() {
            let from = self.archive_path(idx);
            if from.exists() {
                fs::rename(&from, self.archive_path(idx + 1))?;
            }
        }

        // 写入第 1 号归档
        let target = self.archive_path(1);
        if self.cfg.compress {
            let gz = gzip_compress(&raw);
            report.compressed_bytes = gz.len() as u64;
            fs::write(&target, gz)?;
        } else {
            report.compressed_bytes = raw.len() as u64;
            fs::write(&target, &raw)?;
        }
        report.archives.push(target);

        // 清空活动文件（保持同一个 inode 之外无副作用：重新打开更稳）
        OpenOptions::new().write(true).open(&active)?.set_len(0)?;
        self.written = 0;
        Ok(report)
    }

    /// 归档文件路径：`crossmail.1.log.gz` / `crossmail.1.log`
    pub fn archive_path(&self, idx: usize) -> PathBuf {
        let name = if self.cfg.compress {
            format!("crossmail.{idx}.log.gz")
        } else {
            format!("crossmail.{idx}.log")
        };
        self.dir.join(name)
    }

    /// 列出当前目录下所有日志文件（活动 + 归档），按文件名排序。
    pub fn list_files(&self) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name == ACTIVE_LOG || (name.starts_with("crossmail.") && name.contains(".log")) {
                    out.push(path);
                }
            }
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::temp_dir;

    fn small_cfg() -> RotateConfig {
        RotateConfig {
            max_bytes: 200,
            max_files: 3,
            compress: true,
        }
    }

    #[test]
    fn rotates_when_exceeding_limit() {
        let dir = temp_dir("rotate-basic");
        let mut sink = LogSink::open(&dir, small_cfg()).unwrap();
        for i in 0..20 {
            sink.write_line(&format!("line {i} padding padding padding\n"))
                .unwrap();
        }
        let files = sink.list_files().unwrap();
        assert!(files.len() <= 3, "最多保留 3 份，实际 {}", files.len());
        assert!(dir.join("crossmail.log").exists());
        assert!(dir.join("crossmail.1.log.gz").exists(), "应产生压缩归档");
        // 活动文件确实被清空后重写
        assert!(sink.written() <= 200);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn keeps_at_most_configured_files_and_drops_oldest() {
        let dir = temp_dir("rotate-evict");
        let mut sink = LogSink::open(&dir, small_cfg()).unwrap();
        for i in 0..60 {
            sink.write_line(&format!("entry-{i:04} abcdefghijklmnopqrstuvwxyz\n"))
                .unwrap();
        }
        let files = sink.list_files().unwrap();
        assert_eq!(files.len(), 3, "应精确保留 3 份，实际 {files:?}");
        assert!(dir.join("crossmail.2.log.gz").exists());
        assert!(!dir.join("crossmail.3.log.gz").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn archive_content_is_gunzippable() {
        use std::process::Command;
        if Command::new("gunzip").arg("--version").output().is_err() {
            return;
        }
        let dir = temp_dir("rotate-gzip");
        // 上限设大，确保只有最后那次显式 rotate 生效，归档内容才可精确比对
        let cfg = RotateConfig {
            max_bytes: 10_000,
            ..small_cfg()
        };
        let mut sink = LogSink::open(&dir, cfg).unwrap();
        let mut expected = String::new();
        for i in 0..15 {
            let line = format!("payload {i:02} xxxxxxxxxxxxxxxxxxxx\n");
            expected.push_str(&line);
            sink.write_line(&line).unwrap();
        }
        sink.rotate().unwrap();
        let gz = dir.join("crossmail.1.log.gz");
        let out = Command::new("gunzip").arg("-dc").arg(&gz).output().unwrap();
        assert!(out.status.success(), "{:?}", out.stderr);
        let text = String::from_utf8(out.stdout).unwrap();
        assert_eq!(text, expected, "归档内容必须与轮转前活动文件逐字节一致");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn uncompressed_mode_produces_plain_archive() {
        let dir = temp_dir("rotate-plain");
        let cfg = RotateConfig {
            max_bytes: 100,
            max_files: 2,
            compress: false,
        };
        let mut sink = LogSink::open(&dir, cfg).unwrap();
        for i in 0..10 {
            sink.write_line(&format!("plain line {i} aaaaaaaaaaaaaaaaaa\n"))
                .unwrap();
        }
        assert!(dir.join("crossmail.1.log").exists());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn single_file_mode_truncates_instead_of_archiving() {
        let dir = temp_dir("rotate-single");
        let cfg = RotateConfig {
            max_bytes: 80,
            max_files: 1,
            compress: true,
        };
        let mut sink = LogSink::open(&dir, cfg).unwrap();
        for i in 0..10 {
            sink.write_line(&format!("only {i} bbbbbbbbbbbbbbbbbbbb\n"))
                .unwrap();
        }
        let files = sink.list_files().unwrap();
        assert_eq!(files.len(), 1, "{files:?}");
        assert!(sink.written() <= 80);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopening_continues_from_existing_size() {
        let dir = temp_dir("rotate-reopen");
        {
            let mut sink = LogSink::open(&dir, small_cfg()).unwrap();
            sink.write_line("hello world\n").unwrap();
        }
        let sink = LogSink::open(&dir, small_cfg()).unwrap();
        assert_eq!(sink.written(), "hello world\n".len() as u64);
        fs::remove_dir_all(&dir).ok();
    }
}
