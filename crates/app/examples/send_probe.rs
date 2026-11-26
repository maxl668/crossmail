//! 发信链路探针（示例二进制，仅用于真机验证，不进入产品路径）。
//!
//! 背景：FFI 的 36 个方法里**没有** `send_email`，所以 C# 侧根本无法发信。核心侧
//! `MailProvider::send_email` + IMAP/SMTP 实现都在。该探针要同时证明两件事：
//!
//! 1. `build_backend`（生产编排入口）装配的 provider **没有 SMTP 通道** ——
//!    直接调用会返回「该账号未配置 SMTP 发信」，这是编排层缺口，不是网络问题；
//! 2. 手工 `with_smtp(...)` 后，SMTP 465 隐式 TLS 认证 + DATA 真投递成功。
//!
//! 因此「发信不可用」= 三层缺口：账号缺 SMTP 配置 / 编排层未装配 / FFI 未暴露。
//!
//! 红线：凭据从本地 vault 解密（主密钥来自 master.key），**不读环境变量、
//! 不打印明文、不落盘**。为免与运行中的客户端抢锁，建议指向库副本。
//!
//! 用法：
//! ```text
//! cargo run -p crossmail-app --example send_probe --release -- \
//!   --db <crossmail.db> --key <master.key> --account <id> \
//!   --smtp-host mail.example.com --to user@example.com
//! ```

use std::sync::{Arc, Mutex};

use crossmail_app::{build_backend, Backend};
use crossmail_core::model::{Address, OutgoingMessage, SecurityMode};
use crossmail_core::provider::MailProvider;
use crossmail_imap::provider::{ImapProvider, ImapProviderConfig, SmtpAuth, TcpSmtpConnector};
use crossmail_imap::transport::Endpoint;
use crossmail_service::credential::{ChaChaSecretCodec, SecretVault};
use crossmail_service::AccountService;
use crossmail_storage::Database;

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn build_message(account_id: &str, from: &str, to: &str, subject: &str) -> OutgoingMessage {
    let mut msg = OutgoingMessage::new(
        account_id,
        Address {
            name: Some("CrossMail Probe".into()),
            email: from.to_string(),
        },
        vec![Address {
            name: None,
            email: to.to_string(),
        }],
        subject,
    );
    msg.text_body = Some(format!(
        "CrossMail 核心发信探针自动投递。\n\n链路：本地 vault 解密 -> SMTP 465 隐式 TLS -> AUTH -> DATA\n收件人：{to}\n"
    ));
    msg
}

fn main() {
    let db_path = arg("--db").unwrap_or_default();
    let key_path = arg("--key").unwrap_or_default();
    let account_id = arg("--account").unwrap_or_default();
    let smtp_host = arg("--smtp-host").unwrap_or_default();
    let to = arg("--to").unwrap_or_default();
    let subject = arg("--subject")
        .unwrap_or_else(|| "CrossMail 核心发信探针（SMTP 465 隐式 TLS）".to_string());

    if db_path.is_empty() || key_path.is_empty() || account_id.is_empty() || smtp_host.is_empty() {
        eprintln!("MISSING_ARG need --db --key --account --smtp-host [--to]");
        std::process::exit(2);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let outcome: Result<(), String> = rt.block_on(async {
        // 1) 打开库副本，用主密钥还原 codec，从 vault 取明文（只在内存）。
        let mut db = Database::open(&db_path).map_err(|e| format!("DB_OPEN_FAIL {e}"))?;
        db.migrate().map_err(|e| format!("DB_MIGRATE_FAIL {e}"))?;
        let db = Arc::new(Mutex::new(db));

        let exported = std::fs::read_to_string(&key_path)
            .map_err(|e| format!("KEY_READ_FAIL {e}"))?
            .trim()
            .to_string();
        let codec = Arc::new(
            ChaChaSecretCodec::from_exported(&exported).map_err(|e| format!("KEY_DECODE_FAIL {e}"))?,
        );
        let vault = Arc::new(SecretVault::new(Arc::clone(&db), codec));
        let accounts = AccountService::new(Arc::clone(&db), vault);

        let account = accounts
            .get(&account_id)
            .map_err(|e| format!("ACCOUNT_LOAD_FAIL {e}"))?;
        let secret = accounts
            .resolve_secret(&account)
            .map_err(|e| format!("VAULT_DECRYPT_FAIL {e}"))?;
        println!("VAULT_DECRYPT_OK account={} chars={}", account.id, secret.len());
        let from = account.email_address.clone();
        let to_addr = if to.is_empty() { from.clone() } else { to.clone() };

        // 2) 生产编排入口发出的 provider：预期没有 SMTP 通道。
        let backend = build_backend(&account, &secret)
            .await
            .map_err(|e| format!("BUILD_BACKEND_FAIL {e}"))?;
        let backend_err = match backend {
            Backend::Imap(p) => p
                .send_email(build_message(&account.id, &from, &to_addr, &subject))
                .await
                .err()
                .map(|e| e.to_string()),
            Backend::Jmap(p) => p
                .send_email(build_message(&account.id, &from, &to_addr, &subject))
                .await
                .err()
                .map(|e| e.to_string()),
        };
        match &backend_err {
            Some(e) => println!("ORCHESTRATION_SEND_ERR={e}"),
            None => {
                // 编排层已能发信，说明 SMTP 装配到位，无需再用手工路径复验一遍
                // （少发一封，避免给测试账号堆无意义的邮件）。
                println!("ORCHESTRATION_SEND_OK=true");
                return Ok(());
            }
        }

        // 3) 手工装配 SMTP 后重发：这条才是「核心是否真能发信」的判据。
        let imap_cfg = account
            .imap
            .as_ref()
            .ok_or_else(|| "NOT_IMAP_ACCOUNT".to_string())?;
        let ep = Endpoint::new(imap_cfg.host.clone(), imap_cfg.port);
        let pcfg = ImapProviderConfig::new(
            account.id.clone(),
            from.clone(),
            from.clone(),
            secret.clone(),
        );
        let provider = ImapProvider::connect(&ep, &from, &secret, pcfg)
            .await
            .map_err(|e| format!("IMAP_CONNECT_FAIL {e}"))?;
        println!("IMAP_CONNECT_OK host={} port={}", imap_cfg.host, imap_cfg.port);

        let provider = provider.with_smtp(SmtpAuth {
            connector: Box::new(TcpSmtpConnector::new(smtp_host.clone(), SecurityMode::ImplicitTls)),
            username: from.clone(),
            secret: secret.clone(),
        });

        let receipt = provider
            .send_email(build_message(&account.id, &from, &to_addr, &subject))
            .await
            .map_err(|e| format!("SEND_FAIL {e}"))?;
        println!(
            "SEND_OK message_id={} sent_at={}",
            receipt.message_id,
            receipt.sent_at.to_rfc3339()
        );
        Ok(())
    });

    match outcome {
        Ok(()) => println!("RESULT=OK"),
        Err(err) => {
            println!("ERROR={err}");
            println!("RESULT=FAILED");
            std::process::exit(1);
        }
    }
}
