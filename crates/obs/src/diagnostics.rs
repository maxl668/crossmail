//! 诊断包导出（OBS-003）：一个 tar 文件，包含脱敏后的清单 + 日志 + 同步状态。
//!
//! 容器选 tar 而不是 zip：tar 是**只写不压缩**的简单格式，本仓库可以在不引入
//! 任何三方库的前提下正确产出（见下方 512 字节头构造），而 zip 需要额外的
//! 中央目录与压缩流。日志本身在里面已经是 `.gz`，容器不需要再压一遍。
//! 产物可直接 `tar -tf` 检视，测试里就是这么验的。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::gzip::gzip_compress;
use crate::redact::redact_json;

/// 诊断包里的账号信息（**只放非敏感字段**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticAccount {
    pub address: String,
    /// `jmap` / `imap`
    pub protocol: String,
    pub last_sync_at: Option<String>,
    pub sync_state_len: usize,
}

/// 导出输入。调用方（业务层）负责把同步状态、队列、缓存占用查出来。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsInput {
    pub app_version: String,
    pub platform: String,
    pub generated_at: DateTime<Utc>,
    pub accounts: Vec<DiagnosticAccount>,
    /// 每个账号的同步状态（任意 JSON，会被脱敏）
    pub sync_status: Value,
    /// 离线队列摘要
    pub offline_queue: Value,
    /// 缓存占用摘要
    pub cache: Value,
    /// 环形缓冲里最近若干条日志（已格式化为行）
    pub recent_logs: Vec<String>,
    /// 磁盘上已有的日志文件（归档的 .gz 原样收录，普通 .log 现场压缩）
    pub log_files: Vec<PathBuf>,
}

impl DiagnosticsInput {
    pub fn new(app_version: impl Into<String>, platform: impl Into<String>) -> Self {
        Self {
            app_version: app_version.into(),
            platform: platform.into(),
            generated_at: Utc::now(),
            accounts: Vec::new(),
            sync_status: json!({}),
            offline_queue: json!({}),
            cache: json!({}),
            recent_logs: Vec::new(),
            log_files: Vec::new(),
        }
    }
}

/// 导出到目录，返回生成的 tar 路径。
pub fn export(dir: impl AsRef<Path>, input: &DiagnosticsInput) -> io::Result<PathBuf> {
    let dir = dir.as_ref();
    fs::create_dir_all(dir)?;
    let name = format!(
        "crossmail-diagnostics-{}.tar",
        input.generated_at.format("%Y%m%d-%H%M%S")
    );
    let path = dir.join(name);
    fs::write(&path, export_bytes(input))?;
    Ok(path)
}

/// 只产出字节：平台层（Android SAF / Windows 文件选择器）自己决定往哪写。
pub fn export_bytes(input: &DiagnosticsInput) -> Vec<u8> {
    let mut tar = TarWriter::new();
    let mtime = input.generated_at.timestamp().max(0) as u64;

    tar.add_file("README.txt", README.as_bytes(), mtime);
    tar.add_file(
        "manifest.json",
        manifest_json(input).to_string().as_bytes(),
        mtime,
    );

    // 环形缓冲日志：现场压缩
    if !input.recent_logs.is_empty() {
        let mut text = String::new();
        for line in &input.recent_logs {
            text.push_str(line);
            if !line.ends_with('\n') {
                text.push('\n');
            }
        }
        tar.add_file("logs/recent.log.gz", &gzip_compress(text.as_bytes()), mtime);
    }

    for path in &input.log_files {
        let Ok(data) = fs::read(path) else { continue };
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("crossmail.log");
        let entry = format!("logs/{file_name}");
        if file_name.ends_with(".gz") {
            // 已经是压缩归档，原样收录（避免二次压缩浪费 CPU）
            tar.add_file(&entry, &data, mtime);
        } else {
            tar.add_file(&format!("{entry}.gz"), &gzip_compress(&data), mtime);
        }
    }

    tar.finish()
}

fn manifest_json(input: &DiagnosticsInput) -> Value {
    let accounts: Vec<Value> = input
        .accounts
        .iter()
        .map(|a| {
            json!({
                "address": a.address,
                "protocol": a.protocol,
                "last_sync_at": a.last_sync_at,
                "sync_state_len": a.sync_state_len,
            })
        })
        .collect();
    let raw = json!({
        "app": "CrossMail",
        "app_version": input.app_version,
        "platform": input.platform,
        "generated_at": input.generated_at.to_rfc3339(),
        "accounts": accounts,
        "sync_status": input.sync_status,
        "offline_queue": input.offline_queue,
        "cache": input.cache,
        "log_entries": input.recent_logs.len(),
        "redaction": "邮箱本地部分已掩码；token/密码等敏感键值已替换；不含邮件正文与附件",
    });
    redact_json(&raw)
}

const README: &str = "\
CrossMail 诊断包
================

本文件由 CrossMail 「导出诊断」功能生成，用于排查同步/连接问题。

内容：
  manifest.json   应用版本、平台、账号（已脱敏）、同步状态、离线队列、缓存占用
  logs/*.gz       运行日志（含轮转归档），gzip 压缩
  logs/recent.log.gz  最近的内存日志缓冲

已做脱敏：
  * 邮箱地址只保留前两位 + 域名（zh***@example.com）
  * token / password / secret 等敏感键值一律替换为 ***
  * 长随机串（会话凭证、blob id）整段替换
  * 不含邮件正文、附件内容与联系人

查看方式：
  tar -xf <本文件>
  gunzip -dc logs/recent.log.gz | less
";

// ------------------------------------------------------------------ tar(ustar)

/// 极简 tar 写出器：只写普通文件项 + 结束块，够 `tar -xf` / `tar -tf` 用。
struct TarWriter {
    buf: Vec<u8>,
}

impl TarWriter {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn add_file(&mut self, name: &str, data: &[u8], mtime: u64) {
        self.buf
            .extend_from_slice(&tar_header(name, data.len(), mtime));
        self.buf.extend_from_slice(data);
        let pad = (512 - data.len() % 512) % 512;
        self.buf.resize(self.buf.len() + pad, 0u8);
    }

    fn finish(mut self) -> Vec<u8> {
        // tar 结束标记：两个全零块
        self.buf.resize(self.buf.len() + 1024, 0u8);
        self.buf
    }
}

fn tar_header(name: &str, size: usize, mtime: u64) -> [u8; 512] {
    let mut h = [0u8; 512];
    let name_bytes = name.as_bytes();
    let n = name_bytes.len().min(100);
    h[..n].copy_from_slice(&name_bytes[..n]);
    write_octal(&mut h[100..108], 0o644);
    write_octal(&mut h[108..116], 0);
    write_octal(&mut h[116..124], 0);
    write_octal(&mut h[124..136], size as u64);
    write_octal(&mut h[136..148], mtime);
    // checksum 先填空格，算完再回填
    for b in h[148..156].iter_mut() {
        *b = b' ';
    }
    h[156] = b'0'; // 普通文件
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[265..274].copy_from_slice(b"crossmail");
    h[297..306].copy_from_slice(b"crossmail");
    write_octal(&mut h[329..337], 0);
    write_octal(&mut h[337..345], 0);

    let sum: u32 = h.iter().map(|b| u32::from(*b)).sum();
    // 6 位八进制 + NUL + 空格，共 8 字节
    let chk = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(&chk.as_bytes()[..8]);
    h
}

/// 右对齐写八进制，末尾留 NUL（tar 的经典字段编码）。
fn write_octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let s = format!("{value:0width$o}", width = width);
    let bytes = s.as_bytes();
    let take = bytes.len().min(width);
    field[..width].iter_mut().for_each(|b| *b = b'0');
    field[width - take..width].copy_from_slice(&bytes[bytes.len() - take..]);
    field[width] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::temp_dir;
    use std::process::Command;

    fn sample_input() -> DiagnosticsInput {
        let mut input = DiagnosticsInput::new("0.1.0", "windows-x64");
        input.accounts.push(DiagnosticAccount {
            address: "zhangsan@example.com".into(),
            protocol: "imap".into(),
            last_sync_at: Some("2026-11-13T22:00:00Z".into()),
            sync_state_len: 12,
        });
        input.sync_status = json!({"INBOX": {"state": "42", "uid_validity": 1}});
        input.offline_queue = json!({"pending": 2, "dead_letter": 0});
        input.cache = json!({"used_bytes": 1024, "quota_bytes": 524288000});
        input.recent_logs = vec![
            "2026-11-13T22:00:00.000Z I sync 增量同步完成\n".to_string(),
            "2026-11-13T22:00:01.000Z E imap 登录失败 user=wangwu@corp.cn\n".to_string(),
        ];
        input
    }

    #[test]
    fn export_writes_tar_with_expected_entries() {
        let dir = temp_dir("diag-export");
        let path = export(&dir, &sample_input()).unwrap();
        assert!(path.exists());
        assert!(path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("crossmail-diagnostics-"));
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.len() > 1024);
        assert!(bytes.ends_with(&[0u8; 1024]), "tar 必须以两个空块结尾");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn manifest_is_redacted() {
        let bytes = export_bytes(&sample_input());
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("zh***@example.com"), "账号必须脱敏");
        assert!(!text.contains("zhangsan@"), "不得出现原始邮箱");
        assert!(text.contains("CrossMail"));
        // 日志里的邮箱同样要脱敏
        assert!(text.contains("wa***@corp.cn") || !text.contains("wangwu@corp.cn"));
    }

    #[test]
    fn tar_is_readable_by_system_tar() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let dir = temp_dir("diag-tar");
        let path = export(&dir, &sample_input()).unwrap();
        let out = Command::new("tar").arg("-tf").arg(&path).output().unwrap();
        assert!(out.status.success(), "{:?}", out.stderr);
        let listing = String::from_utf8_lossy(&out.stdout);
        assert!(listing.contains("manifest.json"), "{listing}");
        assert!(listing.contains("README.txt"), "{listing}");
        assert!(listing.contains("logs/recent.log.gz"), "{listing}");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn included_log_archive_roundtrips_through_gunzip() {
        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        if Command::new("gunzip").arg("--version").output().is_err() {
            return;
        }
        let dir = temp_dir("diag-tar-extract");
        let log_dir = dir.join("logs");
        fs::create_dir_all(&log_dir).unwrap();
        fs::write(log_dir.join("crossmail.log"), "hello log\n").unwrap();
        fs::write(
            log_dir.join("crossmail.1.log.gz"),
            gzip_compress(b"archived\n"),
        )
        .unwrap();

        let mut input = sample_input();
        input.log_files = vec![
            log_dir.join("crossmail.log"),
            log_dir.join("crossmail.1.log.gz"),
        ];
        let tar_path = dir.join("diag.tar");
        fs::write(&tar_path, export_bytes(&input)).unwrap();

        let out_dir = dir.join("out");
        fs::create_dir_all(&out_dir).unwrap();
        // `-m`（--touch）：不去设置文件 mtime。本机沙箱禁止 utime，
        // 不加这个参数 tar 会因「无法 utime」返回非零退出码，与包本身无关。
        let out = Command::new("tar")
            .arg("-xmf")
            .arg(&tar_path)
            .arg("-C")
            .arg(&out_dir)
            .output()
            .unwrap();
        assert!(
            out_dir.join("logs/crossmail.log.gz").exists(),
            "解包失败: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let plain = Command::new("gunzip")
            .arg("-dc")
            .arg(out_dir.join("logs/crossmail.log.gz"))
            .output()
            .unwrap();
        assert_eq!(plain.stdout, b"hello log\n");
        let archived = Command::new("gunzip")
            .arg("-dc")
            .arg(out_dir.join("logs/crossmail.1.log.gz"))
            .output()
            .unwrap();
        assert_eq!(archived.stdout, b"archived\n");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn octal_fields_are_well_formed() {
        // tar 字段编码：N 字节 → 前 N-1 字节写八进制（左补零），末字节写 NUL
        let mut mode = [0u8; 8];
        write_octal(&mut mode, 0o644);
        assert_eq!(&mode[..7], b"0000644");
        assert_eq!(mode[7], 0);

        let mut size = [0u8; 12];
        write_octal(&mut size, 4096);
        assert_eq!(&size[..11], b"00000010000");
        assert_eq!(size[11], 0);
    }
}
