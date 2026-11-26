//! IMAP 增量同步策略（需求 I-004：UIDPLUS / CONDSTORE，不支持则降级）。

use crate::capability::Capabilities;

/// 本地已保存的同步游标。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncState {
    pub uid_validity: Option<u32>,
    /// 已同步的最大 UID
    pub last_uid: Option<u32>,
    pub highest_modseq: Option<u64>,
}

/// SELECT 后从服务端读到的状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerState {
    pub uid_validity: u32,
    /// 下一个待分配的 UID
    pub uid_next: u32,
    pub highest_modseq: Option<u64>,
    pub exists: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStrategy {
    /// 无新邮件且标记无变化
    UpToDate,
    /// UIDVALIDITY 变化或首次同步，必须全量校准
    Full,
    /// CONDSTORE：按 MODSEQ 拉取标记变化
    CondStore { since_modseq: u64 },
    /// UIDPLUS：按 UID 区间拉取新邮件
    UidRange { last_uid: u32 },
    /// 仅 UID 范围（无 UIDPLUS 也按最大 UID 拉取，需客户端自行去重）
    UidRangeOnly { last_uid: u32 },
}

/// 同步执行计划：一次 FETCH 所需参数全部落在这里，便于单测。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncPlan {
    pub strategy: SyncStrategy,
    /// FETCH 的 UID 集合，如 `100:*`
    pub uid_set: String,
    /// 是否追加 `(CHANGEDSINCE n)`
    pub changedsince: Option<u64>,
    /// UIDVALIDITY 变化：本地必须清空该文件夹缓存
    pub reset_cache: bool,
    /// 需要额外做一次纯 FLAGS 扫描
    pub need_flag_scan: bool,
}

impl SyncPlan {
    /// 生成 FETCH 命令的 UID 部分。
    pub fn fetch_uid_clause(&self) -> String {
        match self.changedsince {
            Some(m) => format!("{} (CHANGEDSINCE {})", self.uid_set, m),
            None => self.uid_set.clone(),
        }
    }

    pub fn is_noop(&self) -> bool {
        matches!(self.strategy, SyncStrategy::UpToDate)
    }

    /// UID 区间策略的游标下界（含）；其它策略没有下界。
    pub fn cursor_floor(&self) -> Option<u32> {
        match self.strategy {
            SyncStrategy::UidRange { last_uid } | SyncStrategy::UidRangeOnly { last_uid } => {
                Some(last_uid)
            }
            _ => None,
        }
    }
}

/// 取回的消息是否真的「比游标新」。
///
/// `UID FETCH {n}:*` 里的 `*` 是**当前最大 UID**，而 IMAP 序列集不区分方向
/// （RFC 3501 §9：`n:*` 在 n 大于最大 UID 时被归一化成 `max_uid:n`）。
/// 于是当**最新那封邮件被删除**时，UIDNEXT 继续往前走而最大 UID 回退，
/// `last_uid+1` 永久大于最大 UID，最后一封老邮件会被当成新邮件反复吐出来。
///
/// 真机复现：删掉收件箱最新一封后，每轮增量同步都重复下发同一封旧邮件。
/// 服务端行为合法，客户端必须自己兜——按游标下界过滤。
pub fn is_newer_than_cursor(plan: &SyncPlan, uid: Option<u32>) -> bool {
    match (plan.cursor_floor(), uid) {
        (Some(floor), Some(uid)) => uid > floor,
        // 拿不到的 UID 不归这条规则管，交给上层按 id 去重
        _ => true,
    }
}

/// 依据能力与游标制定同步计划（需求 I-004 降级链）。
pub fn plan_sync(caps: &Capabilities, server: &ServerState, local: &SyncState) -> SyncPlan {
    // 1. UIDVALIDITY 变化 => 全量校准
    if local.uid_validity != Some(server.uid_validity) {
        return SyncPlan {
            strategy: SyncStrategy::Full,
            uid_set: "1:*".into(),
            changedsince: None,
            reset_cache: local.uid_validity.is_some(),
            need_flag_scan: true,
        };
    }

    // 2. CONDSTORE：MODSEQ 相同则完全无变化
    if caps.supports_condstore() {
        if let (Some(local_modseq), Some(server_modseq)) =
            (local.highest_modseq, server.highest_modseq)
        {
            if local_modseq >= server_modseq {
                return SyncPlan {
                    strategy: SyncStrategy::UpToDate,
                    uid_set: String::new(),
                    changedsince: None,
                    reset_cache: false,
                    need_flag_scan: false,
                };
            }
            return SyncPlan {
                strategy: SyncStrategy::CondStore {
                    since_modseq: local_modseq,
                },
                uid_set: "1:*".into(),
                changedsince: Some(local_modseq),
                reset_cache: false,
                need_flag_scan: true,
            };
        }
    }

    // 3. UID 区间：已有游标 => 只拉新 UID
    if let Some(last_uid) = local.last_uid {
        if last_uid >= server.uid_next.saturating_sub(1) {
            return SyncPlan {
                strategy: SyncStrategy::UpToDate,
                uid_set: String::new(),
                changedsince: None,
                reset_cache: false,
                need_flag_scan: false,
            };
        }
        let strategy = if caps.supports_uidplus() {
            SyncStrategy::UidRange { last_uid }
        } else {
            SyncStrategy::UidRangeOnly { last_uid }
        };
        return SyncPlan {
            strategy,
            uid_set: format!("{}:*", last_uid + 1),
            changedsince: None,
            reset_cache: false,
            need_flag_scan: false,
        };
    }

    // 4. 首次同步
    SyncPlan {
        strategy: SyncStrategy::Full,
        uid_set: "1:*".into(),
        changedsince: None,
        reset_cache: false,
        need_flag_scan: false,
    }
}

/// 从 FETCH 结果推进游标。
pub fn advance_state(local: &mut SyncState, server: &ServerState, max_uid: Option<u32>) {
    local.uid_validity = Some(server.uid_validity);
    let candidate = max_uid.unwrap_or_else(|| server.uid_next.saturating_sub(1));
    local.last_uid = Some(match local.last_uid {
        Some(prev) => prev.max(candidate),
        None => candidate,
    });
    if let Some(m) = server.highest_modseq {
        local.highest_modseq = Some(match local.highest_modseq {
            Some(prev) => prev.max(m),
            None => m,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(items: &[&str]) -> Capabilities {
        Capabilities::from_items(items.iter().copied())
    }

    fn server(uid_validity: u32, uid_next: u32, modseq: Option<u64>) -> ServerState {
        ServerState {
            uid_validity,
            uid_next,
            highest_modseq: modseq,
            exists: 10,
        }
    }

    #[test]
    fn first_sync_is_full() {
        let plan = plan_sync(
            &caps(&["IMAP4rev1"]),
            &server(1, 10, None),
            &SyncState::default(),
        );
        assert_eq!(plan.strategy, SyncStrategy::Full);
        assert_eq!(plan.uid_set, "1:*");
        assert!(!plan.reset_cache);
    }

    #[test]
    fn uidvalidity_change_triggers_reset() {
        let local = SyncState {
            uid_validity: Some(1),
            last_uid: Some(9),
            highest_modseq: None,
        };
        let plan = plan_sync(&caps(&["UIDPLUS"]), &server(2, 10, None), &local);
        assert_eq!(plan.strategy, SyncStrategy::Full);
        assert!(plan.reset_cache);
        assert!(plan.need_flag_scan);
    }

    #[test]
    fn condstore_same_modseq_is_noop() {
        let local = SyncState {
            uid_validity: Some(1),
            last_uid: Some(9),
            highest_modseq: Some(100),
        };
        let plan = plan_sync(&caps(&["CONDSTORE"]), &server(1, 10, Some(100)), &local);
        assert!(plan.is_noop());
        assert_eq!(plan.uid_set, "");
    }

    #[test]
    fn condstore_uses_changedsince() {
        let local = SyncState {
            uid_validity: Some(1),
            last_uid: Some(9),
            highest_modseq: Some(100),
        };
        let plan = plan_sync(&caps(&["CONDSTORE"]), &server(1, 30, Some(150)), &local);
        assert_eq!(plan.strategy, SyncStrategy::CondStore { since_modseq: 100 });
        assert_eq!(plan.fetch_uid_clause(), "1:* (CHANGEDSINCE 100)");
        assert!(plan.need_flag_scan);
    }

    #[test]
    fn uidplus_incremental_range() {
        let local = SyncState {
            uid_validity: Some(1),
            last_uid: Some(20),
            highest_modseq: None,
        };
        let plan = plan_sync(&caps(&["UIDPLUS"]), &server(1, 30, None), &local);
        assert_eq!(plan.strategy, SyncStrategy::UidRange { last_uid: 20 });
        assert_eq!(plan.uid_set, "21:*");
    }

    #[test]
    fn no_uidplus_still_incremental() {
        let local = SyncState {
            uid_validity: Some(1),
            last_uid: Some(20),
            highest_modseq: None,
        };
        let plan = plan_sync(&caps(&["IMAP4rev1"]), &server(1, 30, None), &local);
        assert_eq!(plan.strategy, SyncStrategy::UidRangeOnly { last_uid: 20 });
        assert_eq!(plan.fetch_uid_clause(), "21:*");
    }

    #[test]
    fn no_new_mail_is_noop() {
        let local = SyncState {
            uid_validity: Some(1),
            last_uid: Some(29),
            highest_modseq: None,
        };
        let plan = plan_sync(&caps(&["UIDPLUS"]), &server(1, 30, None), &local);
        assert!(plan.is_noop());
    }

    #[test]
    fn advance_state_is_monotonic() {
        let mut local = SyncState::default();
        advance_state(&mut local, &server(3, 50, Some(10)), Some(12));
        assert_eq!(local.uid_validity, Some(3));
        assert_eq!(local.last_uid, Some(12));
        advance_state(&mut local, &server(3, 60, Some(30)), Some(15));
        assert_eq!(local.last_uid, Some(15));
        assert_eq!(local.highest_modseq, Some(30));
        // 回落不生效
        advance_state(&mut local, &server(3, 60, Some(20)), Some(4));
        assert_eq!(local.last_uid, Some(15));
        assert_eq!(local.highest_modseq, Some(30));
    }
}

#[cfg(test)]
mod cursor_floor_tests {
    use super::*;

    fn caps(items: &[&str]) -> Capabilities {
        Capabilities::from_items(items)
    }

    /// 真机翻车场景：删掉**最新那封**邮件后，UIDNEXT 继续往前走（51），
    /// 但最大 UID 回退到 49，游标 last_uid 也停在 49 —— 于是 `50:*` 被服务端
    /// 归一化成 `49:50`，把 uid=49 的老邮件当成新邮件反复吐出来。
    /// 客户端必须按游标下界自我过滤，服务端行为是合法的。
    #[test]
    fn uid_range_filters_message_at_or_below_cursor() {
        let local = SyncState {
            uid_validity: Some(1),
            last_uid: Some(49),
            highest_modseq: None,
        };
        let plan = plan_sync(
            &caps(&["UIDPLUS"]),
            &ServerState {
                uid_validity: 1,
                uid_next: 51,
                highest_modseq: None,
                exists: 46,
            },
            &local,
        );
        assert_eq!(plan.strategy, SyncStrategy::UidRange { last_uid: 49 });
        assert_eq!(plan.fetch_uid_clause(), "50:*");
        // 服务端把 50:* 归一化成 49:50 后回吐的 uid=49 必须被丢弃
        assert!(!is_newer_than_cursor(&plan, Some(49)));
        // 真正的新邮件保留
        assert!(is_newer_than_cursor(&plan, Some(50)));
        assert!(is_newer_than_cursor(&plan, Some(51)));
    }

    /// 非 UID 区间策略（全量 / CONDSTORE）没有下界概念，不能误杀标记变化。
    #[test]
    fn full_and_condstore_plans_have_no_floor() {
        let first = plan_sync(
            &caps(&["UIDPLUS"]),
            &ServerState {
                uid_validity: 1,
                uid_next: 10,
                highest_modseq: None,
                exists: 3,
            },
            &SyncState::default(),
        );
        assert_eq!(first.cursor_floor(), None);
        assert!(is_newer_than_cursor(&first, Some(1)));

        let cs = plan_sync(
            &caps(&["CONDSTORE"]),
            &ServerState {
                uid_validity: 1,
                uid_next: 30,
                highest_modseq: Some(150),
                exists: 20,
            },
            &SyncState {
                uid_validity: Some(1),
                last_uid: Some(20),
                highest_modseq: Some(100),
            },
        );
        assert_eq!(cs.cursor_floor(), None);
        // CONDSTORE 拿的是「标记变化」，uid 比游标小是正常现象，绝不能丢
        assert!(is_newer_than_cursor(&cs, Some(3)));
    }
}
