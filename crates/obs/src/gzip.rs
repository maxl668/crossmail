//! 最小 gzip 写出器（RFC 1951 DEFLATE fixed-Huffman + RFC 1952 封装）。
//!
//! 为什么自己写：OBS-002 要求「旧日志自动压缩」，而本仓库的构建环境无法
//! 引入新的 crate（沙箱禁 utime，cargo 解压新包必失败）。flate2 用不了，
//! 但「写一个 deflate 编码器」这件事本身是封闭的、可被外部命令行
//! （`gunzip -t` / `gzip -dc`）独立校验的——测试里就是这么验的，不做自证。
//!
//! 编码策略：贪心 LZ77（单槽哈希表，3 字节窗口）+ fixed Huffman 码表。
//! 压缩率不如 zlib 的 optimal parse，但对日志文本通常能到 3~6 倍，成本可控。

const MAX_MATCH: usize = 258;
const MIN_MATCH: usize = 3;
const MAX_DIST: usize = 32 * 1024;
const HASH_BITS: u32 = 15;
const HASH_SIZE: usize = 1 << HASH_BITS;

/// gzip 回车换行无关的固定头：magic(1f 8b) + deflate + 无文件名 + mtime=0 + OS=255。
const GZIP_HEADER: [u8; 10] = [0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0x00, 0xff];

/// 压缩为 gzip 字节流。
pub fn gzip_compress(data: &[u8]) -> Vec<u8> {
    let body = deflate_fixed(data);
    let mut out = Vec::with_capacity(body.len() + 20);
    out.extend_from_slice(&GZIP_HEADER);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

// ------------------------------------------------------------------ bit writer

struct BitWriter {
    out: Vec<u8>,
    buf: u64,
    bits: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            buf: 0,
            bits: 0,
        }
    }

    /// 写 `n` 位，低位先出（deflate 的通用规则）。
    fn write_bits(&mut self, value: u32, n: u32) {
        debug_assert!(n <= 32);
        self.buf |= u64::from(value & mask(n)) << self.bits;
        self.bits += n;
        while self.bits >= 8 {
            self.out.push((self.buf & 0xff) as u8);
            self.buf >>= 8;
            self.bits -= 8;
        }
    }

    /// 写 Huffman 码：码字本身是 MSB-first 塞进比特流的。
    fn write_code(&mut self, code: u16, len: u32) {
        for i in (0..len).rev() {
            self.write_bits(u32::from((code >> i) & 1), 1);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        if self.bits > 0 {
            self.out.push((self.buf & 0xff) as u8);
        }
        self.out
    }
}

fn mask(n: u32) -> u32 {
    if n >= 32 {
        u32::MAX
    } else {
        (1u32 << n) - 1
    }
}

// ------------------------------------------------------- fixed huffman 码表

/// 字面量/长度符号的 fixed Huffman 码（RFC 1951 §3.2.6）。
fn emit_litlen(w: &mut BitWriter, sym: u16) {
    match sym {
        0..=143 => w.write_code(0x30 + sym, 8),
        144..=255 => w.write_code(0x190 + (sym - 144), 9),
        256..=279 => w.write_code(sym - 256, 7),
        _ => w.write_code(0xC0 + (sym - 280), 8),
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u32; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u32; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// 长度 → (符号, 附加位宽, 附加值)
fn length_symbol(len: usize) -> (u16, u32, u32) {
    let mut i = LEN_BASE.len() - 1;
    while i > 0 && LEN_BASE[i] > len as u16 {
        i -= 1;
    }
    (
        257 + i as u16,
        LEN_EXTRA[i],
        (len as u32).wrapping_sub(u32::from(LEN_BASE[i])),
    )
}

/// 距离 → (符号, 附加位宽, 附加值)
fn distance_symbol(dist: usize) -> (u16, u32, u32) {
    let mut i = DIST_BASE.len() - 1;
    while i > 0 && usize::from(DIST_BASE[i]) > dist {
        i -= 1;
    }
    (
        i as u16,
        DIST_EXTRA[i],
        (dist as u32).wrapping_sub(u32::from(DIST_BASE[i])),
    )
}

fn hash3(data: &[u8], i: usize) -> usize {
    let v = (u32::from(data[i]) << 16) | (u32::from(data[i + 1]) << 8) | u32::from(data[i + 2]);
    // 乘法散列把 24 bit 摊到 15 bit
    ((v.wrapping_mul(0x9E37_79B1)) >> (32 - HASH_BITS)) as usize % HASH_SIZE
}

/// 单 block 的 fixed-Huffman DEFLATE 编码。
pub fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    // BFINAL=1, BTYPE=01（fixed Huffman）
    w.write_bits(1, 1);
    w.write_bits(1, 2);

    let n = data.len();
    // -1 表示空槽
    let mut head = vec![usize::MAX; HASH_SIZE];
    let mut i = 0usize;

    while i < n {
        let mut best_len = 0usize;
        let mut best_dist = 0usize;
        if i + MIN_MATCH <= n {
            let h = hash3(data, i);
            let cand = head[h];
            head[h] = i;
            if cand != usize::MAX && i - cand <= MAX_DIST {
                let max_len = (n - i).min(MAX_MATCH);
                let mut l = 0usize;
                while l < max_len && data[cand + l] == data[i + l] {
                    l += 1;
                }
                if l >= MIN_MATCH {
                    best_len = l;
                    best_dist = i - cand;
                }
            }
        }
        if best_len >= MIN_MATCH {
            let (lsym, lextra, lval) = length_symbol(best_len);
            emit_litlen(&mut w, lsym);
            if lextra > 0 {
                w.write_bits(lval, lextra);
            }
            let (dsym, dextra, dval) = distance_symbol(best_dist);
            w.write_code(dsym, 5);
            if dextra > 0 {
                w.write_bits(dval, dextra);
            }
            // 把跳过区间内的位置也塞进哈希表，提升后续匹配命中率
            for k in 1..best_len {
                let p = i + k;
                if p + MIN_MATCH <= n {
                    let h = hash3(data, p);
                    head[h] = p;
                }
            }
            i += best_len;
        } else {
            emit_litlen(&mut w, u16::from(data[i]));
            i += 1;
        }
    }

    // 块结束符
    emit_litlen(&mut w, 256);
    w.finish()
}

// ---------------------------------------------------------------------- CRC32

const CRC_TABLE: [u32; 256] = build_crc_table();

const fn build_crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

/// 标准 CRC-32（gzip / zip 使用的那一条）。
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c = CRC_TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::Command;

    #[test]
    fn crc32_matches_known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn gzip_header_and_trailer() {
        let data = b"hello crossmail";
        let gz = gzip_compress(data);
        assert_eq!(&gz[..3], &[0x1f, 0x8b, 0x08]);
        assert_eq!(&gz[gz.len() - 8..gz.len() - 4], &crc32(data).to_le_bytes());
        assert_eq!(
            &gz[gz.len() - 4..],
            &(data.len() as u32).to_le_bytes(),
            "ISIZE 必须等于原始长度"
        );
    }

    #[test]
    fn compresses_repetitive_log_text() {
        let mut src = String::new();
        for i in 0..500 {
            src.push_str(&format!(
                "2026-11-13T22:10:{:02}Z INFO  sync: 增量同步完成 mailbox=INBOX fetched={} skipped=0\n",
                i % 60,
                i
            ));
        }
        let gz = gzip_compress(src.as_bytes());
        assert!(
            gz.len() < src.len() / 3,
            "日志文本压缩率应优于 3 倍，实际 {} -> {}",
            src.len(),
            gz.len()
        );
    }

    #[test]
    fn roundtrip_through_system_gunzip() {
        // 用系统 gzip 独立校验，避免「自己写编码器 + 自己写解码器」的自证循环
        if Command::new("gunzip").arg("--version").output().is_err() {
            eprintln!("gunzip 不可用，跳过外部一致性校验");
            return;
        }
        let payloads: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"a".to_vec(),
            b"abcabcabcabcabcabc".to_vec(),
            r#"{"k":"v","n":123,"arr":[1,2,3],"中文":"邮件正文"}"#
                .repeat(200)
                .into_bytes(),
            (0u8..=255).cycle().take(9000).collect(),
        ];
        let dir = crate::testing::temp_dir("gzip-roundtrip");
        for (idx, p) in payloads.iter().enumerate() {
            let gz_path = dir.join(format!("p{idx}.bin.gz"));
            let mut f = std::fs::File::create(&gz_path).expect("写 gz 失败");
            f.write_all(&gzip_compress(p)).expect("写 gz 失败");
            drop(f);
            let out = Command::new("gunzip")
                .arg("-dc")
                .arg(&gz_path)
                .output()
                .expect("执行 gunzip 失败");
            assert!(out.status.success(), "gunzip 报错: {:?}", out.stderr);
            assert_eq!(out.stdout, *p, "payload #{idx} 解压后不一致");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn length_and_distance_symbol_boundaries() {
        assert_eq!(length_symbol(3).0, 257);
        assert_eq!(length_symbol(258).0, 285);
        assert_eq!(length_symbol(11).0, 265);
        assert_eq!(distance_symbol(1).0, 0);
        assert_eq!(distance_symbol(24577).0, 29);
        assert_eq!(distance_symbol(5).0, 4);
    }
}
