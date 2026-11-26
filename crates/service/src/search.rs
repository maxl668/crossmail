//! 基础搜索（S4-06）：标题 / 发件人 / 收件人，支持分页与字段组合。
//!
//! 只做「够快够准」的本地检索：不解析查询语法，不碰正文（正文在缓存文件里，
//! 全文检索属于后续 Sprint），先把 Backlog 明确的三个字段做扎实。

use crossmail_core::MailResult;
use crossmail_storage::{Database, EmailPage, SearchFields, SearchQuery};
use std::sync::{Arc, Mutex};

use crate::lock_db;

/// 搜索范围（面向 UI 的语义化开关）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    /// 全字段（标题 + 发件人 + 收件人/抄送）
    All,
    /// 只看标题
    Subject,
    /// 只看发件人
    From,
    /// 只看收件人（含抄送）
    To,
}

impl SearchScope {
    fn fields(&self) -> SearchFields {
        match self {
            SearchScope::All => SearchFields::all(),
            SearchScope::Subject => SearchFields::subject_only(),
            SearchScope::From => SearchFields::from_only(),
            SearchScope::To => SearchFields::to_only(),
        }
    }
}

pub struct SearchService {
    db: Arc<Mutex<Database>>,
}

impl SearchService {
    pub fn new(db: Arc<Mutex<Database>>) -> Self {
        Self { db }
    }

    /// 关键词搜索：空关键词直接返回空页，不返回全库（防误触把百万封拉出来）。
    pub fn search(
        &self,
        account_id: &str,
        keyword: &str,
        scope: SearchScope,
        limit: u32,
        offset: u32,
    ) -> MailResult<EmailPage> {
        let kw = keyword.trim();
        if kw.is_empty() {
            return Ok(EmailPage {
                items: Vec::new(),
                total: 0,
                next_offset: None,
            });
        }
        let q = SearchQuery::new(kw)
            .with_fields(scope.fields())
            .with_limit(limit)
            .with_offset(offset);
        lock_db(&self.db)?.search_page(account_id, &q)
    }
}
