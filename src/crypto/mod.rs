pub mod wal;

use aes::Aes256;
use anyhow::{bail, Result};
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use cbc::Decryptor;
use std::io::{Read, Write};
use std::path::Path;

type Block = aes::cipher::Block<Aes256>;

pub const PAGE_SZ: usize = 4096;
pub const SALT_SZ: usize = 16;
pub const RESERVE_SZ: usize = 80; // IV(16) + HMAC(64)

/// SQLite 文件头魔数（16字节）
pub const SQLITE_HDR: &[u8] = b"SQLite format 3\x00";
const FIRST_PAGE_DECRYPT_ERR: &str =
    "解密失败：首页魔数不匹配，可能是密钥错误或 SQLCipher 页格式已变（非文件损坏）";

type Aes256CbcDec = Decryptor<Aes256>;

/// 解密单个 SQLCipher 4 页
///
/// - `enc_key`: 32字节 AES 密钥
/// - `page_data`: 原始加密页面数据（PAGE_SZ 字节）
/// - `pgno`: 页码（从1开始）
///
/// 返回解密后的完整页面（PAGE_SZ 字节）
pub fn decrypt_page(enc_key: &[u8; 32], page_data: &[u8], pgno: u32) -> Result<Vec<u8>> {
    if page_data.len() < PAGE_SZ {
        bail!("页面数据不足 {} 字节", PAGE_SZ);
    }

    // IV 位于页面末尾 RESERVE_SZ 区域的前16字节
    let iv_offset = PAGE_SZ - RESERVE_SZ;
    let iv: &[u8; 16] = page_data[iv_offset..iv_offset + 16]
        .try_into()
        .expect("IV 长度固定为 16");

    let mut result = vec![0u8; PAGE_SZ];

    if pgno == 1 {
        // 第一页：跳过 salt(16字节)，解密 [SALT_SZ..PAGE_SZ-RESERVE_SZ]
        let enc = &page_data[SALT_SZ..PAGE_SZ - RESERVE_SZ];
        let dec = aes_cbc_decrypt(enc_key, iv, enc)?;
        // 写入 SQLite 文件头
        result[..16].copy_from_slice(SQLITE_HDR);
        // 写入解密数据（从第16字节开始）
        result[16..PAGE_SZ - RESERVE_SZ].copy_from_slice(&dec);
        // 末尾 RESERVE_SZ 字节补零
        // （已经是零，无需显式操作）
    } else {
        // 其他页：解密 [0..PAGE_SZ-RESERVE_SZ]
        let enc = &page_data[..PAGE_SZ - RESERVE_SZ];
        let dec = aes_cbc_decrypt(enc_key, iv, enc)?;
        result[..PAGE_SZ - RESERVE_SZ].copy_from_slice(&dec);
        // 末尾 RESERVE_SZ 字节补零
    }

    Ok(result)
}

/// AES-256-CBC 解密（不去除 padding，SQLCipher 不使用 PKCS#7 padding）
fn aes_cbc_decrypt(key: &[u8; 32], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        bail!("密文长度不是 AES 块大小的倍数: {}", data.len());
    }
    // 将 &[u8] 复制为 Block 数组，避免 unsafe from_raw_parts_mut
    let mut blocks: Vec<Block> = data.chunks_exact(16).map(Block::clone_from_slice).collect();
    Aes256CbcDec::new(key.into(), iv.into()).decrypt_blocks_mut(&mut blocks);
    Ok(blocks.iter().flat_map(|b| b.iter().copied()).collect())
}

/// 完整解密一个 SQLCipher 数据库文件（流式，逐页读写避免全量载入内存）
///
/// 读取 `db_path`，按 PAGE_SZ 分页解密，写入 `out_path`
pub fn full_decrypt(db_path: &Path, out_path: &Path, enc_key: &[u8; 32]) -> Result<()> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut input = std::fs::File::open(db_path)?;
    let file_size = input.metadata()?.len() as usize;
    if file_size == 0 {
        bail!("数据库文件为空: {}", db_path.display());
    }

    let mut output = std::fs::File::create(out_path)?;
    let total_pages = (file_size + PAGE_SZ - 1) / PAGE_SZ;
    let mut page_buf = vec![0u8; PAGE_SZ];

    for pgno in 1..=total_pages {
        let page_start = (pgno - 1) * PAGE_SZ;
        let bytes_remaining = file_size.saturating_sub(page_start);
        read_page(&mut input, &mut page_buf, bytes_remaining)?;
        let dec = decrypt_page(enc_key, &page_buf, pgno as u32)?;
        if pgno == 1 {
            validate_first_decrypted_page(&dec)?;
        }
        output.write_all(&dec)?;
    }

    Ok(())
}

fn validate_first_decrypted_page(page: &[u8]) -> Result<()> {
    if page.len() < 24 || &page[..SQLITE_HDR.len()] != SQLITE_HDR {
        bail!(FIRST_PAGE_DECRYPT_ERR);
    }

    // decrypt_page 会为第一页重建 SQLite 魔数；相邻页头字段来自 AES 明文，
    // 一并校验才能把错误 key 的随机明文挡在写盘前。
    let page_size = u16::from_be_bytes([page[16], page[17]]) as usize;
    let write_version = page[18];
    let read_version = page[19];
    let reserved_space = page[20] as usize;
    let payload_fractions = &page[21..24];

    let valid = page_size == PAGE_SZ
        && matches!(write_version, 1 | 2)
        && matches!(read_version, 1 | 2)
        && matches!(reserved_space, 0 | RESERVE_SZ)
        && payload_fractions == [64, 32, 32];
    if !valid {
        bail!(FIRST_PAGE_DECRYPT_ERR);
    }

    Ok(())
}

fn read_page(
    input: &mut impl Read,
    page_buf: &mut [u8],
    bytes_remaining: usize,
) -> std::io::Result<usize> {
    let expected = bytes_remaining.min(PAGE_SZ);
    input.read_exact(&mut page_buf[..expected])?;
    if expected < PAGE_SZ {
        page_buf[expected..].fill(0);
    }
    Ok(expected)
}

#[cfg(test)]
mod tests {
    use super::{full_decrypt, read_page, PAGE_SZ, RESERVE_SZ, SALT_SZ, SQLITE_HDR};
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};
    use std::io::{self, Read};
    use std::path::PathBuf;

    struct ChunkedReader {
        chunks: Vec<Vec<u8>>,
        chunk_idx: usize,
        offset: usize,
    }

    impl ChunkedReader {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks,
                chunk_idx: 0,
                offset: 0,
            }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.chunk_idx >= self.chunks.len() {
                return Ok(0);
            }
            let chunk = &self.chunks[self.chunk_idx];
            let remaining = &chunk[self.offset..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            self.offset += n;
            if self.offset == chunk.len() {
                self.chunk_idx += 1;
                self.offset = 0;
            }
            Ok(n)
        }
    }

    fn unique_tmpdir(tag: &str) -> PathBuf {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("wx-cli-crypto-test-{}-{}-{}", tag, pid, nanos));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn aes_cbc_encrypt(key: &[u8; 32], iv: &[u8; 16], data: &[u8]) -> Vec<u8> {
        assert!(!data.is_empty());
        assert_eq!(data.len() % 16, 0);

        let mut blocks: Vec<super::Block> = data
            .chunks_exact(16)
            .map(super::Block::clone_from_slice)
            .collect();
        cbc::Encryptor::<aes::Aes256>::new(key.into(), iv.into()).encrypt_blocks_mut(&mut blocks);
        blocks.iter().flat_map(|b| b.iter().copied()).collect()
    }

    fn encrypted_first_page(key: &[u8; 32]) -> Vec<u8> {
        let iv = [0x42; 16];
        let mut plain = vec![0u8; PAGE_SZ];
        plain[..SQLITE_HDR.len()].copy_from_slice(SQLITE_HDR);
        plain[16..18].copy_from_slice(&(PAGE_SZ as u16).to_be_bytes());
        plain[18] = 1;
        plain[19] = 1;
        plain[20] = 0;
        plain[21] = 64;
        plain[22] = 32;
        plain[23] = 32;
        plain[28..32].copy_from_slice(&1u32.to_be_bytes());
        plain[44..48].copy_from_slice(&4u32.to_be_bytes());
        plain[56..60].copy_from_slice(&1u32.to_be_bytes());

        let encrypted = aes_cbc_encrypt(key, &iv, &plain[SALT_SZ..PAGE_SZ - RESERVE_SZ]);

        let mut page = vec![0u8; PAGE_SZ];
        page[..SALT_SZ].fill(0x55);
        page[SALT_SZ..PAGE_SZ - RESERVE_SZ].copy_from_slice(&encrypted);
        page[PAGE_SZ - RESERVE_SZ..PAGE_SZ - RESERVE_SZ + 16].copy_from_slice(&iv);
        page
    }

    #[test]
    fn full_decrypt_accepts_page_with_correct_key() {
        let key = [0x11; 32];
        let root = unique_tmpdir("full-ok");
        let db_path = root.join("encrypted.db");
        let out_path = root.join("plain.db");
        std::fs::write(&db_path, encrypted_first_page(&key)).unwrap();

        full_decrypt(&db_path, &out_path, &key).unwrap();

        let decrypted = std::fs::read(&out_path).unwrap();
        assert_eq!(&decrypted[..SQLITE_HDR.len()], SQLITE_HDR);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn full_decrypt_rejects_wrong_key_on_first_page() {
        let key = [0x11; 32];
        let wrong_key = [0x22; 32];
        let root = unique_tmpdir("full-wrong-key");
        let db_path = root.join("encrypted.db");
        let out_path = root.join("plain.db");
        std::fs::write(&db_path, encrypted_first_page(&key)).unwrap();

        let err = full_decrypt(&db_path, &out_path, &wrong_key).unwrap_err();
        assert!(
            err.to_string().contains("解密失败：首页魔数不匹配"),
            "unexpected error: {err:#}"
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn read_page_reads_across_short_chunks() {
        let mut reader = ChunkedReader::new(vec![vec![1; 32], vec![2; PAGE_SZ - 32]]);
        let mut page_buf = vec![0u8; PAGE_SZ];

        let n = read_page(&mut reader, &mut page_buf, PAGE_SZ).unwrap();

        assert_eq!(n, PAGE_SZ);
        assert_eq!(page_buf[0], 1);
        assert_eq!(page_buf[31], 1);
        assert_eq!(page_buf[32], 2);
        assert_eq!(page_buf[PAGE_SZ - 1], 2);
    }

    #[test]
    fn read_page_zero_pads_last_partial_page() {
        let mut reader = ChunkedReader::new(vec![vec![7; 8], vec![9; 4]]);
        let mut page_buf = vec![0u8; PAGE_SZ];

        let n = read_page(&mut reader, &mut page_buf, 12).unwrap();

        assert_eq!(n, 12);
        assert_eq!(&page_buf[..8], &[7; 8]);
        assert_eq!(&page_buf[8..12], &[9; 4]);
        assert!(page_buf[12..].iter().all(|&b| b == 0));
    }

    #[test]
    fn read_page_errors_on_early_eof() {
        let mut reader = ChunkedReader::new(vec![vec![1; 8]]);
        let mut page_buf = vec![0u8; PAGE_SZ];

        let err = read_page(&mut reader, &mut page_buf, 16).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }
}
