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
        let n = input.read(&mut page_buf)?;
        if n == 0 {
            break;
        }
        // 不足一页则补零
        if n < PAGE_SZ {
            page_buf[n..].fill(0);
        }
        let dec = decrypt_page(enc_key, &page_buf, pgno as u32)?;
        output.write_all(&dec)?;
    }

    Ok(())
}
