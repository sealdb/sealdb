//! MySQL encryption builtins (`AES_ENCRYPT` / `AES_DECRYPT`).
//!
//! TiDB follows MySQL: AES with zero-padded keys, PKCS#7 padding on encrypt,
//! and `block_encryption_mode` selecting the key size and chaining mode.

use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::{Aes128, Aes192, Aes256};

/// Parsed `block_encryption_mode` (`aes-128-ecb` by default).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockEncryptionMode {
    pub key_len: usize,
    pub cipher: CipherMode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CipherMode {
    Ecb,
    Cbc,
    Ofb,
}

impl Default for BlockEncryptionMode {
    fn default() -> Self {
        Self {
            key_len: 16,
            cipher: CipherMode::Ecb,
        }
    }
}

impl BlockEncryptionMode {
    /// TiDB `ParseBlockEncryptionMode`; an unknown name falls back to the
    /// default (`aes-128-ecb`).
    pub fn parse(name: &str) -> Self {
        let name = name.trim().to_ascii_lowercase();
        let mut parts = name.split('-');
        let _aes = parts.next();
        let key_len = match parts.next() {
            Some("192") => 24,
            Some("256") => 32,
            _ => 16,
        };
        let cipher = match parts.next() {
            Some("cbc") => CipherMode::Cbc,
            Some("ofb") => CipherMode::Ofb,
            _ => CipherMode::Ecb,
        };
        Self { key_len, cipher }
    }
}

/// Zero-pad (or truncate) the key to the mode's key length.
fn fixed_key(key: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let n = key.len().min(len);
    out[..n].copy_from_slice(&key[..n]);
    out
}

fn xor_block(block: &mut [u8], other: &[u8]) {
    for (a, b) in block.iter_mut().zip(other.iter()) {
        *a ^= b;
    }
}

/// PKCS#7 pad to the AES block size.
fn pad(data: &[u8]) -> Vec<u8> {
    let pad_len = 16 - (data.len() % 16);
    let mut out = Vec::with_capacity(data.len() + pad_len);
    out.extend_from_slice(data);
    out.extend(std::iter::repeat_n(pad_len as u8, pad_len));
    out
}

fn encrypt_block(mode: BlockEncryptionMode, key: &[u8], block: &mut [u8]) {
    let key = fixed_key(key, mode.key_len);
    match mode.key_len {
        24 => Aes192::new_from_slice(&key)
            .unwrap()
            .encrypt_block(block.into()),
        32 => Aes256::new_from_slice(&key)
            .unwrap()
            .encrypt_block(block.into()),
        _ => Aes128::new_from_slice(&key)
            .unwrap()
            .encrypt_block(block.into()),
    }
}

fn decrypt_block(mode: BlockEncryptionMode, key: &[u8], block: &mut [u8]) {
    let key = fixed_key(key, mode.key_len);
    match mode.key_len {
        24 => Aes192::new_from_slice(&key)
            .unwrap()
            .decrypt_block(block.into()),
        32 => Aes256::new_from_slice(&key)
            .unwrap()
            .decrypt_block(block.into()),
        _ => Aes128::new_from_slice(&key)
            .unwrap()
            .decrypt_block(block.into()),
    }
}

/// `AES_ENCRYPT`: PKCS#7-padded encryption. `iv` is used by CBC/OFB and
/// ignored (zero) in ECB.
pub fn aes_encrypt(data: &[u8], key: &[u8], iv: &[u8], mode: BlockEncryptionMode) -> Vec<u8> {
    let mut padded = pad(data);
    match mode.cipher {
        CipherMode::Ecb => {
            for block in padded.chunks_mut(16) {
                encrypt_block(mode, key, block);
            }
        }
        CipherMode::Cbc => {
            let mut prev = [0u8; 16];
            let n = iv.len().min(16);
            prev[..n].copy_from_slice(&iv[..n]);
            for block in padded.chunks_mut(16) {
                xor_block(block, &prev);
                encrypt_block(mode, key, block);
                prev.copy_from_slice(block);
            }
        }
        CipherMode::Ofb => {
            // OFB keystream: repeated encryption of the IV; the padded
            // plaintext is XORed with it block by block.
            let mut feed = [0u8; 16];
            let n = iv.len().min(16);
            feed[..n].copy_from_slice(&iv[..n]);
            for block in padded.chunks_mut(16) {
                encrypt_block(mode, key, &mut feed);
                xor_block(block, &feed);
            }
        }
    }
    padded
}

/// `AES_DECRYPT`: `None` when the input is not a whole number of AES blocks
/// (MySQL returns NULL).
pub fn aes_decrypt(
    data: &[u8],
    key: &[u8],
    iv: &[u8],
    mode: BlockEncryptionMode,
) -> Option<Vec<u8>> {
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let mut out = data.to_vec();
    match mode.cipher {
        CipherMode::Ecb => {
            for block in out.chunks_mut(16) {
                decrypt_block(mode, key, block);
            }
        }
        CipherMode::Cbc => {
            let mut prev = [0u8; 16];
            let n = iv.len().min(16);
            prev[..n].copy_from_slice(&iv[..n]);
            for block in out.chunks_mut(16) {
                let cipher = block.to_vec();
                decrypt_block(mode, key, block);
                xor_block(block, &prev);
                prev.copy_from_slice(&cipher);
            }
        }
        CipherMode::Ofb => {
            let mut feed = [0u8; 16];
            let n = iv.len().min(16);
            feed[..n].copy_from_slice(&iv[..n]);
            for block in out.chunks_mut(16) {
                encrypt_block(mode, key, &mut feed);
                xor_block(block, &feed);
            }
        }
    }
    // Strip PKCS#7 padding; a bad pad leaves the value as-is (MySQL returns
    // the decrypted bytes).
    let pad_len = *out.last()? as usize;
    if pad_len >= 1 && pad_len <= 16 && pad_len <= out.len() {
        let tail_ok = out[out.len() - pad_len..]
            .iter()
            .all(|&b| b as usize == pad_len);
        if tail_ok {
            out.truncate(out.len() - pad_len);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modes() {
        assert_eq!(
            BlockEncryptionMode::parse("aes-128-ecb"),
            BlockEncryptionMode {
                key_len: 16,
                cipher: CipherMode::Ecb
            }
        );
        assert_eq!(BlockEncryptionMode::parse("AES-256-CBC").key_len, 32);
        assert_eq!(
            BlockEncryptionMode::parse("aes-192-ofb").cipher,
            CipherMode::Ofb
        );
    }

    #[test]
    fn encrypt_matches_mysql() {
        // MySQL: `AES_ENCRYPT('a','a')` under the default aes-128-ecb.
        let out = aes_encrypt(b"a", b"a", b"", BlockEncryptionMode::default());
        assert_eq!(
            out,
            vec![
                0x7b, 0x20, 0x57, 0x5d, 0xa1, 0x06, 0x75, 0x9d, 0xbd, 0xb1, 0xa3, 0x2e, 0xe2, 0xd9,
                0xa7, 0x74
            ]
        );
    }
}
