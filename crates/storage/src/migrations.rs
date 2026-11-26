//! 版本化 schema 迁移（STO-004）：增量执行、事务保护、不丢用户数据。

pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
    /// 需要**重建表**的迁移必须置 true：SQLite 的 `DROP TABLE` 在外键约束开启时会
    /// 触发子表的 `ON DELETE CASCADE`，把 `email_mailboxes` / `email_flags` /
    /// `attachments` 一起清空。重建期间必须临时关掉外键（该 PRAGMA 在事务内改无效，
    /// 所以由执行器在 BEGIN 之前切换），提交前再用 `foreign_key_check` 兜一道。
    pub rebuilds_table: bool,
}

/// 追加迁移时只在数组尾部新增，永不修改已发布的迁移。
pub fn all() -> Vec<Migration> {
    vec![
        Migration {
            version: 1,
            name: "initial_schema",
            rebuilds_table: false,
            sql: r#"
CREATE TABLE IF NOT EXISTS accounts (
    id                  TEXT PRIMARY KEY,
    email_address       TEXT NOT NULL,
    display_name        TEXT,
    protocol            TEXT NOT NULL,
    config_json         TEXT NOT NULL DEFAULT '{}',
    sync_days           INTEGER NOT NULL DEFAULT 30,
    poll_interval_secs  INTEGER NOT NULL DEFAULT 300,
    created_at          TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS mailboxes (
    id              TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name            TEXT NOT NULL,
    parent_id       TEXT,
    role            TEXT NOT NULL,
    total           INTEGER NOT NULL DEFAULT 0,
    unread          INTEGER NOT NULL DEFAULT 0,
    uid_validity    INTEGER,
    highest_modseq  INTEGER,
    state           TEXT
);
CREATE INDEX IF NOT EXISTS idx_mailboxes_account ON mailboxes(account_id);
CREATE TABLE IF NOT EXISTS emails (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    subject      TEXT NOT NULL,
    from_json    TEXT NOT NULL DEFAULT '[]',
    to_json      TEXT NOT NULL DEFAULT '[]',
    preview      TEXT NOT NULL DEFAULT '',
    received_at  TEXT NOT NULL,
    size         INTEGER NOT NULL DEFAULT 0,
    state        TEXT,
    modseq       INTEGER,
    uid          INTEGER,
    blob_id      TEXT,
    body_path    TEXT,
    UNIQUE(account_id, uid)
);
CREATE INDEX IF NOT EXISTS idx_emails_account_received ON emails(account_id, received_at DESC);
CREATE TABLE IF NOT EXISTS email_mailboxes (
    email_id    TEXT NOT NULL REFERENCES emails(id) ON DELETE CASCADE,
    mailbox_id  TEXT NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    PRIMARY KEY (email_id, mailbox_id)
);
CREATE TABLE IF NOT EXISTS email_flags (
    email_id  TEXT NOT NULL REFERENCES emails(id) ON DELETE CASCADE,
    flag      TEXT NOT NULL,
    PRIMARY KEY (email_id, flag)
);
CREATE TABLE IF NOT EXISTS attachments (
    id            TEXT PRIMARY KEY,
    email_id      TEXT NOT NULL REFERENCES emails(id) ON DELETE CASCADE,
    name          TEXT,
    content_type  TEXT NOT NULL,
    size          INTEGER NOT NULL DEFAULT 0,
    blob_id       TEXT,
    part_id       TEXT,
    inline        INTEGER NOT NULL DEFAULT 0,
    cid           TEXT
);
CREATE TABLE IF NOT EXISTS sync_state (
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_id   TEXT NOT NULL,
    state        TEXT,
    modseq       INTEGER,
    last_sync_at TEXT,
    PRIMARY KEY (account_id, mailbox_id)
);
CREATE TABLE IF NOT EXISTS offline_queue (
    id          TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    kind_json   TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT
);
CREATE INDEX IF NOT EXISTS idx_queue_order ON offline_queue(account_id, created_at);
"#,
        },
        Migration {
            version: 2,
            name: "cache_quota_and_search_index",
            rebuilds_table: false,
            sql: r#"
CREATE TABLE IF NOT EXISTS cache_files (
    path         TEXT PRIMARY KEY,
    size         INTEGER NOT NULL,
    last_access  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_emails_subject ON emails(account_id, subject);
CREATE INDEX IF NOT EXISTS idx_queue_account ON offline_queue(account_id, attempts);
"#,
        },
        Migration {
            version: 3,
            name: "email_full_metadata_and_query_indexes",
            rebuilds_table: false,
            // S4-01/S4-02：补齐 Email 模型里原先落不了库的字段（cc/bcc/reply_to/message_id/
            // sent_at/thread_id），并为分页、按文件夹过滤、搜索与 LRU 淘汰补索引。
            sql: r#"
ALTER TABLE emails ADD COLUMN cc_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE emails ADD COLUMN bcc_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE emails ADD COLUMN reply_to_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE emails ADD COLUMN message_id TEXT;
ALTER TABLE emails ADD COLUMN sent_at TEXT;
ALTER TABLE emails ADD COLUMN thread_id TEXT;
CREATE INDEX IF NOT EXISTS idx_email_mailboxes_mailbox ON email_mailboxes(mailbox_id, email_id);
CREATE INDEX IF NOT EXISTS idx_email_flags_flag ON email_flags(flag, email_id);
CREATE INDEX IF NOT EXISTS idx_attachments_email ON attachments(email_id);
CREATE INDEX IF NOT EXISTS idx_cache_lru ON cache_files(last_access);
CREATE INDEX IF NOT EXISTS idx_offline_queue_account ON offline_queue(account_id, created_at, attempts);
"#,
        },
        Migration {
            version: 4,
            name: "secret_vault",
            rebuilds_table: false,
            // S4-04：凭证密文仓。只存密文（sealed），明文密钥由业务层加解密，
            // 存储层不认识密钥也不认识算法，避免把密码学决策散落进 SQL 层。
            sql: r#"
CREATE TABLE IF NOT EXISTS secrets (
    alias       TEXT PRIMARY KEY,
    sealed      TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
"#,
        },
        Migration {
            version: 5,
            name: "external_resource_allowlist",
            rebuilds_table: false,
            // S6-02（SEC-003）：外部资源放行白名单。「单次放行」用 expires_at 表达
            // 短期有效，「永久放行」expires_at 为 NULL。「单次」在业务层被消费后
            // 立即删除，而不是在这里做计数器——计数需要事务，删行不需要。
            sql: r#"
CREATE TABLE IF NOT EXISTS external_allowlist (
    sender      TEXT PRIMARY KEY,
    scope       TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    expires_at  TEXT
);
CREATE INDEX IF NOT EXISTS idx_allowlist_expires ON external_allowlist(expires_at);
"#,
        },
        Migration {
            version: 6,
            name: "drop_email_uid_account_unique",
            // `UNIQUE(account_id, uid)` 是个**错误的约束**：IMAP 的 UID 只在单个
            // 文件夹内唯一，跨文件夹完全可能重号（每个文件夹的 UID 序列都从 1 开始）。
            // 只要两个文件夹各有一封 uid=1 的信，第二次写入就撞约束——最典型的触发
            // 场景是发信后把副本 APPEND 进「已发送」：Sent 的第一封 uid=1，收件箱里
            // 的种子邮件也是 uid=1，于是整次发送报「本地存储错误」。
            //
            // 记录的真实身份是主键 `账号/文件夹/UID`（`emails.id`），库层面不需要
            // 再对 uid 加唯一约束；按文件夹取 uid 的查询都带 mailbox_id 条件，不受影响。
            //
            // 重建表是 SQLite 唯一能去掉表级约束的办法，且必须关外键（见
            // `Migration::rebuilds_table` 的注释），否则 DROP 会把子表一起级联清空。
            rebuilds_table: true,
            sql: r#"
CREATE TABLE emails_v6 (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    subject      TEXT NOT NULL,
    from_json    TEXT NOT NULL DEFAULT '[]',
    to_json      TEXT NOT NULL DEFAULT '[]',
    preview      TEXT NOT NULL DEFAULT '',
    received_at  TEXT NOT NULL,
    size         INTEGER NOT NULL DEFAULT 0,
    state        TEXT,
    modseq       INTEGER,
    uid          INTEGER,
    blob_id      TEXT,
    body_path    TEXT,
    cc_json      TEXT NOT NULL DEFAULT '[]',
    bcc_json     TEXT NOT NULL DEFAULT '[]',
    reply_to_json TEXT NOT NULL DEFAULT '[]',
    message_id   TEXT,
    sent_at      TEXT,
    thread_id    TEXT
);
INSERT INTO emails_v6 (
    id,account_id,subject,from_json,to_json,preview,received_at,size,state,modseq,
    uid,blob_id,body_path,cc_json,bcc_json,reply_to_json,message_id,sent_at,thread_id)
SELECT
    id,account_id,subject,from_json,to_json,preview,received_at,size,state,modseq,
    uid,blob_id,body_path,cc_json,bcc_json,reply_to_json,message_id,sent_at,thread_id
FROM emails;
DROP TABLE emails;
ALTER TABLE emails_v6 RENAME TO emails;
CREATE INDEX IF NOT EXISTS idx_emails_account_received ON emails(account_id, received_at DESC);
CREATE INDEX IF NOT EXISTS idx_emails_subject ON emails(account_id, subject);
"#,
        },
        Migration {
            version: 7,
            name: "client_mail_rules",
            rebuilds_table: false,
            // UI-13：客户端收信规则。规则在**客户端**匹配并触发移动（服务器端 Sieve
            // 各家支持参差，客户端规则对 IMAP/JMAP 一视同仁）。匹配字段 from/to/subject
            // + 关键词，命中即把邮件移入 target_mailbox_id。
            sql: r#"
CREATE TABLE IF NOT EXISTS rules (
    id                 TEXT PRIMARY KEY,
    account_id         TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name               TEXT NOT NULL,
    field              TEXT NOT NULL,
    keyword            TEXT NOT NULL,
    target_mailbox_id  TEXT NOT NULL,
    enabled            INTEGER NOT NULL DEFAULT 1,
    created_at         TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_rules_account ON rules(account_id);
"#,
        },
    ]
}
