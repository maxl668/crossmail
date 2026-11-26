//! UID 集合解析与格式化（RFC 3501 sequence-set 子集 / RFC 4315 UIDPLUS）。

use crossmail_core::error::{MailError, MailResult};

/// 闭区间 UID 范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UidRange {
    pub start: u32,
    pub end: u32,
}

impl UidRange {
    pub fn new(a: u32, b: u32) -> Self {
        if a <= b {
            Self { start: a, end: b }
        } else {
            Self { start: b, end: a }
        }
    }

    pub fn single(uid: u32) -> Self {
        Self {
            start: uid,
            end: uid,
        }
    }

    pub fn contains(&self, uid: u32) -> bool {
        uid >= self.start && uid <= self.end
    }

    pub fn len(&self) -> u64 {
        u64::from(self.end - self.start) + 1
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    /// 解析 `1:5,8,10:14` 形式的 UID set。
    pub fn parse_set(input: &str) -> MailResult<Vec<UidRange>> {
        let mut out = Vec::new();
        for part in input.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let (a, b) = match part.split_once(':') {
                Some((a, b)) => (parse_uid(a)?, parse_uid(b)?),
                None => {
                    let v = parse_uid(part)?;
                    (v, v)
                }
            };
            out.push(UidRange::new(a, b));
        }
        if out.is_empty() {
            return Err(MailError::Parse("UID 集合为空".into()));
        }
        Ok(out)
    }

    /// 合并后格式化为 IMAP sequence-set。
    pub fn format_set(ranges: &[UidRange]) -> String {
        if ranges.is_empty() {
            return String::new();
        }
        let mut sorted: Vec<UidRange> = ranges.to_vec();
        sorted.sort();
        let mut merged: Vec<UidRange> = Vec::with_capacity(sorted.len());
        for r in sorted {
            match merged.last_mut() {
                Some(last) if r.start <= last.end.saturating_add(1) => {
                    if r.end > last.end {
                        last.end = r.end;
                    }
                }
                _ => merged.push(r),
            }
        }
        merged
            .iter()
            .map(|r| {
                if r.start == r.end {
                    r.start.to_string()
                } else {
                    format!("{}:{}", r.start, r.end)
                }
            })
            .collect::<Vec<_>>()
            .join(",")
    }

    /// 从 `CopyUid` 响应中还原 源->目标 UID 映射（两侧数量必须一致）。
    pub fn pair(src: &[UidRange], dst: &[UidRange]) -> Vec<(u32, u32)> {
        let mut s = Vec::new();
        let mut d = Vec::new();
        for r in src {
            for uid in r.start..=r.end {
                s.push(uid);
            }
        }
        for r in dst {
            for uid in r.start..=r.end {
                d.push(uid);
            }
        }
        s.into_iter().zip(d).collect()
    }
}

fn parse_uid(s: &str) -> MailResult<u32> {
    s.trim()
        .trim_start_matches('*')
        .parse::<u32>()
        .map_err(|_| MailError::Parse(format!("非法 UID: {s}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mixed_set() {
        let r = UidRange::parse_set("1:5,8,10:14").unwrap();
        assert_eq!(r.len(), 3);
        assert_eq!(r[0], UidRange::new(1, 5));
        assert_eq!(r[1], UidRange::single(8));
    }

    #[test]
    fn reverse_range_normalized() {
        assert_eq!(UidRange::parse_set("9:3").unwrap()[0], UidRange::new(3, 9));
    }

    #[test]
    fn merge_overlapping() {
        let merged = UidRange::format_set(&[
            UidRange::new(1, 5),
            UidRange::new(6, 9),
            UidRange::single(20),
        ]);
        assert_eq!(merged, "1:9,20");
    }

    #[test]
    fn pair_copy_uid() {
        let pairs = UidRange::pair(
            &UidRange::parse_set("1:2").unwrap(),
            &UidRange::parse_set("10:11").unwrap(),
        );
        assert_eq!(pairs, vec![(1, 10), (2, 11)]);
    }

    #[test]
    fn empty_set_rejected() {
        assert!(UidRange::parse_set("").is_err());
    }
}
