//! 同步调度器（S3-07）。
//!
//! 四类触发（A-007）：启动同步 / 定时同步 / 操作触发 / 网络恢复。
//! 核心约束是**频繁操作不得重复请求**：操作类触发先进合并窗口，
//! 达到批量上限或窗口到期才真正发起一次同步。

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 应用启动
    Startup,
    /// 定时轮询到期
    Periodic,
    /// 本地发生一次邮件操作（标记/移动/发送…）
    Operation,
    /// 网络从断到通
    NetworkRecovered,
    /// 用户手动下拉刷新
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunReason {
    Startup,
    Periodic,
    OperationBatch,
    NetworkRecovered,
    Manual,
}

/// 调度决策：`run` 为真时上层才真正发起同步。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Decision {
    pub run: bool,
    /// 是否顺带回放离线队列（启动 / 网络恢复 / 手动时）
    pub flush_offline: bool,
    pub reason: Option<RunReason>,
}

impl Decision {
    fn idle() -> Self {
        Self::default()
    }

    fn go(reason: RunReason, flush_offline: bool) -> Self {
        Self {
            run: true,
            flush_offline,
            reason: Some(reason),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// 定时同步间隔（秒）
    pub poll_interval_secs: u64,
    /// 操作合并窗口（毫秒）：窗口内多次操作合并成一次同步
    pub debounce_ms: i64,
    /// 待合并操作达到该数量立即同步，不必等窗口到期
    pub batch_limit: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 300,
            debounce_ms: 1500,
            batch_limit: 10,
        }
    }
}

pub struct Scheduler {
    cfg: SchedulerConfig,
    last_run_at: Option<DateTime<Utc>>,
    pending_ops: usize,
    first_pending_at: Option<DateTime<Utc>>,
}

impl Scheduler {
    pub fn new(cfg: SchedulerConfig) -> Self {
        Self {
            cfg,
            last_run_at: None,
            pending_ops: 0,
            first_pending_at: None,
        }
    }

    pub fn config(&self) -> &SchedulerConfig {
        &self.cfg
    }

    pub fn pending_ops(&self) -> usize {
        self.pending_ops
    }

    pub fn last_run_at(&self) -> Option<DateTime<Utc>> {
        self.last_run_at
    }

    /// 处理一个触发事件。
    pub fn on_trigger(&mut self, trigger: Trigger, now: DateTime<Utc>) -> Decision {
        match trigger {
            // 启动与网络恢复都需要把离线积压补上
            Trigger::Startup => self.fire(RunReason::Startup, true, now),
            Trigger::NetworkRecovered => self.fire(RunReason::NetworkRecovered, true, now),
            Trigger::Manual => self.fire(RunReason::Manual, true, now),
            Trigger::Periodic => {
                if self.is_due(now) {
                    self.fire(RunReason::Periodic, false, now)
                } else {
                    Decision::idle()
                }
            }
            Trigger::Operation => {
                self.pending_ops += 1;
                let first = *self.first_pending_at.get_or_insert(now);
                let window_elapsed = self.window_elapsed(first, now);
                if self.pending_ops >= self.cfg.batch_limit.max(1) || window_elapsed {
                    self.fire(RunReason::OperationBatch, false, now)
                } else {
                    Decision::idle()
                }
            }
        }
    }

    /// 由上层定时驱动（UI 帧循环 / 客户端心跳）调用，用于检查合并窗口与轮询到期。
    pub fn tick(&mut self, now: DateTime<Utc>) -> Decision {
        if let Some(first) = self.first_pending_at {
            // 合并窗口内不得再发一次「定时同步」，否则批量合并形同虚设
            return if self.window_elapsed(first, now) {
                self.fire(RunReason::OperationBatch, false, now)
            } else {
                Decision::idle()
            };
        }
        if self.is_due(now) {
            return self.fire(RunReason::Periodic, false, now);
        }
        Decision::idle()
    }

    fn window_elapsed(&self, first: DateTime<Utc>, now: DateTime<Utc>) -> bool {
        (now - first).num_milliseconds() >= self.cfg.debounce_ms
    }

    fn is_due(&self, now: DateTime<Utc>) -> bool {
        match self.last_run_at {
            None => true,
            Some(last) => (now - last).num_seconds() >= self.cfg.poll_interval_secs as i64,
        }
    }

    fn fire(&mut self, reason: RunReason, flush_offline: bool, now: DateTime<Utc>) -> Decision {
        self.last_run_at = Some(now);
        self.pending_ops = 0;
        self.first_pending_at = None;
        Decision::go(reason, flush_offline)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn cfg() -> SchedulerConfig {
        SchedulerConfig {
            poll_interval_secs: 300,
            debounce_ms: 1500,
            batch_limit: 3,
        }
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap() + Duration::seconds(secs)
    }

    fn at_ms(ms: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).unwrap() + Duration::milliseconds(ms)
    }

    #[test]
    fn startup_runs_immediately_and_flushes_offline() {
        let mut s = Scheduler::new(cfg());
        let d = s.on_trigger(Trigger::Startup, at(0));
        assert!(d.run);
        assert!(d.flush_offline);
        assert_eq!(d.reason, Some(RunReason::Startup));
    }

    #[test]
    fn periodic_respects_interval() {
        let mut s = Scheduler::new(cfg());
        assert!(s.on_trigger(Trigger::Periodic, at(0)).run, "首次立即同步");
        assert!(
            !s.on_trigger(Trigger::Periodic, at(60)).run,
            "未到间隔不重复拉取"
        );
        assert!(s.on_trigger(Trigger::Periodic, at(300)).run, "到间隔后执行");
    }

    #[test]
    fn operations_coalesce_until_batch_limit_or_window() {
        let mut s = Scheduler::new(cfg());
        let d1 = s.on_trigger(Trigger::Operation, at(0));
        let d2 = s.on_trigger(Trigger::Operation, at(1));
        assert!(!d1.run && !d2.run, "窗口内多次操作合并，不逐条同步");
        assert_eq!(s.pending_ops(), 2);

        let d3 = s.on_trigger(Trigger::Operation, at(2));
        assert!(d3.run, "达到批量上限立即同步");
        assert_eq!(d3.reason, Some(RunReason::OperationBatch));
        assert_eq!(s.pending_ops(), 0);
    }

    #[test]
    fn debounce_window_flushes_on_tick() {
        let mut s = Scheduler::new(cfg());
        s.on_trigger(Trigger::Operation, at_ms(0));
        assert!(
            !s.tick(at_ms(1000)).run,
            "窗口未到期不触发（也不得改走定时同步）"
        );
        let d = s.tick(at_ms(1500));
        assert!(d.run);
        assert_eq!(d.reason, Some(RunReason::OperationBatch));
    }

    #[test]
    fn network_recovered_runs_and_flushes_offline() {
        let mut s = Scheduler::new(cfg());
        s.on_trigger(Trigger::Startup, at(0));
        let d = s.on_trigger(Trigger::NetworkRecovered, at(10));
        assert!(d.run);
        assert!(d.flush_offline, "网络恢复要顺带补发离线积压");
        assert_eq!(d.reason, Some(RunReason::NetworkRecovered));
    }

    #[test]
    fn burst_of_operations_yields_single_run() {
        let mut s = Scheduler::new(SchedulerConfig {
            batch_limit: 100,
            ..cfg()
        });
        let mut runs = 0;
        // 1 秒内连续 10 次操作，全部落在 1.5s 合并窗口内
        for i in 0..10 {
            if s.on_trigger(Trigger::Operation, at_ms(i * 100)).run {
                runs += 1;
            }
        }
        assert_eq!(runs, 0, "未到窗口/上限前不应发起任何同步");
        if s.tick(at_ms(1600)).run {
            runs += 1;
        }
        assert_eq!(runs, 1, "一波操作只合并成一次同步");
    }
}
