//! 冲突裁决（A-005）：删除 > 移动 > 标记。
//!
//! 裁决前提：**服务端是权威副本**，本地只有在「存在未上传的离线操作」时才对相应维度有话语权。
//! 这样既能保证多端最终一致，又不会把用户刚做的离线操作覆盖掉。

use crossmail_core::model::{Email, Flag, Id};
use crossmail_core::provider::OperationKind;

/// 冲突维度，数值越大优先级越高（与 `conflict_priority` 保持同一套排序）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Dimension {
    Flag,
    Move,
    Delete,
}

/// 合并结果：为什么这样合并，需要能被上层解释（诊断/日志/回归断言）。
#[derive(Debug, Clone, PartialEq)]
pub struct MergeOutcome {
    pub email: Email,
    /// 命中的最高优先级冲突维度；无冲突时为 None
    pub winner: Option<Dimension>,
    /// 覆盖了服务端状态的本地操作条数
    pub local_overrides: usize,
}

/// 取与该邮件相关的离线操作（保持入队顺序）。
pub fn pending_ops_for_email<'a>(
    ops: &'a [OperationKind],
    email_id: &Id,
) -> Vec<&'a OperationKind> {
    ops.iter()
        .filter(|op| match op {
            OperationKind::SetFlag { email_id: id, .. } => id == email_id,
            OperationKind::Move { email_ids, .. }
            | OperationKind::Copy { email_ids, .. }
            | OperationKind::Delete { email_ids, .. }
            | OperationKind::Archive { email_ids, .. } => email_ids.iter().any(|i| i == email_id),
            OperationKind::Send { .. } | OperationKind::CreateMailbox { .. } => false,
        })
        .collect()
}

/// 三维度合并：以远端为基线，叠加本地尚未上传的操作。
pub fn merge(local: &Email, remote: &Email, pending: &[&OperationKind]) -> MergeOutcome {
    let mut merged = remote.clone();
    let mut winner: Option<Dimension> = None;
    let mut overrides = 0usize;

    let has_delete = pending
        .iter()
        .any(|op| matches!(op, OperationKind::Delete { .. }));
    let has_move = pending.iter().any(|op| {
        matches!(
            op,
            OperationKind::Move { .. } | OperationKind::Archive { .. } | OperationKind::Copy { .. }
        )
    });

    // 维度一：删除（最高优先级）——已删除的邮件不再参与移动
    if has_delete {
        merged.set_flag(Flag::Deleted, true);
        winner = Some(Dimension::Delete);
        overrides += 1;
    }

    // 维度二：移动/归档/复制——只有未删除时才把本地归属写回
    if has_move && !has_delete {
        merged.mailbox_ids = local.mailbox_ids.clone();
        winner = Some(Dimension::Move);
        overrides += 1;
    }

    // 维度三：标记——按入队顺序重放，保证「最后一次操作生效」
    for op in pending {
        if let OperationKind::SetFlag { flag, enabled, .. } = op {
            merged.set_flag(flag.clone(), *enabled);
            overrides += 1;
            if winner.is_none() {
                winner = Some(Dimension::Flag);
            }
        }
    }

    // 正文只在远端没给时保留本地缓存，避免增量同步把已缓存正文清空（I-005 / J-005）
    if merged.text_body.is_none() {
        merged.text_body = local.text_body.clone();
    }
    if merged.html_body.is_none() {
        merged.html_body = local.html_body.clone();
    }
    if merged.preview.is_empty() {
        merged.preview = local.preview.clone();
    }
    // 远端不返回 UID / blob 时保留本地值（IMAP 主键反解依赖它）
    if merged.uid.is_none() {
        merged.uid = local.uid;
    }
    if merged.blob_id.is_none() {
        merged.blob_id = local.blob_id.clone();
    }

    MergeOutcome {
        email: merged,
        winner,
        local_overrides: overrides,
    }
}
