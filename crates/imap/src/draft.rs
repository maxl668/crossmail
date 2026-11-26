//! 草稿合并（需求 I-010）：本地与服务端双向合并，冲突保留双版本。

use crossmail_core::model::Email;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftConflict {
    pub local_id: String,
    pub remote_id: String,
    pub subject: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MergeReport {
    /// 需要上传到服务端的草稿（新增或冲突副本）
    pub to_upload: Vec<Email>,
    /// 需要落到本地的服务端草稿
    pub to_adopt: Vec<Email>,
    /// 两边完全一致的草稿 id
    pub in_sync: Vec<String>,
    /// 冲突（两版都保留）
    pub conflicts: Vec<DraftConflict>,
}

impl MergeReport {
    pub fn has_changes(&self) -> bool {
        !self.to_upload.is_empty() || !self.to_adopt.is_empty()
    }
}

fn normalize(s: &str) -> String {
    s.trim().to_lowercase()
}

/// 匹配键：优先 Message-ID，退化到「标题 + 首收件人」。
fn match_key(draft: &Email) -> String {
    if let Some(mid) = draft.message_id.as_deref().filter(|m| !m.trim().is_empty()) {
        return format!("mid:{}", normalize(mid));
    }
    let to = draft
        .to
        .first()
        .map(|a| normalize(&a.email))
        .unwrap_or_default();
    format!("sub:{}\u{1}{}", normalize(&draft.subject), to)
}

/// 内容指纹：用于判定「是否有实质改动」。
pub fn fingerprint(draft: &Email) -> u64 {
    let mut h = DefaultHasher::new();
    draft.subject.hash(&mut h);
    for a in draft
        .to
        .iter()
        .chain(draft.cc.iter())
        .chain(draft.bcc.iter())
    {
        a.email.hash(&mut h);
    }
    draft.text_body.hash(&mut h);
    draft.html_body.hash(&mut h);
    for att in &draft.attachments {
        att.name.hash(&mut h);
        att.size.hash(&mut h);
    }
    h.finish()
}

/// 合并本地与服务端草稿。
///
/// 规则（I-010）：
/// - 仅本地有 → 上传；
/// - 仅服务端有 → 采纳到本地；
/// - 两边指纹一致 → 视为已同步；
/// - 指纹不同 → 记为冲突，**两版都保留**（本地作为副本上传，服务端原样保留）。
pub fn merge_drafts(local: &[Email], remote: &[Email]) -> MergeReport {
    let mut report = MergeReport::default();
    let mut used_remote: Vec<usize> = Vec::new();

    for l in local {
        let key = match_key(l);
        let found = remote
            .iter()
            .enumerate()
            .find(|(idx, r)| !used_remote.contains(idx) && match_key(r) == key);

        match found {
            None => report.to_upload.push(l.clone()),
            Some((idx, r)) => {
                used_remote.push(idx);
                if fingerprint(l) == fingerprint(r) {
                    report.in_sync.push(l.id.clone());
                } else {
                    report.conflicts.push(DraftConflict {
                        local_id: l.id.clone(),
                        remote_id: r.id.clone(),
                        subject: l.subject.clone(),
                    });
                    report.to_upload.push(l.clone());
                }
            }
        }
    }

    for (idx, r) in remote.iter().enumerate() {
        if !used_remote.contains(&idx) {
            report.to_adopt.push(r.clone());
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossmail_core::model::Address;

    fn draft(id: &str, subject: &str, body: &str, mid: Option<&str>) -> Email {
        let mut e = Email::new(id, "acc", "drafts");
        e.subject = subject.into();
        e.text_body = Some(body.into());
        e.to = vec![Address::new("bob@example.com")];
        e.message_id = mid.map(|m| m.to_string());
        e
    }

    #[test]
    fn local_only_uploaded() {
        let r = merge_drafts(&[draft("L1", "hi", "a", Some("<1@x>"))], &[]);
        assert_eq!(r.to_upload.len(), 1);
        assert!(r.conflicts.is_empty());
    }

    #[test]
    fn remote_only_adopted() {
        let r = merge_drafts(&[], &[draft("R1", "hi", "a", Some("<1@x>"))]);
        assert_eq!(r.to_adopt.len(), 1);
    }

    #[test]
    fn identical_content_in_sync() {
        let r = merge_drafts(
            &[draft("L1", "hi", "same", Some("<1@x>"))],
            &[draft("R1", "hi", "same", Some("<1@x>"))],
        );
        assert_eq!(r.in_sync, vec!["L1".to_string()]);
        assert!(!r.has_changes());
    }

    #[test]
    fn divergent_content_keeps_both_versions() {
        let r = merge_drafts(
            &[draft("L1", "hi", "local-edit", Some("<1@x>"))],
            &[draft("R1", "hi", "remote-edit", Some("<1@x>"))],
        );
        assert_eq!(r.conflicts.len(), 1);
        assert_eq!(r.conflicts[0].remote_id, "R1");
        // 冲突时本地版本仍上传（保留双版本），服务端版本不被覆盖
        assert_eq!(r.to_upload.len(), 1);
        assert!(r.to_adopt.is_empty());
    }

    #[test]
    fn fallback_key_without_message_id() {
        let l = draft("L1", "Report", "x", None);
        let mut r = draft("R1", " report ", "y", None);
        r.to = vec![Address::new("BOB@example.com")];
        let rep = merge_drafts(&[l], &[r]);
        // 标题大小写/空白与地址大小写归一后匹配成功 => 冲突而非新增
        assert_eq!(rep.conflicts.len(), 1);
    }
}
