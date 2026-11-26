//! 发件人钓鱼判定（SEC-004）：把「这封邮件到底是不是它声称的那个人」拆成可解释的信号。
//!
//! 原则：**不做真伪断言，只给证据**。邮件客户端没有 DKIM/SPF 的完整上下文时，
//! 唯一能确定说的是「域名长得像谁」。所以这里输出的是 `warnings` 列表 + 一个
//! 汇总等级，UI 负责把人话提示放到发件人旁边，绝不拦截邮件本身。

use crossmail_core::Address;
use serde::{Deserialize, Serialize};

/// 风险等级，取所有命中的最严重项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustLevel {
    /// 未发现异常
    Safe,
    /// 需要留意（如 Reply-To 域与发件域不同、免费邮箱冒充品牌）
    Caution,
    /// 高度可疑（同形字域名、相似域名、IP 直连、子域伪装）
    Danger,
}

impl TrustLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            TrustLevel::Safe => "safe",
            TrustLevel::Caution => "caution",
            TrustLevel::Danger => "danger",
        }
    }
}

/// 单条可疑信号。枚举成员自带上下文，便于日志与诊断包直接落文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrustWarning {
    /// 显示名里写了一个邮箱/域名，但和实际发件域不一致
    DisplayNameSpoof { claimed: String, actual: String },
    /// 国际化域名（xn--）—— 解码后给用户看，避免同形字欺骗
    PunycodeDomain {
        domain: String,
        decoded: Option<String>,
    },
    /// 同一域名标签里混用拉丁与非拉丁字符
    MixedScript { domain: String },
    /// 发件域是 IP 字面量
    IpLiteralDomain { domain: String },
    /// 使用了高滥用率顶级域
    SuspiciousTld { domain: String, tld: String },
    /// 与知名品牌域名高度相似
    LookalikeDomain { domain: String, resembles: String },
    /// 品牌名被塞进子域（`apple.com.secure-login.cn` 式伪装）
    SubdomainTrick { domain: String, brand: String },
    /// Reply-To 与发件域不是同一个可注册域
    ReplyToMismatch {
        from_domain: String,
        reply_to_domain: String,
    },
    /// 免费邮箱地址却用了品牌显示名
    FreeMailForBrand { domain: String, brand: String },
}

impl TrustWarning {
    pub fn level(&self) -> TrustLevel {
        match self {
            TrustWarning::DisplayNameSpoof { .. }
            | TrustWarning::ReplyToMismatch { .. }
            | TrustWarning::FreeMailForBrand { .. } => TrustLevel::Caution,
            TrustWarning::PunycodeDomain { .. }
            | TrustWarning::MixedScript { .. }
            | TrustWarning::IpLiteralDomain { .. }
            | TrustWarning::SuspiciousTld { .. }
            | TrustWarning::LookalikeDomain { .. }
            | TrustWarning::SubdomainTrick { .. } => TrustLevel::Danger,
        }
    }

    /// 面向用户的一句话（中文，不含技术黑话，避免把用户教成攻击者）。
    pub fn describe(&self) -> String {
        match self {
            TrustWarning::DisplayNameSpoof { claimed, actual } => {
                format!("发件人名称声称是 {claimed}，但真实域名是 {actual}")
            }
            TrustWarning::PunycodeDomain { domain, decoded } => match decoded {
                Some(d) => format!("发件域名 {domain} 使用了国际化编码，实际含义为「{d}」"),
                None => format!("发件域名 {domain} 使用了国际化编码，可能是同形字伪装"),
            },
            TrustWarning::MixedScript { domain } => {
                format!("发件域名 {domain} 混用了不同语系的字符，可能是同形字伪装")
            }
            TrustWarning::IpLiteralDomain { domain } => {
                format!("发件人使用 IP 地址 {domain} 而非正常域名")
            }
            TrustWarning::SuspiciousTld { domain, tld } => {
                format!("发件域名 {domain} 的顶级域 .{tld} 常被用于钓鱼")
            }
            TrustWarning::LookalikeDomain { domain, resembles } => {
                format!("发件域名 {domain} 与 {resembles} 极为相似，请确认不是仿冒")
            }
            TrustWarning::SubdomainTrick { domain, brand } => {
                format!("发件域名 {domain} 把 {brand} 放在子域位置以伪装身份")
            }
            TrustWarning::ReplyToMismatch {
                from_domain,
                reply_to_domain,
            } => format!("回复地址（{reply_to_domain}）与发件域名（{from_domain}）不一致"),
            TrustWarning::FreeMailForBrand { domain, brand } => {
                format!("{brand} 使用免费邮箱域名 {domain} 发信，官方通常不会这样做")
            }
        }
    }
}

/// 发件人信任评估结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SenderTrust {
    pub address: String,
    pub display_name: Option<String>,
    /// 真实发件域（小写）
    pub domain: String,
    /// 可注册域（`mail.example.com.cn` → `example.com.cn`）
    pub registrable_domain: String,
    pub level: TrustLevel,
    pub warnings: Vec<TrustWarning>,
}

impl SenderTrust {
    /// 是否需要用警告样式高亮发件人
    pub fn suspicious(&self) -> bool {
        self.level != TrustLevel::Safe
    }

    /// 用户可见提示（多条用 `；` 连接），Safe 时为空串。
    pub fn summary(&self) -> String {
        self.warnings
            .iter()
            .map(|w| w.describe())
            .collect::<Vec<_>>()
            .join("；")
    }
}

/// 高滥用率顶级域（保留常见滥用项，不追求穷尽）。
const SUSPICIOUS_TLDS: &[&str] = &[
    "zip", "mov", "top", "xyz", "click", "link", "work", "gq", "tk", "ml", "cf", "ga", "buzz",
    "icu", "cam", "rest", "monster", "sbs", "cyou", "lol", "bond", "quest", "cfd", "support",
    "country", "kim", "gdn", "men", "loan",
];

/// 免费邮箱域名：品牌方几乎不会用它们做官方通知。
const FREE_MAIL_DOMAINS: &[&str] = &[
    "qq.com",
    "163.com",
    "126.com",
    "sina.com",
    "sohu.com",
    "gmail.com",
    "outlook.com",
    "hotmail.com",
    "yahoo.com",
    "foxmail.com",
    "yeah.net",
    "aliyun.com",
    "mail.ru",
];

/// 常见被冒充的品牌可注册域。
const BRAND_DOMAINS: &[&str] = &[
    "apple.com",
    "icloud.com",
    "google.com",
    "gmail.com",
    "microsoft.com",
    "outlook.com",
    "office.com",
    "live.com",
    "amazon.com",
    "paypal.com",
    "netflix.com",
    "facebook.com",
    "instagram.com",
    "whatsapp.com",
    "github.com",
    "dropbox.com",
    "steampowered.com",
    "linkedin.com",
    "alipay.com",
    "taobao.com",
    "tmall.com",
    "jd.com",
    "tencent.com",
    "weixin.qq.com",
    "alibaba.com",
    "icbc.com.cn",
    "ccb.com",
    "abchina.com",
    "boc.cn",
    "cmbchina.com",
    "unionpay.com",
    "bankofamerica.com",
    "wellsfargo.com",
    "chase.com",
    "dhl.com",
    "fedex.com",
    "ups.com",
];

/// 二级公共后缀白名单：这些后缀下的可注册域要取三段。
const MULTI_PART_SUFFIX: &[&str] = &[
    "com", "net", "org", "gov", "edu", "co", "ac", "or", "ne", "go", "gob", "mil", "sch", "nic",
];
/// 允许上面那批二级后缀的国家/地区顶级域。
const CC_TLDS: &[&str] = &[
    "cn", "uk", "jp", "au", "br", "hk", "tw", "kr", "sg", "nz", "za", "mx", "ru", "in", "id", "my",
    "th", "vn", "tr", "il", "ar", "pl", "pt",
];

/// 从域名取可注册域（eTLD+1 的简化版，覆盖常见多段后缀）。
pub fn registrable_domain(domain: &str) -> String {
    let d = domain.trim().trim_end_matches('.').to_ascii_lowercase();
    if d.is_empty() {
        return String::new();
    }
    let labels: Vec<&str> = d.split('.').collect();
    if labels.len() <= 2 {
        return d;
    }
    let last = labels[labels.len() - 1];
    let second = labels[labels.len() - 2];
    if CC_TLDS.contains(&last) && MULTI_PART_SUFFIX.contains(&second) {
        labels[labels.len() - 3..].join(".")
    } else {
        labels[labels.len() - 2..].join(".")
    }
}

/// 评估一封邮件的发件人可信度。
pub fn analyze_sender(from: &Address, reply_to: &[Address]) -> SenderTrust {
    let address = from.email.trim().to_string();
    // 没有发件人地址就没有「它声称是谁」可言：草稿、本地新建邮件、协议缺失
    // 都会走到这里。此时既不能判安全也不能判可疑——直接给 Safe 且不带任何
    // 提示，避免把「未知」误报成「IP 直连」这类更吓人的结论。
    if address.is_empty() {
        return SenderTrust {
            address,
            display_name: from.name.clone(),
            domain: String::new(),
            registrable_domain: String::new(),
            level: TrustLevel::Safe,
            warnings: Vec::new(),
        };
    }
    let domain = from
        .domain()
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let registrable = registrable_domain(&domain);
    let mut warnings = Vec::new();

    // 没有可注册域（空 / 裸主机名 / IP 字面量）都归到「发件人不是正常域名」这一类
    let bare_host = domain.is_empty() || (!domain.contains('.') && !domain.contains(':'));
    if bare_host || is_ip_literal(&domain) {
        warnings.push(TrustWarning::IpLiteralDomain {
            domain: domain.clone(),
        });
    }

    // 国际化域名：解码后给用户看真实含义
    let mut puny_decoded: Option<String> = None;
    if domain.split('.').any(|l| l.starts_with("xn--")) {
        puny_decoded = decode_punycode_domain(&domain);
        warnings.push(TrustWarning::PunycodeDomain {
            domain: domain.clone(),
            decoded: puny_decoded.clone(),
        });
    }

    // 同形字：混用拉丁与西里尔/希腊字母
    let probe = puny_decoded.clone().unwrap_or_else(|| domain.clone());
    if has_mixed_script(&probe) {
        warnings.push(TrustWarning::MixedScript {
            domain: probe.clone(),
        });
    }

    if let Some(tld) = registrable.rsplit_once('.').map(|(_, t)| t) {
        if SUSPICIOUS_TLDS.contains(&tld) {
            warnings.push(TrustWarning::SuspiciousTld {
                domain: registrable.clone(),
                tld: tld.to_string(),
            });
        }
    }

    if let Some(brand) = lookalike_of(&registrable) {
        warnings.push(TrustWarning::LookalikeDomain {
            domain: registrable.clone(),
            resembles: brand,
        });
    }

    if let Some(brand) = subdomain_brand(&domain, &registrable) {
        warnings.push(TrustWarning::SubdomainTrick {
            domain: domain.clone(),
            brand,
        });
    }

    // 显示名伪装：显示名里出现邮箱或完整域名，且和真实发件域不符
    if let Some(name) = &from.name {
        if let Some(claimed) = extract_identity(name) {
            let claimed_reg = registrable_domain(&claimed);
            if !claimed_reg.is_empty() && claimed_reg != registrable {
                warnings.push(TrustWarning::DisplayNameSpoof {
                    claimed,
                    actual: registrable.clone(),
                });
            }
        }
    }

    // 免费邮箱冒充品牌
    if FREE_MAIL_DOMAINS.contains(&registrable.as_str()) {
        if let Some(name) = &from.name {
            let name_lower = name.to_ascii_lowercase();
            if let Some(brand) = BRAND_DOMAINS.iter().find(|b| {
                let brand_label = b.split('.').next().unwrap_or_default();
                name_lower.contains(brand_label) && registrable_domain(b) != registrable
            }) {
                warnings.push(TrustWarning::FreeMailForBrand {
                    domain: registrable.clone(),
                    brand: (*brand).to_string(),
                });
            }
        }
    }

    // Reply-To 漂移
    for addr in reply_to {
        let rt = registrable_domain(&addr.email);
        if !rt.is_empty() && rt != registrable {
            warnings.push(TrustWarning::ReplyToMismatch {
                from_domain: registrable.clone(),
                reply_to_domain: rt,
            });
            break;
        }
    }

    let level = warnings
        .iter()
        .map(TrustWarning::level)
        .max()
        .unwrap_or(TrustLevel::Safe);

    SenderTrust {
        address: from.email.clone(),
        display_name: from.name.clone(),
        domain,
        registrable_domain: registrable,
        level,
        warnings,
    }
}

fn is_ip_literal(domain: &str) -> bool {
    if domain.contains(':') {
        return true; // IPv6（extract_host 已去方括号）
    }
    let parts: Vec<&str> = domain.split('.').collect();
    parts.len() == 4 && parts.iter().all(|p| p.parse::<u8>().is_ok())
}

/// 从显示名里抠出「声称的身份」：`support@paypal.com` 或 `paypal.com`。
fn extract_identity(name: &str) -> Option<String> {
    let lower = name.to_ascii_lowercase();
    // 优先取邮箱形态
    for token in
        lower.split(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '(' | ')' | ',' | ';'))
    {
        if let Some((_, dom)) = token.split_once('@') {
            if dom.contains('.') {
                return Some(strip_non_domain(dom));
            }
        }
    }
    // 其次取裸域名
    for token in lower.split_whitespace() {
        let t = strip_non_domain(token);
        if t.matches('.').count() == 1 && t.len() > 3 && t.split('.').all(|p| !p.is_empty()) {
            return Some(t);
        }
    }
    None
}

fn strip_non_domain(s: &str) -> String {
    s.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-')
        .to_string()
}

/// 品牌相似度判定：先做同形骨架化（0→o、1→l、rn→m 等），再做编辑距离。
fn lookalike_of(registrable: &str) -> Option<String> {
    let label = registrable.split('.').next().unwrap_or_default();
    if label.is_empty() {
        return None;
    }
    let skel = skeleton(label);
    for brand in BRAND_DOMAINS {
        let brand_label = brand.split('.').next().unwrap_or_default();
        if brand_label == label {
            // 完全一致：但可注册域不同（如 gmail.com vs mail.gmail.com 已在前一步拦住），
            // 这里只处理「标签相同、后缀不同」的情况
            if registrable_domain(brand) != registrable && !registrable.ends_with(brand) {
                return Some((*brand).to_string());
            }
            return None;
        }
        if skeleton(brand_label) == skel {
            return Some((*brand).to_string());
        }
        if brand_label.len() >= 6 && levenshtein(label, brand_label) <= 1 && label.len() >= 6 {
            return Some((*brand).to_string());
        }
    }
    None
}

/// 品牌名被塞进非可注册域位置（`apple.com.secure-login.cn`）。
fn subdomain_brand(domain: &str, registrable: &str) -> Option<String> {
    if domain == registrable {
        return None;
    }
    let prefix = domain.strip_suffix(registrable)?.trim_end_matches('.');
    for part in prefix.split('.') {
        for brand in BRAND_DOMAINS {
            let brand_label = brand.split('.').next().unwrap_or_default();
            if part == brand_label {
                return Some((*brand).to_string());
            }
        }
    }
    None
}

fn skeleton(label: &str) -> String {
    let lowered: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c {
            '0' => 'o',
            '1' => 'l',
            '3' => 'e',
            '4' => 'a',
            '5' => 's',
            '7' => 't',
            '8' => 'b',
            c => c,
        })
        .collect();
    lowered.replace("rn", "m").replace("vv", "w")
}

fn has_mixed_script(s: &str) -> bool {
    let mut latin = false;
    let mut other = false;
    for c in s.chars() {
        if c.is_ascii_alphabetic() {
            latin = true;
        } else if matches!(c as u32, 0x0400..=0x04FF | 0x0370..=0x03FF | 0x0530..=0x058F) {
            other = true;
        }
    }
    latin && other
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// RFC 3492 punycode 解码（仅用于把 `xn--` 域名还原成人可读形式）。
pub fn decode_punycode_domain(domain: &str) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    for label in domain.split('.') {
        if let Some(encoded) = label.strip_prefix("xn--") {
            out.push(decode_punycode(encoded)?);
        } else {
            out.push(label.to_string());
        }
    }
    Some(out.join("."))
}

/// punycode 的 digit 映射与通用 base36 **不同**：字母 a-z/A-Z 代表 0-25，
/// 数字 0-9 代表 26-35（RFC 3492 §5）。用 `char::to_digit(36)` 会静默解错。
fn puny_digit(c: char) -> Option<u32> {
    match c {
        'a'..='z' => Some(c as u32 - 'a' as u32),
        'A'..='Z' => Some(c as u32 - 'A' as u32),
        '0'..='9' => Some(c as u32 - '0' as u32 + 26),
        _ => None,
    }
}

fn decode_punycode(input: &str) -> Option<String> {
    const BASE: u32 = 36;
    const TMIN: u32 = 1;
    const TMAX: u32 = 26;
    const SKEW: u32 = 38;
    const DAMP: u32 = 700;
    const INITIAL_BIAS: u32 = 72;
    const INITIAL_N: u32 = 128;

    let (basic, encoded) = match input.rfind('-') {
        Some(idx) => (&input[..idx], &input[idx + 1..]),
        None => ("", input),
    };
    let mut output: Vec<char> = basic.chars().collect();
    let mut n = INITIAL_N;
    let mut i: u32 = 0;
    let mut bias = INITIAL_BIAS;
    let chars: Vec<char> = encoded.chars().collect();
    let mut pos = 0usize;

    while pos < chars.len() {
        let oldi = i;
        let mut w: u32 = 1;
        let mut k = BASE;
        loop {
            let c = *chars.get(pos)?;
            pos += 1;
            let digit = puny_digit(c)?;
            i = i.checked_add(digit.checked_mul(w)?)?;
            let t = if k <= bias {
                TMIN
            } else if k >= bias + TMAX {
                TMAX
            } else {
                k - bias
            };
            if digit < t {
                break;
            }
            w = w.checked_mul(BASE - t)?;
            k += BASE;
        }
        let len = output.len() as u32 + 1;
        let delta = i - oldi;
        bias = adapt(delta, len, oldi == 0, DAMP, SKEW, BASE, TMIN, TMAX);
        n = n.checked_add(i / len)?;
        i %= len;
        output.insert(i as usize, char::from_u32(n)?);
        i += 1;
    }
    Some(output.into_iter().collect())
}

#[allow(clippy::too_many_arguments)]
fn adapt(
    delta: u32,
    numpoints: u32,
    first: bool,
    damp: u32,
    skew: u32,
    base: u32,
    tmin: u32,
    tmax: u32,
) -> u32 {
    let mut delta = if first { delta / damp } else { delta / 2 };
    delta += delta / numpoints;
    let mut k = 0;
    while delta > ((base - tmin) * tmax) / 2 {
        delta /= base - tmin;
        k += base;
    }
    k + (base - tmin + 1) * delta / (delta + skew)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trust(email: &str, name: Option<&str>) -> SenderTrust {
        let from = match name {
            Some(n) => Address::with_name(n, email),
            None => Address::new(email),
        };
        analyze_sender(&from, &[])
    }

    #[test]
    fn normal_sender_is_safe() {
        let t = trust("alice@example.com", Some("Alice"));
        assert_eq!(t.level, TrustLevel::Safe);
        assert_eq!(t.registrable_domain, "example.com");
        assert!(!t.suspicious());
    }

    #[test]
    fn registrable_domain_handles_multi_part_suffix() {
        assert_eq!(registrable_domain("mail.example.com.cn"), "example.com.cn");
        assert_eq!(registrable_domain("a.b.example.co.uk"), "example.co.uk");
        assert_eq!(registrable_domain("example.com"), "example.com");
        assert_eq!(registrable_domain("localhost"), "localhost");
    }

    #[test]
    fn detects_lookalike_digits() {
        let t = trust("support@paypa1.com", None);
        assert_eq!(t.level, TrustLevel::Danger);
        assert!(t
            .warnings
            .iter()
            .any(|w| matches!(w, TrustWarning::LookalikeDomain { .. })));
    }

    #[test]
    fn detects_punycode_and_decodes_it() {
        // xn--80ak6aa92e.com 解码为 аррӏе.com（西里尔字母同形字）
        let t = trust("support@xn--80ak6aa92e.com", None);
        assert!(t.suspicious());
        let w = t
            .warnings
            .iter()
            .find(|w| matches!(w, TrustWarning::PunycodeDomain { .. }))
            .expect("应命中 punycode 警告");
        let TrustWarning::PunycodeDomain { decoded, .. } = w else {
            unreachable!()
        };
        let decoded = decoded.as_deref().expect("应能解码");
        assert!(
            decoded.contains('р'),
            "解码结果应含西里尔字符，实际 {decoded}"
        );
    }

    #[test]
    fn detects_mixed_script_homograph() {
        let t = trust("a@\u{0430}pple.com", None); // 首字母为西里尔 а
        assert_eq!(t.level, TrustLevel::Danger);
    }

    #[test]
    fn detects_ip_literal_sender() {
        let t = trust("admin@192.0.2.1", None);
        assert_eq!(t.level, TrustLevel::Danger);
        assert!(t
            .warnings
            .iter()
            .any(|w| matches!(w, TrustWarning::IpLiteralDomain { .. })));
    }

    #[test]
    fn detects_suspicious_tld() {
        let t = trust("notice@bank-secure.top", None);
        assert!(t.warnings.iter().any(|w| matches!(
            w,
            TrustWarning::SuspiciousTld { tld, .. } if tld == "top"
        )));
    }

    #[test]
    fn detects_subdomain_trick() {
        let t = trust("no-reply@apple.com.secure-login.cn", None);
        assert_eq!(t.level, TrustLevel::Danger);
        assert!(t.warnings.iter().any(|w| matches!(
            w,
            TrustWarning::SubdomainTrick { brand, .. } if brand == "apple.com"
        )));
    }

    #[test]
    fn detects_display_name_spoof() {
        let t = trust(
            "attacker@evil.example",
            Some("PayPal Service <service@paypal.com>"),
        );
        assert!(t
            .warnings
            .iter()
            .any(|w| matches!(w, TrustWarning::DisplayNameSpoof { .. })));
        assert!(t.suspicious());
    }

    #[test]
    fn display_name_with_same_domain_is_fine() {
        let t = trust("service@paypal.com", Some("service@paypal.com"));
        assert_eq!(t.level, TrustLevel::Safe, "{:?}", t.warnings);
    }

    #[test]
    fn detects_reply_to_drift() {
        let from = Address::new("billing@example.com");
        let reply = vec![Address::new("collect@evil.example")];
        let t = analyze_sender(&from, &reply);
        assert_eq!(t.level, TrustLevel::Caution);
        assert!(t.summary().contains("回复地址"));
    }

    #[test]
    fn free_mail_claiming_brand_is_caution() {
        let t = trust("apple-id@qq.com", Some("Apple Support"));
        assert!(t
            .warnings
            .iter()
            .any(|w| matches!(w, TrustWarning::FreeMailForBrand { .. })));
        assert_eq!(t.level, TrustLevel::Caution);
    }

    #[test]
    fn punycode_roundtrip_known_value() {
        assert_eq!(decode_punycode("bcher-kva").as_deref(), Some("bücher"));
        assert_eq!(
            decode_punycode_domain("xn--bcher-kva.example").as_deref(),
            Some("bücher.example")
        );
    }

    #[test]
    fn empty_sender_is_not_flagged() {
        // 草稿 / 本地新建邮件没有 From：不能报「IP 直连」这种误导性结论
        let t = trust("", None);
        assert_eq!(t.level, TrustLevel::Safe, "{:?}", t.warnings);
        assert!(t.warnings.is_empty());
        assert!(t.summary().is_empty());
    }

    #[test]
    fn summary_is_user_readable() {
        let t = trust("apple-id@qq.com", Some("Apple Support"));
        let s = t.summary();
        assert!(s.contains("免费邮箱"), "{s}");
        assert!(!s.contains("Some("));
    }
}
