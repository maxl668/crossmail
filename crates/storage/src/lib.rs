//! 本地存储（STO-001~004）：SQLite + WAL、版本化迁移、元数据 CRUD、离线队列、缓存配额。

pub mod migrations;

use chrono::{DateTime, Utc};
use crossmail_core::{
    Account, Address, Attachment as CrossAttachment, Email, Flag, Id, ImapConfig, JmapConfig,
    MailError, MailResult, Mailbox, MailboxRole, OfflineOperation, ProtocolKind, SmtpConfig,
};
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

fn db_err(e: rusqlite::Error) -> MailError {
    MailError::Storage(e.to_string())
}

/// 库里有没有悬空外键引用（重建表时最容易把子表指到不存在的行上）。
fn has_foreign_key_violation(conn: &Connection) -> MailResult<bool> {
    let mut stmt = conn.prepare("PRAGMA foreign_key_check").map_err(db_err)?;
    let mut rows = stmt.query([]).map_err(db_err)?;
    Ok(rows.next().map_err(db_err)?.is_some())
}

/// 单页上限：UI 一次最多拉 500 条，避免误传巨大 limit 打爆内存。
pub const MAX_PAGE_SIZE: u32 = 500;

/// 关联表批量查询单批上限，留足 SQLite 变量数余量。
const RELATION_CHUNK: usize = 400;

/// 邮件 SELECT 列顺序，与 `EmailRow::read` 严格对应。
const EMAIL_COLUMNS: &str = "e.id,e.account_id,e.subject,e.from_json,e.to_json,e.cc_json,\
     e.bcc_json,e.reply_to_json,e.message_id,e.sent_at,e.received_at,e.preview,e.size,\
     e.state,e.modseq,e.uid,e.blob_id,e.body_path,e.thread_id";

fn json_of<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn parse_addrs(raw: &str) -> Vec<Address> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// LIKE 通配符转义：用户输入的 % 和 _ 只当普通字符，语义不被改写。
fn like_pattern(keyword: &str) -> String {
    let escaped = keyword
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

fn placeholders(n: usize) -> String {
    std::iter::repeat("?").take(n).collect::<Vec<_>>().join(",")
}

/// 拼出邮件列表的 WHERE 片段与绑定值（只拼静态 SQL，值一律参数绑定）。
fn email_predicate(
    account_id: &str,
    mailbox_id: Option<&str>,
    unread_only: bool,
) -> (String, Vec<Value>) {
    let mut where_sql = String::from("e.account_id=?");
    let mut args = vec![Value::Text(account_id.to_string())];
    if let Some(mid) = mailbox_id {
        where_sql.push_str(
            " AND EXISTS (SELECT 1 FROM email_mailboxes m \
             WHERE m.email_id=e.id AND m.mailbox_id=?)",
        );
        args.push(Value::Text(mid.to_string()));
    }
    if unread_only {
        where_sql.push_str(
            " AND NOT EXISTS (SELECT 1 FROM email_flags f \
             WHERE f.email_id=e.id AND f.flag=?)",
        );
        args.push(Value::Text(json_of(&Flag::Seen)));
    }
    (where_sql, args)
}

/// 邮件列表查询条件（S4-02）。空 mailbox_id 表示整个账号。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmailQuery {
    pub mailbox_id: Option<Id>,
    pub unread_only: bool,
    pub limit: u32,
    pub offset: u32,
}

impl EmailQuery {
    pub fn new() -> Self {
        Self {
            mailbox_id: None,
            unread_only: false,
            limit: 50,
            offset: 0,
        }
    }

    pub fn in_mailbox(mut self, mailbox_id: impl Into<Id>) -> Self {
        self.mailbox_id = Some(mailbox_id.into());
        self
    }

    pub fn unread_only(mut self) -> Self {
        self.unread_only = true;
        self
    }

    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmailPage {
    pub items: Vec<Email>,
    pub total: u32,
    /// 下一页偏移；None 表示已到末页
    pub next_offset: Option<u32>,
}

/// 搜索字段开关（S4-06：标题 / 发件人 / 收件人）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchFields {
    pub subject: bool,
    pub from: bool,
    pub to: bool,
}

impl Default for SearchFields {
    fn default() -> Self {
        Self {
            subject: true,
            from: true,
            to: true,
        }
    }
}

impl SearchFields {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn subject_only() -> Self {
        Self {
            subject: true,
            from: false,
            to: false,
        }
    }

    pub fn from_only() -> Self {
        Self {
            subject: false,
            from: true,
            to: false,
        }
    }

    pub fn to_only() -> Self {
        Self {
            subject: false,
            from: false,
            to: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub keyword: String,
    pub fields: SearchFields,
    pub limit: u32,
    pub offset: u32,
}

impl SearchQuery {
    pub fn new(keyword: impl Into<String>) -> Self {
        Self {
            keyword: keyword.into(),
            fields: SearchFields::all(),
            limit: 50,
            offset: 0,
        }
    }

    pub fn with_fields(mut self, fields: SearchFields) -> Self {
        self.fields = fields;
        self
    }

    pub fn with_limit(mut self, limit: u32) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_offset(mut self, offset: u32) -> Self {
        self.offset = offset;
        self
    }
}

/// 邮件行：列顺序与 `EMAIL_COLUMNS` 一一对应。
struct EmailRow {
    id: String,
    account_id: String,
    subject: String,
    from_json: String,
    to_json: String,
    cc_json: String,
    bcc_json: String,
    reply_to_json: String,
    message_id: Option<String>,
    sent_at: Option<String>,
    received_at: String,
    preview: String,
    size: i64,
    state: Option<String>,
    modseq: Option<i64>,
    uid: Option<i64>,
    blob_id: Option<String>,
    body_path: Option<String>,
    thread_id: Option<String>,
}

impl EmailRow {
    fn read(r: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: r.get(0)?,
            account_id: r.get(1)?,
            subject: r.get(2)?,
            from_json: r.get(3)?,
            to_json: r.get(4)?,
            cc_json: r.get(5)?,
            bcc_json: r.get(6)?,
            reply_to_json: r.get(7)?,
            message_id: r.get(8)?,
            sent_at: r.get(9)?,
            received_at: r.get(10)?,
            preview: r.get(11)?,
            size: r.get(12)?,
            state: r.get(13)?,
            modseq: r.get(14)?,
            uid: r.get(15)?,
            blob_id: r.get(16)?,
            body_path: r.get(17)?,
            thread_id: r.get(18)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub from_version: i64,
    pub to_version: i64,
    pub applied: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncCursor {
    pub state: Option<String>,
    pub modseq: Option<u64>,
}

/// 外部资源放行记录（S6-02 / SEC-003）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowanceRow {
    /// 发件人邮箱或可注册域
    pub sender: String,
    /// `once` / `sender` / `domain`
    pub scope: String,
    pub created_at: Option<DateTime<Utc>>,
    /// None = 永久
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize, Default)]
struct AccountConfig {
    jmap: Option<JmapConfig>,
    imap: Option<ImapConfig>,
    smtp: Option<SmtpConfig>,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> MailResult<Self> {
        let conn = Connection::open(path).map_err(db_err)?;
        let db = Self { conn };
        db.configure()?;
        Ok(db)
    }

    pub fn open_in_memory() -> MailResult<Self> {
        let conn = Connection::open_in_memory().map_err(db_err)?;
        let db = Self { conn };
        db.configure()?;
        Ok(db)
    }

    fn configure(&self) -> MailResult<()> {
        self.conn
            .execute_batch(
                "PRAGMA journal_mode=WAL;\
                 PRAGMA foreign_keys=ON;\
                 PRAGMA busy_timeout=5000;\
                 PRAGMA synchronous=NORMAL;",
            )
            .map_err(db_err)
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn schema_version(&self) -> MailResult<i64> {
        self.conn
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .map_err(db_err)
    }

    /// 幂等增量迁移：每个版本在独立事务内执行，失败整体回滚。
    pub fn migrate(&mut self) -> MailResult<MigrationReport> {
        let from = self.schema_version()?;
        let mut to = from;
        let mut applied = Vec::new();
        for m in migrations::all() {
            if m.version <= from {
                continue;
            }
            // 重建表的迁移必须先关外键：`PRAGMA foreign_keys` 在事务内改是空操作，
            // 只能在 BEGIN 之前切。开着外键 `DROP TABLE emails` 会把
            // email_mailboxes / email_flags / attachments 按 CASCADE 一起清空。
            if m.rebuilds_table {
                self.conn
                    .execute_batch("PRAGMA foreign_keys=OFF;")
                    .map_err(db_err)?;
            }
            let outcome = self.apply_one(&m);
            if m.rebuilds_table {
                // 无论成败都恢复：外键约束是数据一致性的最后一道闸，
                // 失败路径也绝不能把它永久关掉。
                let restore = self
                    .conn
                    .execute_batch("PRAGMA foreign_keys=ON;")
                    .map_err(db_err);
                if outcome.is_ok() {
                    restore?;
                }
            }
            outcome?;
            to = m.version;
            applied.push(format!("v{} {}", m.version, m.name));
        }
        Ok(MigrationReport {
            from_version: from,
            to_version: to,
            applied,
        })
    }

    /// 执行单个迁移版本（事务 + 版本号原子推进）。
    fn apply_one(&mut self, m: &migrations::Migration) -> MailResult<()> {
        let tx = self.conn.transaction().map_err(db_err)?;
        tx.execute_batch(m.sql).map_err(db_err)?;
        if m.rebuilds_table && has_foreign_key_violation(&tx)? {
            // 重建把子表指向搞坏时宁可整体回滚，也不要留一个「能读不能写」的库给用户。
            // 这里直接返回，`tx` 出作用域即回滚。
            return Err(MailError::Storage(format!(
                "迁移 v{} 破坏了外键引用，已回滚",
                m.version
            )));
        }
        // user_version 只能由内部整数生成，无注入面
        tx.execute_batch(&format!("PRAGMA user_version = {};", m.version))
            .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok(())
    }

    // ------------------------------------------------------------ 账号（ACC / S4-04）
    pub fn upsert_account(&self, a: &Account) -> MailResult<()> {
        let cfg = serde_json::json!({ "jmap": a.jmap, "imap": a.imap, "smtp": a.smtp });
        let protocol = match a.protocol {
            ProtocolKind::Jmap => "jmap",
            ProtocolKind::Imap => "imap",
        };
        self.conn
            .execute(
                "INSERT INTO accounts (id,email_address,display_name,protocol,config_json,\
                 sync_days,poll_interval_secs,created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8) \
                 ON CONFLICT(id) DO UPDATE SET email_address=excluded.email_address,\
                 display_name=excluded.display_name, protocol=excluded.protocol,\
                 config_json=excluded.config_json, sync_days=excluded.sync_days,\
                 poll_interval_secs=excluded.poll_interval_secs",
                params![
                    a.id,
                    a.email_address,
                    a.display_name,
                    protocol,
                    cfg.to_string(),
                    a.sync_days as i64,
                    a.poll_interval_secs as i64,
                    a.created_at.to_rfc3339()
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn get_account(&self, id: &str) -> MailResult<Option<Account>> {
        self.conn
            .query_row(
                "SELECT id,email_address,display_name,protocol,config_json,sync_days,\
                 poll_interval_secs,created_at FROM accounts WHERE id=?1",
                params![id],
                |r| {
                    let protocol: String = r.get(3)?;
                    let cfg: String = r.get(4)?;
                    let created: String = r.get(7)?;
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        protocol,
                        cfg,
                        r.get::<_, i64>(5)?,
                        r.get::<_, i64>(6)?,
                        created,
                    ))
                },
            )
            .optional()
            .map_err(db_err)?
            .map(|(id, email, display, protocol, cfg, days, poll, created)| {
                let parsed: AccountConfig = serde_json::from_str(&cfg).unwrap_or_default();
                let created_at = DateTime::parse_from_rfc3339(&created)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                Ok(Account {
                    id,
                    email_address: email,
                    display_name: display,
                    protocol: if protocol == "jmap" {
                        ProtocolKind::Jmap
                    } else {
                        ProtocolKind::Imap
                    },
                    jmap: parsed.jmap,
                    imap: parsed.imap,
                    smtp: parsed.smtp,
                    sync_days: days as u32,
                    poll_interval_secs: poll as u32,
                    created_at,
                })
            })
            .transpose()
    }

    pub fn list_accounts(&self) -> MailResult<Vec<Id>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM accounts ORDER BY created_at")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(db_err)?);
        }
        Ok(out)
    }

    pub fn delete_account(&self, id: &str) -> MailResult<usize> {
        self.conn
            .execute("DELETE FROM accounts WHERE id=?1", params![id])
            .map_err(db_err)
    }

    // ------------------------------------------------------------ 文件夹（S4-01）
    pub fn upsert_mailbox(&self, m: &Mailbox) -> MailResult<()> {
        self.conn
            .execute(
                "INSERT INTO mailboxes (id,account_id,name,parent_id,role,total,unread,\
                 uid_validity,highest_modseq,state) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) \
                 ON CONFLICT(id) DO UPDATE SET name=excluded.name,parent_id=excluded.parent_id,\
                 role=excluded.role,total=excluded.total,unread=excluded.unread,\
                 uid_validity=excluded.uid_validity,highest_modseq=excluded.highest_modseq,\
                 state=excluded.state",
                params![
                    m.id,
                    m.account_id,
                    m.name,
                    m.parent_id,
                    format!("{:?}", m.role).to_lowercase(),
                    m.total as i64,
                    m.unread as i64,
                    m.uid_validity.map(|v| v as i64),
                    m.highest_modseq.map(|v| v as i64),
                    m.state
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn list_mailboxes(&self, account_id: &str) -> MailResult<Vec<Mailbox>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id,account_id,name,parent_id,role,total,unread,uid_validity,\
                 highest_modseq,state FROM mailboxes WHERE account_id=?1 ORDER BY name",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![account_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?,
                    r.get::<_, i64>(6)?,
                    r.get::<_, Option<i64>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                ))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let (id, account_id, name, parent_id, role, total, unread, uidv, modseq, state) =
                row.map_err(db_err)?;
            let role = serde_json::from_value::<MailboxRole>(serde_json::Value::String(role))
                .unwrap_or(MailboxRole::Other);
            out.push(Mailbox {
                id,
                account_id,
                name,
                parent_id,
                role,
                total: total as u64,
                unread: unread as u64,
                uid_validity: uidv.map(|v| v as u32),
                highest_modseq: modseq.map(|v| v as u64),
                state,
            });
        }
        Ok(out)
    }

    // ------------------------------------------------------------ 邮件元数据（S4-02）
    pub fn upsert_email(&self, e: &Email) -> MailResult<()> {
        // unchecked_transaction 支持 &self，便于 Database 被 Arc 共享；事务仍保证原子性
        let tx = self.conn.unchecked_transaction().map_err(db_err)?;
        tx.execute(
            "INSERT INTO emails (id,account_id,subject,from_json,to_json,cc_json,bcc_json,\
             reply_to_json,message_id,thread_id,sent_at,preview,received_at,size,state,modseq,\
             uid,blob_id,body_path) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19) \
             ON CONFLICT(id) DO UPDATE SET subject=excluded.subject,from_json=excluded.from_json,\
             to_json=excluded.to_json,cc_json=excluded.cc_json,bcc_json=excluded.bcc_json,\
             reply_to_json=excluded.reply_to_json,message_id=excluded.message_id,\
             thread_id=excluded.thread_id,sent_at=excluded.sent_at,preview=excluded.preview,\
             size=excluded.size,state=excluded.state,modseq=excluded.modseq,uid=excluded.uid,\
             blob_id=excluded.blob_id",
            params![
                e.id,
                e.account_id,
                e.subject,
                json_of(&e.from),
                json_of(&e.to),
                json_of(&e.cc),
                json_of(&e.bcc),
                json_of(&e.reply_to),
                e.message_id,
                e.thread_id,
                e.sent_at.map(|d| d.to_rfc3339()),
                e.preview,
                e.received_at.to_rfc3339(),
                e.size as i64,
                e.state,
                e.modseq.map(|v| v as i64),
                e.uid.map(|v| v as i64),
                e.blob_id,
                Option::<String>::None
            ],
        )
        .map_err(db_err)?;
        tx.execute(
            "DELETE FROM email_mailboxes WHERE email_id=?1",
            params![e.id],
        )
        .map_err(db_err)?;
        for mid in &e.mailbox_ids {
            tx.execute(
                "INSERT OR IGNORE INTO email_mailboxes (email_id,mailbox_id) VALUES (?1,?2)",
                params![e.id, mid],
            )
            .map_err(db_err)?;
        }
        tx.execute("DELETE FROM email_flags WHERE email_id=?1", params![e.id])
            .map_err(db_err)?;
        for f in &e.flags {
            tx.execute(
                "INSERT OR IGNORE INTO email_flags (email_id,flag) VALUES (?1,?2)",
                params![e.id, json_of(f)],
            )
            .map_err(db_err)?;
        }
        tx.execute("DELETE FROM attachments WHERE email_id=?1", params![e.id])
            .map_err(db_err)?;
        for a in &e.attachments {
            tx.execute(
                "INSERT OR REPLACE INTO attachments \
                 (id,email_id,name,content_type,size,blob_id,part_id,inline,cid) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    a.id,
                    e.id,
                    a.name,
                    a.content_type,
                    a.size as i64,
                    a.blob_id,
                    a.part_id,
                    a.inline as i64,
                    a.cid
                ],
            )
            .map_err(db_err)?;
        }
        tx.commit().map_err(db_err)
    }

    pub fn get_email(&self, id: &str) -> MailResult<Option<Email>> {
        let mut emails = self.query_emails(
            &format!("SELECT {EMAIL_COLUMNS} FROM emails e WHERE e.id=?1"),
            params![id],
        )?;
        Ok(emails.pop())
    }

    pub fn list_emails(&self, account_id: &str, limit: u32) -> MailResult<Vec<Email>> {
        self.query_emails(
            &format!(
                "SELECT {EMAIL_COLUMNS} FROM emails e WHERE e.account_id=?1 \
                 ORDER BY e.received_at DESC, e.id DESC LIMIT ?2"
            ),
            params![account_id, limit],
        )
    }

    /// 按文件夹列邮件（S3-05 全量校准用：判定本地多余行）。
    pub fn list_emails_in_mailbox(
        &self,
        account_id: &str,
        mailbox_id: &str,
        limit: u32,
    ) -> MailResult<Vec<Email>> {
        self.query_emails(
            &format!(
                "SELECT {EMAIL_COLUMNS} FROM emails e \
                 JOIN email_mailboxes m ON m.email_id=e.id \
                 WHERE e.account_id=?1 AND m.mailbox_id=?2 \
                 ORDER BY e.received_at DESC, e.id DESC LIMIT ?3"
            ),
            params![account_id, mailbox_id, limit],
        )
    }

    /// 分页列出邮件（S4-02）：文件夹过滤 + 仅看未读，返回总数用于 UI 分页器。
    pub fn page_emails(&self, account_id: &str, q: &EmailQuery) -> MailResult<EmailPage> {
        let (where_sql, mut args) =
            email_predicate(account_id, q.mailbox_id.as_deref(), q.unread_only);
        let total: u32 = self
            .conn
            .query_row(
                &format!("SELECT COUNT(*) FROM emails e WHERE {where_sql}"),
                params_from_iter(args.iter()),
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u32)
            .map_err(db_err)?;

        let limit = q.limit.clamp(1, MAX_PAGE_SIZE) as i64;
        // SQL 文本里 limit/offset 的顺序与绑定顺序必须一致：先 LIMIT 后 OFFSET
        let sql = format!(
            "SELECT {EMAIL_COLUMNS} FROM emails e WHERE {where_sql} \
             ORDER BY e.received_at DESC, e.id DESC LIMIT ? OFFSET ?"
        );
        args.push(Value::Integer(limit));
        args.push(Value::Integer(q.offset as i64));
        let items = self.query_emails(&sql, params_from_iter(args.iter()))?;
        let next_offset = q.offset.saturating_add(limit as u32);
        Ok(EmailPage {
            items,
            total,
            next_offset: if next_offset < total {
                Some(next_offset)
            } else {
                None
            },
        })
    }

    /// 单个账号的邮件总数（S4-02 验收：万级数据可查）。
    pub fn count_emails(&self, account_id: &str) -> MailResult<u32> {
        self.page_emails(account_id, &EmailQuery::new().with_limit(1))
            .map(|p| p.total)
    }

    /// 每个文件夹的未读计数（S4-02），一次查询算完，避免逐文件夹 COUNT。
    pub fn unread_counts(&self, account_id: &str) -> MailResult<BTreeMap<Id, u32>> {
        let seen = json_of(&Flag::Seen);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT m.mailbox_id, COUNT(*) FROM email_mailboxes m \
                 JOIN emails e ON e.id=m.email_id \
                 WHERE e.account_id=?1 AND NOT EXISTS ( \
                     SELECT 1 FROM email_flags f WHERE f.email_id=e.id AND f.flag=?2) \
                 GROUP BY m.mailbox_id",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![account_id, seen], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(db_err)?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (id, n) = row.map_err(db_err)?;
            out.insert(id, n as u32);
        }
        Ok(out)
    }

    /// 关键词搜索（S4-06）：可按标题/发件人/收件人（含抄送）分字段组合，支持分页。
    pub fn search_page(&self, account_id: &str, q: &SearchQuery) -> MailResult<EmailPage> {
        // 参数绑定 + 转义 LIKE 通配符，防止用户输入改变匹配语义
        let pattern = like_pattern(&q.keyword);
        // 每个分支声明自己的绑定值个数（“收件人”分支同时覆盖 to 与 cc）
        let mut clauses: Vec<(&str, usize)> = Vec::new();
        if q.fields.subject {
            clauses.push(("e.subject LIKE ? ESCAPE '\\'", 1));
        }
        if q.fields.from {
            clauses.push(("e.from_json LIKE ? ESCAPE '\\'", 1));
        }
        if q.fields.to {
            clauses.push((
                "(e.to_json LIKE ? ESCAPE '\\' OR e.cc_json LIKE ? ESCAPE '\\')",
                2,
            ));
        }
        // 一个字段都不选时退化为全字段搜索，不返回空结果
        if clauses.is_empty() {
            clauses = vec![
                ("e.subject LIKE ? ESCAPE '\\'", 1),
                ("e.from_json LIKE ? ESCAPE '\\'", 1),
                ("e.to_json LIKE ? ESCAPE '\\'", 1),
            ];
        }
        let where_sql = format!(
            "e.account_id=? AND ({})",
            clauses
                .iter()
                .map(|(sql, _)| *sql)
                .collect::<Vec<_>>()
                .join(" OR ")
        );
        let mut args: Vec<Value> = vec![Value::Text(account_id.to_string())];
        for (_, n) in &clauses {
            for _ in 0..*n {
                args.push(Value::Text(pattern.clone()));
            }
        }

        let total: u32 = self
            .conn
            .query_row(
                &format!("SELECT COUNT(*) FROM emails e WHERE {where_sql}"),
                params_from_iter(args.iter()),
                |r| r.get::<_, i64>(0),
            )
            .map(|v| v as u32)
            .map_err(db_err)?;

        let limit = q.limit.clamp(1, MAX_PAGE_SIZE) as i64;
        let sql = format!(
            "SELECT {EMAIL_COLUMNS} FROM emails e WHERE {where_sql} \
             ORDER BY e.received_at DESC, e.id DESC LIMIT ? OFFSET ?"
        );
        args.push(Value::Integer(limit));
        args.push(Value::Integer(q.offset as i64));
        let items = self.query_emails(&sql, params_from_iter(args.iter()))?;
        let next_offset = q.offset.saturating_add(limit as u32);
        Ok(EmailPage {
            items,
            total,
            next_offset: if next_offset < total {
                Some(next_offset)
            } else {
                None
            },
        })
    }

    /// 兼容旧调用：默认全字段搜索，最多 200 条。
    pub fn search_emails(&self, account_id: &str, keyword: &str) -> MailResult<Vec<Email>> {
        let q = SearchQuery::new(keyword).with_limit(200);
        self.search_page(account_id, &q).map(|p| p.items)
    }

    /// 正文落盘路径登记（S4-03）：缓存管理器把正文写到磁盘后回写此列。
    pub fn set_email_body_path(&self, id: &str, path: Option<&str>) -> MailResult<usize> {
        self.conn
            .execute(
                "UPDATE emails SET body_path=?2 WHERE id=?1",
                params![id, path],
            )
            .map_err(db_err)
    }

    /// 批量取正文缓存路径（S4-03：避免为了读正文把整封元数据拉回来）。
    pub fn email_body_path(&self, id: &str) -> MailResult<Option<String>> {
        self.conn
            .query_row(
                "SELECT body_path FROM emails WHERE id=?1",
                params![id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(db_err)
            .map(|v| v.flatten())
    }

    /// 查询执行计划，供性能测试断言走了索引（S4-02）。
    pub fn explain(&self, sql: &str) -> MailResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(3))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db_err)?);
        }
        Ok(out)
    }

    fn query_emails(&self, sql: &str, p: impl rusqlite::Params) -> MailResult<Vec<Email>> {
        let mut stmt = self.conn.prepare(sql).map_err(db_err)?;
        let rows = stmt.query_map(p, EmailRow::read).map_err(db_err)?;
        let mut out = Vec::new();
        for row in rows {
            let r = row.map_err(db_err)?;
            let body_path = r.body_path.clone();
            let mut e = Email::new(r.id.clone(), r.account_id, "");
            e.subject = r.subject;
            e.from = parse_addrs(&r.from_json);
            e.to = parse_addrs(&r.to_json);
            e.cc = parse_addrs(&r.cc_json);
            e.bcc = parse_addrs(&r.bcc_json);
            e.reply_to = parse_addrs(&r.reply_to_json);
            e.message_id = r.message_id;
            e.thread_id = r.thread_id;
            e.sent_at = r.sent_at.as_deref().and_then(parse_time);
            e.received_at = parse_time(&r.received_at).unwrap_or_else(Utc::now);
            e.preview = r.preview;
            e.size = r.size as u64;
            e.state = r.state;
            e.modseq = r.modseq.map(|v| v as u64);
            e.uid = r.uid.map(|v| v as u32);
            e.blob_id = r.blob_id;
            e.text_body = body_path.and_then(|p| std::fs::read_to_string(p).ok());
            out.push(e);
        }
        // 关联表一次性批量取回，杜绝逐行 N+1（万级列表的性能分水岭）
        self.attach_relations(&mut out)?;
        Ok(out)
    }

    /// 为一批邮件填充文件夹/标记/附件关联（分批 IN 查询，单批不超过 SQLite 变量上限）。
    fn attach_relations(&self, emails: &mut [Email]) -> MailResult<()> {
        if emails.is_empty() {
            return Ok(());
        }
        let mut mailbox_map: HashMap<String, Vec<Id>> = HashMap::new();
        let mut flag_map: HashMap<String, Vec<Flag>> = HashMap::new();
        let mut att_map: HashMap<String, Vec<CrossAttachment>> = HashMap::new();
        let ids: Vec<String> = emails.iter().map(|e| e.id.clone()).collect();

        for chunk in ids.chunks(RELATION_CHUNK) {
            let holders = placeholders(chunk.len());
            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT email_id, mailbox_id FROM email_mailboxes \
                     WHERE email_id IN ({holders})"
                ))
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params_from_iter(chunk.iter()), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(db_err)?;
            for row in rows {
                let (eid, mid) = row.map_err(db_err)?;
                mailbox_map.entry(eid).or_default().push(mid);
            }

            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT email_id, flag FROM email_flags WHERE email_id IN ({holders})"
                ))
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params_from_iter(chunk.iter()), |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .map_err(db_err)?;
            for row in rows {
                let (eid, flag) = row.map_err(db_err)?;
                if let Ok(f) = serde_json::from_str::<Flag>(&flag) {
                    flag_map.entry(eid).or_default().push(f);
                }
            }

            let mut stmt = self
                .conn
                .prepare(&format!(
                    "SELECT email_id,id,name,content_type,size,blob_id,part_id,inline,cid \
                     FROM attachments WHERE email_id IN ({holders})"
                ))
                .map_err(db_err)?;
            let rows = stmt
                .query_map(params_from_iter(chunk.iter()), |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        CrossAttachment {
                            id: r.get(1)?,
                            name: r.get(2)?,
                            content_type: r.get(3)?,
                            size: r.get::<_, i64>(4)? as u64,
                            blob_id: r.get(5)?,
                            part_id: r.get(6)?,
                            inline: r.get::<_, i64>(7)? != 0,
                            cid: r.get(8)?,
                        },
                    ))
                })
                .map_err(db_err)?;
            for row in rows {
                let (eid, att) = row.map_err(db_err)?;
                att_map.entry(eid).or_default().push(att);
            }
        }

        for e in emails.iter_mut() {
            e.mailbox_ids = mailbox_map.remove(&e.id).unwrap_or_default();
            e.flags = flag_map.remove(&e.id).unwrap_or_default();
            e.attachments = att_map.remove(&e.id).unwrap_or_default();
        }
        Ok(())
    }

    pub fn delete_email(&self, id: &str) -> MailResult<usize> {
        self.conn
            .execute("DELETE FROM emails WHERE id=?1", params![id])
            .map_err(db_err)
    }

    // ------------------------------------------------------------ 身份改写（移动后校准）

    /// 服务器给邮件换了身份（IMAP MOVE 会分配新 UID）后，把本地记录一并搬到新主键。
    ///
    /// 三件事必须一起做，少一件就会留下不一致：
    /// 1. 本地邮件记录换 id（连带 `email_mailboxes` 指向新文件夹）——否则 UI 拿旧 id 打不开；
    /// 2. **队列里引用旧 id 的后续操作一起改写**——否则「先移动、再标星」的第二条会打向
    ///    不存在的旧 UID，而服务端对这种 STORE 静默回 OK，用户以为标上了；
    /// 3. 返回改写的队列条数，便于上层记录/断言。
    ///
    /// 正文缓存路径不迁移（`body_path` 不随 upsert 保留）：新记录下次打开正文会重新拉取，
    /// 代价是一次网络往返，好过把旧 id 的缓存错挂到新 id 上。
    pub fn remap_email(
        &self,
        account_id: &str,
        from: &str,
        to: &str,
        mailbox_id: &str,
    ) -> MailResult<usize> {
        let mut touched = 0usize;
        if let Some(mut email) = self.get_email(from)? {
            email.id = to.to_string();
            email.mailbox_ids = vec![mailbox_id.to_string()];
            // 主键最后一段就是 UID（`账号/文件夹/UID`）；新文件夹的 UID 空间是独立的，
            // 可能就是 1，和旧记录撞车的概率不低，所以 UID 必须跟着换。
            if let Some(uid) = to.rsplit('/').next().and_then(|s| s.parse::<u32>().ok()) {
                email.uid = Some(uid);
            }
            // 先插新行再删旧行：v6 迁移已经去掉了错误的 UNIQUE(account_id, uid)，
            // 两行短暂共存不会撞约束；反过来（先删后插）中间有一瞬间这封邮件不存在，
            // 进程恰好在此崩溃就会从本地消失。
            self.upsert_email(&email)?;
            self.delete_email(from)?;
        }
        // 队列改写：只动邮件主键，不动文件夹字段
        for mut op in self.pending_operations(account_id)? {
            let before = op.kind.clone();
            op.kind.remap_email_id(&from.to_string(), &to.to_string());
            if op.kind != before {
                self.enqueue(&op)?;
                touched += 1;
            }
        }
        Ok(touched)
    }

    // ------------------------------------------------------------ 同步游标（A-005）
    pub fn get_sync_cursor(&self, account_id: &str, mailbox_id: &str) -> MailResult<SyncCursor> {
        let row = self
            .conn
            .query_row(
                "SELECT state,modseq FROM sync_state WHERE account_id=?1 AND mailbox_id=?2",
                params![account_id, mailbox_id],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .optional()
            .map_err(db_err)?;
        Ok(match row {
            Some((state, modseq)) => SyncCursor {
                state,
                modseq: modseq.map(|v| v as u64),
            },
            None => SyncCursor::default(),
        })
    }

    pub fn set_sync_cursor(
        &self,
        account_id: &str,
        mailbox_id: &str,
        c: &SyncCursor,
    ) -> MailResult<()> {
        self.conn
            .execute(
                "INSERT INTO sync_state (account_id,mailbox_id,state,modseq,last_sync_at) \
             VALUES (?1,?2,?3,?4,?5) ON CONFLICT(account_id,mailbox_id) DO UPDATE SET \
             state=excluded.state,modseq=excluded.modseq,last_sync_at=excluded.last_sync_at",
                params![
                    account_id,
                    mailbox_id,
                    c.state,
                    c.modseq.map(|v| v as i64),
                    Utc::now().to_rfc3339()
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    // ------------------------------------------------------------ 离线队列（A-006）
    pub fn enqueue(&self, op: &OfflineOperation) -> MailResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO offline_queue (id,account_id,kind_json,created_at,attempts,last_error) \
             VALUES (?1,?2,?3,?4,?5,?6)",
            params![op.id, op.account_id,
                    serde_json::to_string(&op.kind).unwrap_or_default(),
                    op.created_at.to_rfc3339(), op.attempts as i64, op.last_error],
        ).map_err(db_err)?;
        Ok(())
    }

    /// 按入队顺序返回（先入先回放），保证离线操作不丢失。
    pub fn pending_operations(&self, account_id: &str) -> MailResult<Vec<OfflineOperation>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id,account_id,kind_json,created_at,attempts,last_error \
             FROM offline_queue WHERE account_id=?1 ORDER BY created_at ASC, rowid ASC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![account_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for r in rows {
            let (id, account_id, kind_json, created, attempts, last_error) = r.map_err(db_err)?;
            let kind =
                serde_json::from_str(&kind_json).map_err(|e| MailError::Storage(e.to_string()))?;
            let created_at = DateTime::parse_from_rfc3339(&created)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            out.push(OfflineOperation {
                id,
                account_id,
                kind,
                created_at,
                attempts: attempts as u32,
                last_error,
            });
        }
        Ok(out)
    }

    pub fn mark_operation_done(&self, id: &str) -> MailResult<()> {
        self.conn
            .execute("DELETE FROM offline_queue WHERE id=?1", params![id])
            .map_err(db_err)?;
        Ok(())
    }

    pub fn record_operation_error(&self, id: &str, err: &str) -> MailResult<()> {
        self.conn
            .execute(
                "UPDATE offline_queue SET attempts=attempts+1,last_error=?2 WHERE id=?1",
                params![id, err],
            )
            .map_err(db_err)?;
        Ok(())
    }

    // ------------------------------------------------------------ 凭证密文仓（S4-04）
    /// 写入密文（明文加密由业务层完成，存储层只做持久化）。
    pub fn put_secret(&self, alias: &str, sealed: &str, now: DateTime<Utc>) -> MailResult<()> {
        self.conn
            .execute(
                "INSERT INTO secrets (alias,sealed,updated_at) VALUES (?1,?2,?3) \
                 ON CONFLICT(alias) DO UPDATE SET sealed=excluded.sealed,\
                 updated_at=excluded.updated_at",
                params![alias, sealed, now.to_rfc3339()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn get_secret(&self, alias: &str) -> MailResult<Option<String>> {
        self.conn
            .query_row(
                "SELECT sealed FROM secrets WHERE alias=?1",
                params![alias],
                |r| r.get::<_, String>(0),
            )
            .optional()
            .map_err(db_err)
    }

    pub fn delete_secret(&self, alias: &str) -> MailResult<usize> {
        self.conn
            .execute("DELETE FROM secrets WHERE alias=?1", params![alias])
            .map_err(db_err)
    }

    /// 按别名前缀批量删除（删账号时清掉它名下所有凭证）。
    pub fn delete_secrets_with_prefix(&self, prefix: &str) -> MailResult<usize> {
        // 前缀作为绑定参数传入，'%' '_' 均转义，避免别名里的下划线变成通配符
        let escaped = prefix
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        self.conn
            .execute(
                "DELETE FROM secrets WHERE alias LIKE ?1 ESCAPE '\\'",
                params![format!("{escaped}%")],
            )
            .map_err(db_err)
    }

    pub fn list_secret_aliases(&self) -> MailResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT alias FROM secrets ORDER BY alias")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(db_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db_err)?);
        }
        Ok(out)
    }

    // ------------------------------------------------------------ 外部资源放行（S6-02）

    /// 写入/更新一条放行记录。`key` 由业务层决定（发件人邮箱或可注册域）。
    pub fn put_allowance(
        &self,
        key: &str,
        scope: &str,
        now: DateTime<Utc>,
        expires_at: Option<DateTime<Utc>>,
    ) -> MailResult<()> {
        self.conn
            .execute(
                "INSERT INTO external_allowlist (sender,scope,created_at,expires_at) \
                 VALUES (?1,?2,?3,?4) \
                 ON CONFLICT(sender) DO UPDATE SET scope=excluded.scope,\
                 created_at=excluded.created_at,expires_at=excluded.expires_at",
                params![
                    key,
                    scope,
                    now.to_rfc3339(),
                    expires_at.map(|t| t.to_rfc3339())
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn get_allowance(&self, key: &str) -> MailResult<Option<AllowanceRow>> {
        self.conn
            .query_row(
                "SELECT sender,scope,created_at,expires_at FROM external_allowlist WHERE sender=?1",
                params![key],
                |r| {
                    Ok(AllowanceRow {
                        sender: r.get(0)?,
                        scope: r.get(1)?,
                        created_at: parse_time(&r.get::<_, String>(2)?),
                        expires_at: r
                            .get::<_, Option<String>>(3)?
                            .as_deref()
                            .and_then(parse_time),
                    })
                },
            )
            .optional()
            .map_err(db_err)
    }

    pub fn delete_allowance(&self, key: &str) -> MailResult<usize> {
        self.conn
            .execute(
                "DELETE FROM external_allowlist WHERE sender=?1",
                params![key],
            )
            .map_err(db_err)
    }

    pub fn list_allowances(&self) -> MailResult<Vec<AllowanceRow>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT sender,scope,created_at,expires_at FROM external_allowlist \
                 ORDER BY created_at DESC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AllowanceRow {
                    sender: r.get(0)?,
                    scope: r.get(1)?,
                    created_at: parse_time(&r.get::<_, String>(2)?),
                    expires_at: r
                        .get::<_, Option<String>>(3)?
                        .as_deref()
                        .and_then(parse_time),
                })
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db_err)?);
        }
        Ok(out)
    }

    /// 清掉已过期的放行记录（每次渲染前调一次，避免表无限长）。
    pub fn purge_expired_allowances(&self, now: DateTime<Utc>) -> MailResult<usize> {
        self.conn
            .execute(
                "DELETE FROM external_allowlist WHERE expires_at IS NOT NULL AND expires_at <= ?1",
                params![now.to_rfc3339()],
            )
            .map_err(db_err)
    }

    // ------------------------------------------------------------ 缓存配额（STO-002/003）
    pub fn record_cache_file(&self, path: &str, size: u64) -> MailResult<()> {
        self.conn
            .execute(
                "INSERT INTO cache_files (path,size,last_access) VALUES (?1,?2,?3) \
             ON CONFLICT(path) DO UPDATE SET size=excluded.size,last_access=excluded.last_access",
                params![path, size as i64, Utc::now().to_rfc3339()],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn cache_usage(&self) -> MailResult<u64> {
        self.conn
            .query_row("SELECT COALESCE(SUM(size),0) FROM cache_files", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|v| v as u64)
            .map_err(db_err)
    }

    /// 命中缓存时刷新访问时间（LRU 的“最近使用”语义）。
    pub fn touch_cache_file(&self, path: &str, now: DateTime<Utc>) -> MailResult<bool> {
        let n = self
            .conn
            .execute(
                "UPDATE cache_files SET last_access=?2 WHERE path=?1",
                params![path, now.to_rfc3339()],
            )
            .map_err(db_err)?;
        Ok(n > 0)
    }

    /// 单条登记查询：判断某路径是否仍在缓存账本里。
    pub fn cache_file_size(&self, path: &str) -> MailResult<Option<u64>> {
        self.conn
            .query_row(
                "SELECT size FROM cache_files WHERE path=?1",
                params![path],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map(|v| v.map(|s| s as u64))
            .map_err(db_err)
    }

    pub fn forget_cache_file(&self, path: &str) -> MailResult<usize> {
        self.conn
            .execute("DELETE FROM cache_files WHERE path=?1", params![path])
            .map_err(db_err)
    }

    /// LRU 淘汰：超过配额时按 last_access 从旧到新删除，返回被清理路径。
    pub fn evict_lru(&self, quota_bytes: u64) -> MailResult<Vec<String>> {
        let mut removed = Vec::new();
        let mut usage = self.cache_usage()?;
        if usage <= quota_bytes {
            return Ok(removed);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT path,size FROM cache_files ORDER BY last_access ASC, path ASC")
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(db_err)?;
        let candidates: Vec<(String, i64)> = rows.filter_map(|r| r.ok()).collect();
        for (path, size) in candidates {
            if usage <= quota_bytes {
                break;
            }
            self.conn
                .execute("DELETE FROM cache_files WHERE path=?1", params![path])
                .map_err(db_err)?;
            usage = usage.saturating_sub(size as u64);
            removed.push(path);
        }
        Ok(removed)
    }

    // ---------- 收信规则（v1.3，UI-13） ----------

    /// 新增规则。id 由调用方生成（UUID 风格），created_at 由这里写入。
    pub fn insert_rule(&self, rule: &MailRule) -> MailResult<()> {
        self.conn
            .execute(
                "INSERT INTO rules (id,account_id,name,field,keyword,target_mailbox_id,enabled,created_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    rule.id,
                    rule.account_id,
                    rule.name,
                    rule.field,
                    rule.keyword,
                    rule.target_mailbox_id,
                    rule.enabled as i64,
                    Utc::now().to_rfc3339(),
                ],
            )
            .map_err(db_err)?;
        Ok(())
    }

    pub fn list_rules(&self, account_id: &str) -> MailResult<Vec<MailRule>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id,account_id,name,field,keyword,target_mailbox_id,enabled,created_at
                 FROM rules WHERE account_id=?1 ORDER BY created_at ASC, id ASC",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![account_id], map_rule)
            .map_err(db_err)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn update_rule_enabled(&self, rule_id: &str, enabled: bool) -> MailResult<()> {
        let n = self
            .conn
            .execute(
                "UPDATE rules SET enabled=?1 WHERE id=?2",
                params![enabled as i64, rule_id],
            )
            .map_err(db_err)?;
        if n == 0 {
            return Err(MailError::NotFound("rule".into()));
        }
        Ok(())
    }

    pub fn delete_rule(&self, rule_id: &str) -> MailResult<()> {
        let n = self
            .conn
            .execute("DELETE FROM rules WHERE id=?1", params![rule_id])
            .map_err(db_err)?;
        if n == 0 {
            return Err(MailError::NotFound("rule".into()));
        }
        Ok(())
    }
}

/// 一条客户端收信规则：`field` 匹配字段（from/to/subject），`keyword` 关键词，
/// 命中的邮件被移入 `target_mailbox_id`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MailRule {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub field: String,
    pub keyword: String,
    pub target_mailbox_id: String,
    pub enabled: bool,
    pub created_at: String,
}

fn map_rule(r: &rusqlite::Row<'_>) -> rusqlite::Result<MailRule> {
    Ok(MailRule {
        id: r.get(0)?,
        account_id: r.get(1)?,
        name: r.get(2)?,
        field: r.get(3)?,
        keyword: r.get(4)?,
        target_mailbox_id: r.get(5)?,
        enabled: r.get::<_, i64>(6)? != 0,
        created_at: r.get(7)?,
    })
}
