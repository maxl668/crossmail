//! 凭证加密仓（S4-04 / SEC-005）：明文密钥永不落盘，落盘的是密文。
//!
//! 分层：
//! - `SecretCodec`：纯密码学接口（封存/解封），可换成平台密钥库实现；
//! - `SecretVault`：把 `alias -> 密文` 持久化到 SQLite，业务层只按别名取用；
//! - `Account` 模型里的 `Credential::Keyring { alias }` 只是别名引用，密文在 vault 里。

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use chrono::Utc;
use crossmail_core::{MailError, MailResult};
use crossmail_storage::Database;
use std::sync::{Arc, Mutex};

use crate::lock_db;

/// 密文版本前缀：升级算法时可并行存在多代密文。
pub const SEALED_PREFIX: &str = "enc:v1:";

/// 算法绑定串，作为 AEAD 的附加认证数据，防止跨用途重放。
const AAD: &[u8] = b"crossmail.credential.v1";

/// 是否是本模块产生的密文（防止把密文再封一次）。
pub fn is_sealed(value: &str) -> bool {
    value.starts_with(SEALED_PREFIX)
}

/// 凭证封存接口。平台若要用系统钥匙串，实现本 trait 即可，业务层无需改动。
pub trait SecretCodec: Send + Sync {
    fn seal(&self, plaintext: &str) -> MailResult<String>;
    fn open(&self, sealed: &str) -> MailResult<String>;
}

/// ChaCha20-Poly1305 实现：AEAD，密文带完整性校验，篡改会解密失败而不是解出垃圾。
pub struct ChaChaSecretCodec {
    cipher: ChaCha20Poly1305,
    /// 主密钥副本（仅进程内存在，用于导出给平台密钥库）
    key: [u8; 32],
}

impl ChaChaSecretCodec {
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(&key)),
            key,
        }
    }

    /// 生成随机主密钥（首启用；调用方负责把它放进平台安全存储）。
    pub fn generate() -> Self {
        let key = ChaCha20Poly1305::generate_key(&mut OsRng);
        Self::new(key.into())
    }

    /// 主密钥的 base64（URL-safe 无填充）表示，供平台密钥库保存。
    pub fn export_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.key)
    }

    /// 从导出的主密钥恢复。
    pub fn from_exported(exported: &str) -> MailResult<Self> {
        let bytes = URL_SAFE_NO_PAD
            .decode(exported)
            .map_err(|_| MailError::InvalidInput("主密钥编码损坏".into()))?;
        let key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| MailError::InvalidInput("主密钥长度必须为 32 字节".into()))?;
        Ok(Self::new(key))
    }
}

impl SecretCodec for ChaChaSecretCodec {
    fn seal(&self, plaintext: &str) -> MailResult<String> {
        if is_sealed(plaintext) {
            return Err(MailError::InvalidInput("凭证已是密文，拒绝二次封存".into()));
        }
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ct = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: AAD,
                },
            )
            .map_err(|_| MailError::Storage("凭证加密失败".into()))?;
        let mut blob = nonce.to_vec();
        blob.extend_from_slice(&ct);
        Ok(format!("{SEALED_PREFIX}{}", URL_SAFE_NO_PAD.encode(blob)))
    }

    fn open(&self, sealed: &str) -> MailResult<String> {
        let body = sealed
            .strip_prefix(SEALED_PREFIX)
            .ok_or_else(|| MailError::InvalidInput("凭证密文缺少版本前缀".into()))?;
        let blob = URL_SAFE_NO_PAD
            .decode(body)
            .map_err(|_| MailError::InvalidInput("凭证密文编码损坏".into()))?;
        if blob.len() <= 12 {
            return Err(MailError::InvalidInput("凭证密文长度异常".into()));
        }
        let (nonce_bytes, ct) = blob.split_at(12);
        let nonce = Nonce::from_slice(nonce_bytes);
        let pt = self
            .cipher
            .decrypt(nonce, Payload { msg: ct, aad: AAD })
            .map_err(|_| MailError::Storage("凭证解密失败（密钥不匹配或密文被篡改）".into()))?;
        String::from_utf8(pt).map_err(|_| MailError::Storage("凭证明文不是合法 UTF-8".into()))
    }
}

/// 凭证密文仓：SQLite 持久化 + 内存加解密。
pub struct SecretVault {
    db: Arc<Mutex<Database>>,
    codec: Arc<dyn SecretCodec>,
}

impl SecretVault {
    pub fn new(db: Arc<Mutex<Database>>, codec: Arc<dyn SecretCodec>) -> Self {
        Self { db, codec }
    }

    /// 存入明文凭证：立即封存，明文只在内存里活这一下。
    pub fn put(&self, alias: &str, secret: &str) -> MailResult<()> {
        if alias.trim().is_empty() {
            return Err(MailError::InvalidInput("凭证别名不能为空".into()));
        }
        if secret.is_empty() {
            return Err(MailError::InvalidInput("凭证内容不能为空".into()));
        }
        let sealed = self.codec.seal(secret)?;
        lock_db(&self.db)?.put_secret(alias, &sealed, Utc::now())
    }

    pub fn get(&self, alias: &str) -> MailResult<Option<String>> {
        let sealed = lock_db(&self.db)?.get_secret(alias)?;
        match sealed {
            None => Ok(None),
            Some(s) => self.codec.open(&s).map(Some),
        }
    }

    pub fn delete(&self, alias: &str) -> MailResult<usize> {
        lock_db(&self.db)?.delete_secret(alias)
    }

    pub fn delete_prefix(&self, prefix: &str) -> MailResult<usize> {
        lock_db(&self.db)?.delete_secrets_with_prefix(prefix)
    }

    pub fn aliases(&self) -> MailResult<Vec<String>> {
        lock_db(&self.db)?.list_secret_aliases()
    }

    /// 直接读落盘的原始串（安全测试用：断言库里没有明文）。
    pub fn raw_stored(&self, alias: &str) -> MailResult<Option<String>> {
        lock_db(&self.db)?.get_secret(alias)
    }
}

/// 内存实现，供测试与“系统钥匙串可用”的平台注入。
#[derive(Default)]
pub struct MemorySecretCodec {
    inner: Mutex<std::collections::HashMap<String, String>>,
}

impl MemorySecretCodec {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretCodec for MemorySecretCodec {
    fn seal(&self, plaintext: &str) -> MailResult<String> {
        let tag = format!("mem:{}", Utc::now().timestamp_nanos_opt().unwrap_or(0));
        if let Ok(mut m) = self.inner.lock() {
            m.insert(tag.clone(), plaintext.to_string());
        }
        Ok(tag)
    }

    fn open(&self, sealed: &str) -> MailResult<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|m| m.get(sealed).cloned())
            .ok_or_else(|| MailError::Storage("凭证不存在".into()))
    }
}
