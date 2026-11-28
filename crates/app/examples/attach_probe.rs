//! 附件发信探针（示例二进制，仅用于真机验证，不进入产品路径）。
//!
//! 排查「带附件发送失败」：分阶段定位失败点——
//!   S1 vault 解密 → S2 build_backend(JMAP) → S3 blob 上传（uploadUrl 实测）
//!   → S4 带附件 EmailSubmission 真投递（收件人=自己，主题标注探针）。
//!
//! 红线与 send_probe 相同：凭据只在内存，不打印、不落盘。
//! 用法：
//! ```text
//! cargo run -p crossmail-app --example attach_probe --release -- \
//!   --db <crossmail.db> --key <crossmail.key> [--to self@addr]
//! ```

use std::sync::{Arc, Mutex};

use crossmail_app::{build_backend, Backend};
use crossmail_core::model::{Address, OutgoingAttachment, OutgoingMessage};
use crossmail_core::provider::MailProvider;
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

fn main() {
    let db_path = arg("--db").unwrap_or_default();
    let key_path = arg("--key").unwrap_or_default();
    let to = arg("--to").unwrap_or_default();
    let size_kb: usize = arg("--size-kb").and_then(|v| v.parse().ok()).unwrap_or(1);
    let file = arg("--file").unwrap_or_default();
    let subject = arg("--subject")
        .unwrap_or_else(|| String::from("CrossMail 附件探针（可删除）"));
    if db_path.is_empty() || key_path.is_empty() {
        eprintln!("MISSING_ARG need --db --key [--to]");
        std::process::exit(2);
    }

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let outcome: Result<(), String> = rt.block_on(async {
        // S1: vault 解密
        let mut db = Database::open(&db_path).map_err(|e| format!("DB_OPEN_FAIL {e}"))?;
        db.migrate().map_err(|e| format!("DB_MIGRATE_FAIL {e}"))?;
        let db = Arc::new(Mutex::new(db));
        let exported = std::fs::read_to_string(&key_path)
            .map_err(|e| format!("KEY_READ_FAIL {e}"))?
            .trim()
            .to_string();
        let codec = Arc::new(
            ChaChaSecretCodec::from_exported(&exported)
                .map_err(|e| format!("KEY_DECODE_FAIL {e}"))?,
        );
        let vault = Arc::new(SecretVault::new(Arc::clone(&db), codec));
        let accounts = AccountService::new(Arc::clone(&db), vault);
        let account = accounts
            .get(&accounts.list().map_err(|e| format!("LIST_FAIL {e}"))?[0].id)
            .map_err(|e| format!("ACCOUNT_LOAD_FAIL {e}"))?;
        let secret = accounts
            .resolve_secret(&account)
            .map_err(|e| format!("VAULT_DECRYPT_FAIL {e}"))?;
        println!("S1_VAULT_OK account={} chars={}", account.email_address, secret.len());
        let from = account.email_address.clone();
        let to_addr = if to.is_empty() { from.clone() } else { to.clone() };

        // S2: build_backend
        let provider = match build_backend(&account, &secret)
            .await
            .map_err(|e| format!("BUILD_BACKEND_FAIL {e}"))?
        {
            Backend::Jmap(p) => p,
            Backend::Imap(_) => return Err("NOT_JMAP_ACCOUNT（探针仅针对 JMAP 链路）".into()),
        };
        println!("S2_BACKEND_OK protocol=jmap");

        // S3: blob 上传（--file 指定真实文件，否则生成 size_kb 测试载荷）
        let (payload, att_name, att_ct): (Vec<u8>, String, String) = if file.is_empty() {
            (
                (0..(size_kb * 1024) as u32).map(|i| (i % 251) as u8).collect(),
                String::from("attach-probe.bin"),
                String::from("application/octet-stream"),
            )
        } else {
            let p = std::path::Path::new(&file);
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| file.clone());
            let data = std::fs::read(p).map_err(|e| format!("FILE_READ_FAIL {e}"))?;
            (
                data,
                name,
                mime_guess::from_path(p)
                    .first_or_octet_stream()
                    .to_string(),
            )
        };
        let blob = provider
            .upload_blob(&payload, &att_ct)
            .await
            .map_err(|e| format!("S3_UPLOAD_FAIL {e}"))?;
        println!(
            "S3_UPLOAD_OK blob_id={} size={} file={}",
            blob.id,
            blob.size,
            if file.is_empty() { "<generated>" } else { &file }
        );

        // S4: 带附件真投递（收件人=自己）
        let att = OutgoingAttachment {
            name: att_name,
            content_type: att_ct,
            data: payload.clone(),
        };
        let mut msg = OutgoingMessage::new(
            account.id.clone(),
            Address { name: Some("CrossMail Attach Probe".into()), email: from.clone() },
            vec![Address { name: None, email: to_addr.clone() }],
            subject,
        );
        msg.text_body = Some("附件链路探针自动投递，收到即整链路正常，可删除。".into());
        msg.attachments = vec![att];
        let receipt = provider
            .send_email(msg)
            .await
            .map_err(|e| format!("S4_SEND_FAIL {e}"))?;
        println!("S4_SEND_OK message_id={}", receipt.message_id);
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
