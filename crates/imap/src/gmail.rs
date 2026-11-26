//! Gmail 标签体系适配（需求 I-011）：归档 = 移出 INBOX，删除 = 移入废件箱，永不 EXPUNGE。

use crate::client::quote;
use crate::uid::UidRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GmailAdapter {
    pub all_mail: String,
    pub trash: String,
    pub sent: String,
    pub drafts: String,
    pub important: String,
}

impl Default for GmailAdapter {
    fn default() -> Self {
        Self {
            all_mail: "[Gmail]/All Mail".into(),
            trash: "[Gmail]/Trash".into(),
            sent: "[Gmail]/Sent Mail".into(),
            drafts: "[Gmail]/Drafts".into(),
            important: "[Gmail]/Important".into(),
        }
    }
}

impl GmailAdapter {
    /// Gmail 的邮件永远不会被真正删除，EXPUNGE 会破坏用户预期。
    pub fn allow_expunge(&self) -> bool {
        false
    }

    fn set(uids: &[u32]) -> String {
        UidRange::format_set(
            &uids
                .iter()
                .map(|u| UidRange::single(*u))
                .collect::<Vec<_>>(),
        )
    }

    /// 归档：仅移除 INBOX 标签，邮件仍留在 All Mail（I-011 核心要求）。
    pub fn archive_commands(&self, uids: &[u32]) -> Vec<String> {
        if uids.is_empty() {
            return Vec::new();
        }
        vec![format!(
            "UID STORE {} -X-GM-LABELS.SILENT (\\Inbox)",
            Self::set(uids)
        )]
    }

    /// 删除：打上 \Trash 标签，不 EXPUNGE。
    pub fn delete_commands(&self, uids: &[u32], supports_move: bool) -> Vec<String> {
        if uids.is_empty() {
            return Vec::new();
        }
        if supports_move {
            vec![format!(
                "UID MOVE {} {}",
                Self::set(uids),
                quote(&self.trash)
            )]
        } else {
            vec![
                format!("UID COPY {} {}", Self::set(uids), quote(&self.trash)),
                format!("UID STORE {} +FLAGS.SILENT (\\Deleted)", Self::set(uids)),
            ]
        }
    }

    /// 标签增删（X-GM-LABELS 双向同步）。
    pub fn label_commands(&self, uids: &[u32], add: &[String], remove: &[String]) -> Vec<String> {
        if uids.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !add.is_empty() {
            out.push(format!(
                "UID STORE {} +X-GM-LABELS.SILENT ({})",
                Self::set(uids),
                add.join(" ")
            ));
        }
        if !remove.is_empty() {
            out.push(format!(
                "UID STORE {} -X-GM-LABELS.SILENT ({})",
                Self::set(uids),
                remove.join(" ")
            ));
        }
        out
    }

    /// 移动：COPY 到目标标签后再移除 INBOX，不 EXPUNGE。
    pub fn move_commands(&self, uids: &[u32], destination: &str) -> Vec<String> {
        if uids.is_empty() {
            return Vec::new();
        }
        vec![
            format!("UID COPY {} {}", Self::set(uids), quote(destination)),
            format!(
                "UID STORE {} -X-GM-LABELS.SILENT (\\Inbox)",
                Self::set(uids)
            ),
        ]
    }

    /// 把 Gmail 标签归一为统一 Flag 列表（系统标签映射为 \Flagged 等）。
    pub fn normalize_labels(labels: &[String]) -> Vec<String> {
        labels
            .iter()
            .map(|l| l.trim().trim_matches('"').to_string())
            .filter(|l| !l.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_removes_inbox_label_only() {
        let g = GmailAdapter::default();
        let cmds = g.archive_commands(&[1, 2, 3]);
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].contains("-X-GM-LABELS"));
        assert!(cmds[0].contains("\\Inbox"));
        assert!(!cmds[0].contains("EXPUNGE"));
    }

    #[test]
    fn delete_never_expunges() {
        let g = GmailAdapter::default();
        let with_move = g.delete_commands(&[5], true);
        assert!(with_move[0].starts_with("UID MOVE"));
        let without_move = g.delete_commands(&[5], false);
        assert_eq!(without_move.len(), 2);
        assert!(without_move.iter().all(|c| !c.contains("EXPUNGE")));
        assert!(!g.allow_expunge());
    }

    #[test]
    fn label_ops_merge_into_uidset() {
        let g = GmailAdapter::default();
        let cmds = g.label_commands(&[1, 2, 9], &["Work".into()], &["Old".into()]);
        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].contains("+X-GM-LABELS"));
        assert!(cmds[0].contains("1:2,9"));
    }

    #[test]
    fn empty_selection_produces_no_command() {
        let g = GmailAdapter::default();
        assert!(g.archive_commands(&[]).is_empty());
        assert!(g.move_commands(&[], g.all_mail.as_str()).is_empty());
    }

    #[test]
    fn move_uses_copy_then_unlabel_inbox() {
        let g = GmailAdapter::default();
        let cmds = g.move_commands(&[7], &g.all_mail);
        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].starts_with("UID COPY"));
        assert!(cmds[1].contains("-X-GM-LABELS"));
    }
}
