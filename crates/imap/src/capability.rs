//! IMAP 能力协商（需求 I-001 / I-004 / I-007 / I-008 / I-011）。

use crossmail_core::provider::ProviderCapabilities;
use std::collections::BTreeSet;

/// 服务端 CAPABILITY 集合，大小写不敏感。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    items: BTreeSet<String>,
}

impl Capabilities {
    pub fn from_items<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            items: items
                .into_iter()
                .map(|s| s.as_ref().trim().to_ascii_uppercase())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    /// 解析 `* CAPABILITY IMAP4rev1 UIDPLUS ...` 或 `IMAP4rev1 UIDPLUS ...`。
    pub fn parse(line: &str) -> Self {
        let mut body = line.trim();
        if let Some(rest) = body.strip_prefix('*') {
            body = rest.trim();
        }
        if let Some(rest) = body.strip_prefix('[') {
            body = rest;
        }
        let body = body.trim_end_matches(']').trim();
        let lower = body.to_ascii_uppercase();
        let payload = if lower.starts_with("CAPABILITY") {
            body[10..].trim()
        } else {
            body
        };
        Self::from_items(payload.split_whitespace())
    }

    pub fn has(&self, name: &str) -> bool {
        self.items.contains(&name.to_ascii_uppercase())
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &String> {
        self.items.iter()
    }

    pub fn extend(&mut self, other: &Capabilities) {
        self.items.extend(other.items.iter().cloned());
    }

    /// 是否 IMAP4rev2（RFC 9051）。rev2 把多个扩展吸收进核心，
    /// 服务器**不会再单独列出**这些扩展名，必须按版本推断，否则会把能力判成不支持。
    pub fn is_rev2(&self) -> bool {
        self.has("IMAP4rev2")
    }

    /// UIDPLUS：rev2 核心包含 UID EXPUNGE / APPENDUID / COPYUID，无需单独声明。
    pub fn supports_uidplus(&self) -> bool {
        self.has("UIDPLUS") || self.is_rev2()
    }

    /// CONDSTORE 或 ENABLE（RFC 7162 中 ENABLE 是 CONDSTORE 的前置能力）。
    pub fn supports_condstore(&self) -> bool {
        self.has("CONDSTORE") || self.has("QRESYNC")
    }

    pub fn supports_enable(&self) -> bool {
        self.has("ENABLE") || self.is_rev2()
    }

    /// MOVE：rev2 核心包含，服务器通常不再单独列出 `MOVE`。
    pub fn supports_move(&self) -> bool {
        self.has("MOVE") || self.is_rev2()
    }

    pub fn supports_idle(&self) -> bool {
        self.has("IDLE")
    }

    /// Gmail 标签扩展（I-011）。
    pub fn is_gmail(&self) -> bool {
        self.has("X-GM-EXT-1")
    }

    /// AUTH=*** 机制列表。
    pub fn auth_mechanisms(&self) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|c| c.strip_prefix("AUTH=").map(|s| s.to_string()))
            .collect()
    }

    /// 统一能力集（不支持的能力自动降级，需求 I-004 / I-007 / I-008）。
    pub fn to_provider_capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            jmap_mail: false,
            condstore: self.supports_condstore(),
            uidplus: self.supports_uidplus(),
            imap_move: self.supports_move(),
            imap_idle: self.supports_idle(),
            gmail_labels: self.is_gmail(),
            push: self.supports_idle(),
            blob_upload: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_greeting_capability() {
        let caps = Capabilities::parse("* CAPABILITY IMAP4rev1 UIDPLUS MOVE IDLE CONDSTORE");
        assert!(caps.has("imap4rev1"));
        assert!(caps.supports_uidplus());
        assert!(caps.supports_move());
        assert!(caps.supports_idle());
        assert!(caps.supports_condstore());
    }

    #[test]
    fn parse_response_code_form() {
        let caps = Capabilities::parse("[CAPABILITY IMAP4rev1 AUTH=PLAIN AUTH=LOGIN]");
        assert!(caps.has("IMAP4rev1"));
        assert_eq!(caps.auth_mechanisms(), vec!["LOGIN", "PLAIN"]);
    }

    #[test]
    fn enable_implies_condstore_support() {
        let caps = Capabilities::from_items(["IMAP4rev1", "ENABLE", "QRESYNC"]);
        assert!(caps.supports_condstore());
        assert!(caps.supports_enable());
    }

    #[test]
    fn gmail_detection_and_degradation() {
        let caps = Capabilities::from_items(["IMAP4rev1", "X-GM-EXT-1"]);
        assert!(caps.is_gmail());
        let pc = caps.to_provider_capabilities();
        assert!(pc.gmail_labels);
        assert!(!pc.uidplus);
        assert!(!pc.imap_move);
        assert!(!pc.condstore);
    }

    #[test]
    fn greedy_extend_merges() {
        let mut a = Capabilities::from_items(["IMAP4rev1"]);
        a.extend(&Capabilities::from_items(["UIDPLUS"]));
        assert!(a.supports_uidplus());
    }
}
