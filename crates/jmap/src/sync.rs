//! JMAP 增量同步（J-004 / J-011 / A-004 / A-005）。
//!
//! sinceState 变更集优先；服务端返回 cannotCalculateChanges / 状态冲突时自动全量校准，
//! 保证「无重复、无遗漏」。所有长循环检查取消令牌并上报进度。

use crate::client::JmapClient;
use crate::transport::Transport;
use chrono::{DateTime, Utc};
use crossmail_core::error::{MailError, MailResult};
use crossmail_core::model::{Email, Id};
use crossmail_core::provider::{
    CancellationToken, Progress, ProgressReporter, SyncPage, SyncPhase, SyncRequest,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryPage {
    pub ids: Vec<Id>,
    pub total: u64,
    pub position: u32,
    pub can_calculate_changes: bool,
    pub query_state: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Changes {
    pub old_state: String,
    pub new_state: String,
    pub created: Vec<Id>,
    pub updated: Vec<Id>,
    pub destroyed: Vec<Id>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SetOutcome {
    pub created: BTreeMap<String, Value>,
    pub updated: BTreeMap<String, Value>,
    pub destroyed: Vec<Id>,
    pub not_created: BTreeMap<String, Value>,
    pub not_updated: BTreeMap<String, Value>,
    pub not_destroyed: BTreeMap<String, Value>,
    pub new_state: Option<String>,
}

pub type MailboxSetOutcome = SetOutcome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmissionOutcome {
    pub id: Id,
    pub email_id: Id,
    pub sent_at: Option<DateTime<Utc>>,
}

impl<T: Transport> JmapClient<T> {
    /// 一次完整增量同步。
    pub async fn sync_emails(
        &self,
        req: &SyncRequest,
        cancel: &CancellationToken,
        reporter: Option<&dyn ProgressReporter>,
    ) -> MailResult<SyncPage> {
        cancel.check()?;
        let mut page = SyncPage::default();
        let mut ids: BTreeSet<Id> = BTreeSet::new();
        let mut deleted: BTreeSet<Id> = BTreeSet::new();

        match req.since_state.as_deref() {
            Some(state) if !state.is_empty() => {
                match self
                    .collect_changes(state, cancel, &mut ids, &mut deleted)
                    .await
                {
                    Ok(new_state) => page.new_state = Some(new_state),
                    Err(err) if err.requires_full_resync() => {
                        page.full_resync = true;
                        let query = self.full_query(req, cancel).await?;
                        ids.extend(query.ids);
                        page.new_state = query.query_state;
                        page.has_more = query.total > u64::from(req.limit);
                    }
                    Err(err) => return Err(err),
                }
            }
            _ => {
                let query = self.full_query(req, cancel).await?;
                ids.extend(query.ids);
                page.new_state = query.query_state;
                page.has_more = query.total > u64::from(req.limit);
            }
        }

        let all_ids: Vec<Id> = ids.into_iter().collect();
        let total = all_ids.len() as u64;
        report(
            reporter,
            Progress {
                phase: SyncPhase::Headers,
                processed: 0,
                total,
            },
        );

        let chunk = self.session().await?.max_objects_in_get().max(1) as usize;
        for (i, batch) in all_ids.chunks(chunk).enumerate() {
            cancel.check()?;
            let emails = self.email_get(batch, req.include_body).await?;
            page.emails.extend(emails);
            report(
                reporter,
                Progress {
                    phase: SyncPhase::Bodies,
                    processed: (((i + 1) * chunk).min(all_ids.len())) as u64,
                    total,
                },
            );
        }

        dedup_emails(&mut page.emails);
        page.deleted_ids = deleted.into_iter().collect();
        report(
            reporter,
            Progress {
                phase: SyncPhase::Done,
                processed: page.emails.len() as u64,
                total: page.emails.len() as u64,
            },
        );
        Ok(page)
    }

    async fn full_query(
        &self,
        req: &SyncRequest,
        cancel: &CancellationToken,
    ) -> MailResult<QueryPage> {
        cancel.check()?;
        let filter = req
            .mailbox_id
            .as_ref()
            .map(|id| serde_json::json!({ "inMailbox": id }));
        let query = self.email_query(filter, req.limit, 0).await?;
        Ok(query)
    }

    /// 翻页拉取变更集；`hasMoreChanges` 为 true 时继续，直到收齐或取消。
    async fn collect_changes(
        &self,
        initial_state: &str,
        cancel: &CancellationToken,
        ids: &mut BTreeSet<Id>,
        deleted: &mut BTreeSet<Id>,
    ) -> MailResult<String> {
        let mut state = initial_state.to_string();
        loop {
            cancel.check()?;
            let changes = self.email_changes(&state, 500).await?;
            ids.extend(changes.created);
            ids.extend(changes.updated);
            let destroyed_ids = changes.destroyed;
            deleted.extend(destroyed_ids.iter().cloned());
            // 同一封邮件既被更新又被删除时，以删除为准（冲突优先级 A-005）
            for id in &destroyed_ids {
                ids.remove(id);
            }
            state = changes.new_state;
            if !changes.has_more {
                break;
            }
        }
        Ok(state)
    }
}

fn dedup_emails(emails: &mut Vec<Email>) {
    let mut seen = BTreeSet::new();
    emails.retain(|e| seen.insert(e.id.clone()));
}

fn report(reporter: Option<&dyn ProgressReporter>, progress: Progress) {
    if let Some(r) = reporter {
        r.report(progress);
    }
}

/// 解析 /query 响应；`canCalculateChanges=false` 时上层不得依赖 state。
pub fn parse_query(v: &Value) -> QueryPage {
    QueryPage {
        ids: v
            .get("ids")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        total: v.get("total").and_then(Value::as_u64).unwrap_or(0),
        position: v.get("position").and_then(Value::as_u64).unwrap_or(0) as u32,
        can_calculate_changes: v
            .get("canCalculateChanges")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        query_state: v
            .get("queryState")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

pub fn parse_changes(v: &Value) -> Changes {
    let list = |key: &str| -> Vec<Id> {
        v.get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Changes {
        old_state: v
            .get("oldState")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        new_state: v
            .get("newState")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        created: list("created"),
        updated: list("updated"),
        destroyed: list("destroyed"),
        has_more: v
            .get("hasMoreChanges")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// /set 响应解析：把 notCreated / notUpdated / notDestroyed 提升为可判定的错误。
pub fn parse_set(v: &Value) -> MailResult<SetOutcome> {
    let object = |key: &str| -> BTreeMap<String, Value> {
        v.get(key)
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, x)| (k.clone(), x.clone())).collect())
            .unwrap_or_default()
    };
    let outcome = SetOutcome {
        created: object("created"),
        updated: object("updated"),
        destroyed: v
            .get("destroyed")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        not_created: object("notCreated"),
        not_updated: object("notUpdated"),
        not_destroyed: object("notDestroyed"),
        new_state: v
            .get("newState")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    if !outcome.not_created.is_empty()
        || !outcome.not_updated.is_empty()
        || !outcome.not_destroyed.is_empty()
    {
        // 只报数量等于把诊断信息全扔掉：RFC 8620 §5.3 对每个失败对象都给了
        // `{"type": "...", "description": "..."}`（notFound / forbidden /
        // mailboxHasEmail / invalidProperties / tooManyKeywords ...），
        // 不同 type 对应完全不同的处置方式（重试 / 换 id / 先清空 / 放弃）。
        // 真机上「销毁文件夹失败」就是这样：只看到 notDestroyed=1 时，
        // 完全无法区分「文件夹不存在」和「文件夹里还有邮件」。
        return Err(MailError::Protocol(format!(
            "JMAP /set 部分失败: {}",
            describe_set_failures(&outcome)
        )));
    }
    Ok(outcome)
}

/// 把 `/set` 的失败对象摊平成「id=type(description)」的可读串。
pub(crate) fn describe_set_failures(outcome: &SetOutcome) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (label, map) in [
        ("notCreated", &outcome.not_created),
        ("notUpdated", &outcome.not_updated),
        ("notDestroyed", &outcome.not_destroyed),
    ] {
        for (id, err) in map {
            let kind = err.get("type").and_then(Value::as_str).unwrap_or("unknown");
            let desc = err.get("description").and_then(Value::as_str);
            let properties = err.get("properties").and_then(Value::as_array);
            let mut detail = kind.to_string();
            if let Some(d) = desc {
                detail.push_str(&format!(": {d}"));
            }
            if let Some(props) = properties {
                let names: Vec<&str> = props.iter().filter_map(Value::as_str).collect();
                if !names.is_empty() {
                    detail.push_str(&format!(" [属性: {}]", names.join(",")));
                }
            }
            parts.push(format!("{label}[{id}]={detail}"));
        }
    }
    if parts.is_empty() {
        "（无失败对象）".to_string()
    } else {
        parts.join("; ")
    }
}
