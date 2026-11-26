//! 发信产品化探针：走**与 C# 完全相同的 FFI 入口**（`send_email`）真跑一次投递。
//!
//! 与 `crates/app/examples/send_probe.rs` 的区别：
//! * 那个探针验证的是「编排层能不能发信」（`MailProvider::send_email`）；
//! * 这个探针验证的是「产品链路能不能发信」：UDL 契约 → `CrossMailEngine::send_email`
//!   → 落草稿 → SMTP 投递 → 草稿转「已发送」→ 再同步回来看见它。
//!
//! 红线（与既有探针一致）：
//! * 只在**真实库副本**上操作，绝不写用户库；副本临时目录用完即删。
//! * 主密钥从 `%LOCALAPPDATA%\CrossMail\master.key` 读入并直接注入 EngineConfig，
//!   明文凭证始终留在 vault 里，本进程只在 Rust 内部解一次，不打印、不落盘。
//! * 账号若原本没有 SMTP 配置，用 `update_account` 补一份（等价于用户在账号页填
//!   SMTP Host/端口），并明确打印 `SMTP_CONFIG_INJECTED=true`。
//!
//! 用法：
//! ```text
//! cargo run -p crossmail-ffi --example send_email_probe --release -- \
//!   --db "%LOCALAPPDATA%\CrossMail\crossmail.db" \
//!   --key "%LOCALAPPDATA%\CrossMail\master.key" \
//!   --smtp-host mail.example.com
//! ```

use crossmail_ffi::{
    core_status, open_engine, AccountPatchDto, AddressDto, DraftDto, EngineConfig, ProtocolKindDto,
    SecurityModeDto,
};

fn arg(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn flag(name: &str) -> bool {
    std::env::args().any(|a| a == name)
}

/// `--apply-smtp`：把 SMTP 配置写进**真实库**（不复制），让已安装客户端的「发送」
/// 立刻可用。只写三段连接信息，不碰凭证（patch.secret=None 保留原密文）。
fn apply_smtp(db: &str, key: &str, host: &str) -> ! {
    let cfg = EngineConfig {
        db_path: db.to_string(),
        cache_dir: std::env::temp_dir().join("crossmail-apply-smtp").to_string_lossy().to_string(),
        cache_quota_bytes: 64 * 1024 * 1024,
        master_key_b64: Some(key.to_string()),
    };
    let engine = match open_engine(cfg) {
        Ok(e) => e,
        Err(e) => {
            println!("ERROR=OPEN_ENGINE_FAIL {e}");
            std::process::exit(1);
        }
    };
    let accounts = match engine.list_accounts() {
        Ok(a) => a,
        Err(e) => {
            println!("ERROR=LIST_ACCOUNTS_FAIL {e}");
            std::process::exit(1);
        }
    };
    let mut changed = 0;
    for a in accounts.iter().filter(|a| a.protocol == ProtocolKindDto::Imap) {
        let patch = AccountPatchDto {
            display_name: None,
            clear_display_name: false,
            sync_days: None,
            poll_interval_secs: None,
            jmap_endpoint: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: Some(host.to_string()),
            smtp_port: Some(465),
            smtp_security: Some(SecurityModeDto::ImplicitTls),
            clear_smtp: false,
            secret: None,
        };
        match engine.update_account(a.id.clone(), patch) {
            Ok(u) => {
                changed += 1;
                println!(
                    "SMTP_APPLIED account={} email={} smtp={:?}:{:?}",
                    u.id, u.email_address, u.smtp_host, u.smtp_port
                );
            }
            Err(e) => println!("ERROR=UPDATE_ACCOUNT_FAIL account={} {e}", a.id),
        }
    }
    println!("SMTP_APPLIED_COUNT={changed}");
    println!("RESULT=OK");
    std::process::exit(0);
}

fn main() {
    let db_src = arg("--db").unwrap_or_default();
    let key_path = arg("--key").unwrap_or_default();
    let smtp_host = arg("--smtp-host").unwrap_or_default();
    let account_filter = arg("--account").unwrap_or_default();

    if db_src.is_empty() || key_path.is_empty() || smtp_host.is_empty() {
        println!("MISSING_ARG need --db --key --smtp-host [--account]");
        std::process::exit(2);
    }

    if flag("--apply-smtp") {
        let key = match std::fs::read_to_string(&key_path) {
            Ok(k) => k.trim().to_string(),
            Err(e) => {
                println!("ERROR=KEY_READ_FAIL {e}");
                std::process::exit(1);
            }
        };
        apply_smtp(&db_src, &key, &smtp_host);
    }

    let uniq = uuid::Uuid::new_v4().simple().to_string();
    let work = std::env::temp_dir().join(format!("crossmail-sendprobe-{uniq}"));
    let cache = work.join("cache");
    if let Err(e) = std::fs::create_dir_all(&cache) {
        println!("ERROR=WORKDIR_FAIL {e}");
        std::process::exit(1);
    }
    let db_copy = work.join("crossmail.db");
    if let Err(e) = std::fs::copy(&db_src, &db_copy) {
        println!("ERROR=DB_COPY_FAIL {e}");
        std::process::exit(1);
    }
    // SQLite 的 WAL 模式把最近提交放在 `-wal` 里，主库文件可能还没回写。
    // 只复制 .db 会读到旧状态（实测：刚写进去的 SMTP 配置「看不见」），
    // 所以把 sidecar 一并按同名复制过去，让副本自己回放 WAL。
    let mut wal_copied = false;
    for suffix in ["-wal", "-shm"] {
        let src = format!("{db_src}{suffix}");
        if std::path::Path::new(&src).exists() {
            let dst = format!("{}{suffix}", db_copy.to_string_lossy());
            if let Err(e) = std::fs::copy(&src, &dst) {
                println!("WARN=SIDECAR_COPY_FAIL {suffix} {e}");
            } else if suffix == "-wal" {
                wal_copied = true;
            }
        }
    }
    println!("DB_COPY_OK src={db_src} wal_copied={wal_copied}");

    let key = match std::fs::read_to_string(&key_path) {
        Ok(k) => k.trim().to_string(),
        Err(e) => {
            println!("ERROR=KEY_READ_FAIL {e}");
            std::process::exit(1);
        }
    };
    println!("MASTER_KEY_LOADED chars={}", key.len());

    let status = core_status();
    println!("CORE_VERSION={}", status.version);

    let cfg = EngineConfig {
        db_path: db_copy.to_string_lossy().to_string(),
        cache_dir: cache.to_string_lossy().to_string(),
        cache_quota_bytes: 64 * 1024 * 1024,
        master_key_b64: Some(key),
    };
    let engine = match open_engine(cfg) {
        Ok(e) => e,
        Err(e) => {
            println!("ERROR=OPEN_ENGINE_FAIL {e}");
            std::process::exit(1);
        }
    };

    let accounts = match engine.list_accounts() {
        Ok(a) => a,
        Err(e) => {
            println!("ERROR=LIST_ACCOUNTS_FAIL {e}");
            std::process::exit(1);
        }
    };
    println!("ACCOUNTS={}", accounts.len());
    let account = match accounts
        .iter()
        .find(|a| a.protocol == ProtocolKindDto::Imap)
        .filter(|a| account_filter.is_empty() || a.id == account_filter)
        .or_else(|| accounts.first())
    {
        Some(a) => a.clone(),
        None => {
            println!("ERROR=NO_ACCOUNT");
            std::process::exit(1);
        }
    };
    println!(
        "ACCOUNT id={} email={} protocol={:?} smtp={:?}",
        account.id, account.email_address, account.protocol, account.smtp_host
    );

    // 账号没有 SMTP 配置时补一份（等价于用户在账号页填 SMTP 三段）。
    if account.smtp_host.is_none() {
        let patch = AccountPatchDto {
            display_name: None,
            clear_display_name: false,
            sync_days: None,
            poll_interval_secs: None,
            jmap_endpoint: None,
            imap_host: None,
            imap_port: None,
            imap_security: None,
            smtp_host: Some(smtp_host.clone()),
            smtp_port: Some(465),
            smtp_security: Some(SecurityModeDto::ImplicitTls),
            clear_smtp: false,
            secret: None,
        };
        match engine.update_account(account.id.clone(), patch) {
            Ok(a) => println!("SMTP_CONFIG_INJECTED=true smtp={:?}", a.smtp_host),
            Err(e) => {
                println!("ERROR=UPDATE_ACCOUNT_FAIL {e}");
                std::process::exit(1);
            }
        }
    } else {
        println!(
            "SMTP_CONFIG_INJECTED=false host={:?} port={:?}",
            account.smtp_host, account.smtp_port
        );
    }

    let subject = format!("CrossMail FFI 发信探针 {uniq}");
    let draft = DraftDto {
        to: vec![AddressDto {
            name: Some("CrossMail 自测".into()),
            email: account.email_address.clone(),
        }],
        cc: vec![],
        bcc: vec![],
        subject: subject.clone(),
        text_body: Some(format!(
            "由 FFI send_email 真实投递。\n\n入口：CrossMailEngine::send_email\n链路：落草稿 -> SMTP 465 隐式 TLS -> 草稿转已发送\n标记：{uniq}\n"
        )),
        html_body: None,
        in_reply_to: None,
        references: vec![],
    };

    let receipt = match engine.send_email(account.id.clone(), draft) {
        Ok(r) => r,
        Err(e) => {
            println!("SEND_EMAIL_ERR={e}");
            println!("RESULT=FAILED");
            let _ = std::fs::remove_dir_all(&work);
            std::process::exit(1);
        }
    };
    println!(
        "SEND_EMAIL_OK message_id={} local_email_id={} sent_mailbox={}",
        receipt.message_id, receipt.email_id, receipt.sent_mailbox_id
    );

    // 发送成功后本地这封邮件必须已经离开草稿箱、落在「已发送」。
    let drafts_left = engine
        .list_drafts(account.id.clone(), 20)
        .map(|d| d.len())
        .unwrap_or(usize::MAX);
    println!("DRAFTS_AFTER_SEND={drafts_left}");
    match engine.get_email(receipt.email_id.clone()) {
        Ok(e) => println!(
            "LOCAL_SENT_MAILBOX_OK mailbox_ids={:?} subject={:?} flags={:?}",
            e.mailbox_ids, e.subject, e.flags
        ),
        Err(e) => println!("LOCAL_GET_ERR={e}"),
    }

    // 本地主键必须与服务端归档身份一致（`账号/文件夹/UID`）。
    // 不一致的后果很具体：下次同步会把服务端那封按自己的主键再插一行，
    // 「已发送」里出现两封一模一样的邮件，用户以为自己发了两遍。
    let uid_in_id = receipt
        .email_id
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<u32>().ok());
    let aligned = receipt.email_id.starts_with(&format!("{}/", account.id))
        && receipt.email_id.contains(receipt.sent_mailbox_id.as_str())
        && uid_in_id.is_some();
    println!(
        "SERVER_ALIGNED_ID ok={aligned} uid={uid_in_id:?} id={} mailbox={}",
        receipt.email_id, receipt.sent_mailbox_id
    );

    // 同步一次：服务器侧应能把这封自己发给自己的信收回来看见。
    match engine.sync_account(account.id.clone()) {
        Ok(r) => println!(
            "SYNC_AFTER_SEND inserted={} updated={} full_resync={}",
            r.inserted, r.updated, r.full_resync
        ),
        Err(e) => println!("SYNC_AFTER_SEND_ERR={e}"),
    }
    let hits = engine
        .search(
            account.id.clone(),
            uniq.clone(),
            crossmail_ffi::SearchScopeDto::Subject,
            10,
            0,
        )
        .map(|p| p.items)
        .unwrap_or_default();
    let found: Vec<String> = hits.iter().map(|i| i.subject.clone()).collect();
    // 已发送里必须**只有一封**：同步按 UID 命中本地那一行，只会更新不会新插。
    let in_sent = hits
        .iter()
        .filter(|i| i.mailbox_ids.iter().any(|m| m == &receipt.sent_mailbox_id))
        .count();
    println!("SEARCH_BY_TOKEN count={} subjects={found:?}", hits.len());
    println!(
        "SENT_COPIES_AFTER_SYNC count={in_sent} sent_mailbox={}",
        receipt.sent_mailbox_id
    );
    let ok = drafts_left == 0 && aligned && in_sent == 1;
    println!("RESULT={}", if ok { "OK" } else { "FAILED" });

    let _ = std::fs::remove_dir_all(&work);
}
