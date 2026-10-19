//! Media bytes travel only through authenticated provider endpoints, never through a supplied HTTP URL.
use anyhow::{Context, Result, ensure};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD},
};
use ctr::cipher::{KeyIvInit, StreamCipher};
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const MAX_BYTES: usize = 10 * 1024 * 1024;
// Base64 RPCs hold both encoded and decoded buffers. Serialize transfers across
// tenants to bound the working set on the operator's small bridge VM.
pub(crate) static TRANSFER: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Debug)]
pub struct Rejected(pub &'static str);
impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Rejected {}

pub struct Media {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

pub fn filename(value: &str) -> Result<String> {
    let name = value.rsplit(['/', '\\']).next().unwrap_or("");
    ensure!(
        !name.is_empty() && name.len() <= 255 && !name.chars().any(char::is_control),
        Rejected("Invalid attachment filename.")
    );
    ensure!(
        name != "." && name != "..",
        Rejected("Invalid attachment filename.")
    );
    Ok(name.to_string())
}

pub fn mime(value: &str) -> Result<String> {
    let value = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    ensure!(
        value.len() <= 128
            && value.split_once('/').is_some()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/.-+".contains(&byte)),
        Rejected("Invalid attachment MIME type.")
    );
    Ok(value)
}

pub fn msgtype(mime: &str) -> &'static str {
    if mime.starts_with("image/") {
        "m.image"
    } else if mime.starts_with("audio/") {
        "m.audio"
    } else if mime.starts_with("video/") {
        "m.video"
    } else {
        "m.file"
    }
}

/// @implements url:https://spec.matrix.org/v1.16/client-server-api/#sending-encrypted-attachments -- v2 SHA-256 authentication before AES-256-CTR decryption
pub fn decrypt(bytes: &mut [u8], file: &Value) -> Result<()> {
    ensure!(
        file["v"] == "v2" && file["key"]["kty"] == "oct" && file["key"]["alg"] == "A256CTR",
        "unsupported Matrix attachment encryption"
    );
    let key = URL_SAFE_NO_PAD.decode(
        file["key"]["k"]
            .as_str()
            .context("attachment key missing")?
            .trim_end_matches('='),
    )?;
    let iv = STANDARD_NO_PAD.decode(
        file["iv"]
            .as_str()
            .context("attachment IV missing")?
            .trim_end_matches('='),
    )?;
    let expected = STANDARD_NO_PAD.decode(
        file["hashes"]["sha256"]
            .as_str()
            .context("attachment hash missing")?
            .trim_end_matches('='),
    )?;
    ensure!(
        key.len() == 32 && iv.len() == 16 && expected.len() == 32,
        "invalid encrypted attachment metadata"
    );
    ensure!(
        bool::from(Sha256::digest(&*bytes).as_slice().ct_eq(&expected)),
        "attachment ciphertext hash mismatch"
    );
    ctr::Ctr64BE::<aes::Aes256>::new_from_slices(&key, &iv)
        .map_err(|_| anyhow::anyhow!("invalid attachment cipher parameters"))?
        .try_apply_keystream(bytes)
        .map_err(|_| anyhow::anyhow!("attachment counter overflow"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn encrypted_attachment_authenticates_before_decrypting() {
        let key = [7; 32];
        let iv = [0; 16];
        let mut bytes = b"private attachment".to_vec();
        ctr::Ctr64BE::<aes::Aes256>::new(&key.into(), &iv.into()).apply_keystream(&mut bytes);
        let file = json!({"v":"v2","key":{"kty":"oct","alg":"A256CTR","k":URL_SAFE_NO_PAD.encode(key)},"iv":STANDARD_NO_PAD.encode(iv),"hashes":{"sha256":STANDARD_NO_PAD.encode(Sha256::digest(&bytes))}});
        let mut corrupt = bytes.clone();
        corrupt[0] ^= 1;
        assert!(decrypt(&mut corrupt, &file).is_err());
        decrypt(&mut bytes, &file).unwrap();
        assert_eq!(bytes, b"private attachment");
    }
    #[test]
    fn decrypts_an_openssl_generated_attachment_fixture() {
        // Independent fixture: openssl enc -aes-256-ctr, all-zero 256-bit key and IV.
        let mut bytes = hex::decode("bde1b419c128e4ecc33c8272fbfc54f2216a").unwrap();
        let file = json!({"v":"v2","key":{"kty":"oct","alg":"A256CTR","k":URL_SAFE_NO_PAD.encode([0;32])},"iv":STANDARD_NO_PAD.encode([0;16]),"hashes":{"sha256":STANDARD_NO_PAD.encode(Sha256::digest(&bytes))}});
        decrypt(&mut bytes, &file).unwrap();
        assert_eq!(bytes, b"attachment fixture");
    }
    #[test]
    fn metadata_cannot_become_a_path_or_header() {
        assert_eq!(filename("../../photo.png").unwrap(), "photo.png");
        assert!(filename("../").is_err());
        assert!(filename("file\nname").is_err());
        assert_eq!(mime("IMAGE/PNG; charset=utf-8").unwrap(), "image/png");
        assert!(mime("text/plain\r\nAuthorization: secret").is_err());
    }
}
