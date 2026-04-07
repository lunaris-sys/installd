/// Ed25519 signature verification for .lunpkg packages.
///
/// The signature covers a SHA-256 hash computed over all package files
/// (excluding `signature.sig` itself) in deterministic sorted order.
/// Verification tries all trusted public keys and succeeds if any one
/// matches.
///
/// Key storage:
/// - System keys: `/etc/lunaris/trusted-keys/*.pub`
/// - User keys:   `~/.config/lunaris/trusted-keys/*.pub`
///
/// Key format: 32 bytes raw Ed25519 public key, or 44 characters
/// base64-encoded (one key per `.pub` file).
///
/// Hard fail: invalid or missing signature blocks the install. There
/// is no "Install Anyway" bypass.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Errors from signature verification.
#[derive(Debug, Error)]
pub enum SignatureError {
    #[error("signature.sig not found")]
    SignatureNotFound,
    #[error("signature.sig has invalid length (expected 64 bytes, got {0})")]
    InvalidSignatureLength(usize),
    #[error("no trusted public keys found")]
    NoTrustedKeys,
    #[error("signature verification failed: package may have been tampered with")]
    VerificationFailed,
    #[error("invalid public key in {path}: {reason}")]
    InvalidPublicKey { path: String, reason: String },
    #[error("IO: {0}")]
    Io(#[from] std::io::Error),
}

const SYSTEM_KEYS_DIR: &str = "/etc/lunaris/trusted-keys";

/// Get the system trusted keys directory.
fn system_keys_dir() -> PathBuf {
    std::env::var("LUNARIS_SYSTEM_KEYS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(SYSTEM_KEYS_DIR))
}

/// Get the user trusted keys directory.
fn user_keys_dir() -> PathBuf {
    std::env::var("LUNARIS_USER_KEYS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("~/.config"))
                .join("lunaris/trusted-keys")
        })
}

/// Verify the package signature.
///
/// Computes a deterministic SHA-256 hash over all files in the
/// extracted package (excluding `signature.sig`), then verifies the
/// Ed25519 signature against all trusted public keys.
///
/// Returns `Ok(())` if at least one key verifies the signature.
/// Returns an error on any failure (hard fail, no bypass).
pub fn verify_signature(extracted_dir: &Path) -> Result<(), SignatureError> {
    // 1. Load signature.
    let sig_path = extracted_dir.join("signature.sig");
    if !sig_path.exists() {
        return Err(SignatureError::SignatureNotFound);
    }
    let sig_bytes = fs::read(&sig_path)?;
    if sig_bytes.len() != 64 {
        return Err(SignatureError::InvalidSignatureLength(sig_bytes.len()));
    }
    let signature = Signature::from_bytes(
        sig_bytes
            .as_slice()
            .try_into()
            .map_err(|_| SignatureError::InvalidSignatureLength(sig_bytes.len()))?,
    );

    // 2. Compute content hash.
    let hash = compute_content_hash(extracted_dir)?;

    // 3. Load trusted keys.
    let keys = load_trusted_keys()?;
    if keys.is_empty() {
        return Err(SignatureError::NoTrustedKeys);
    }

    // 4. Try each key.
    for key in &keys {
        if key.verify(&hash, &signature).is_ok() {
            return Ok(());
        }
    }

    Err(SignatureError::VerificationFailed)
}

/// Compute a deterministic SHA-256 hash over all package files.
///
/// Files are processed in sorted order (by relative path). Each file
/// contributes its relative path (UTF-8) and its contents to the hash.
/// `signature.sig` is excluded.
pub fn compute_content_hash(dir: &Path) -> Result<[u8; 32], SignatureError> {
    let mut hasher = Sha256::new();
    let files = collect_files_sorted(dir)?;

    for rel_path in &files {
        let full_path = dir.join(rel_path);
        let content = fs::read(&full_path)?;

        // Hash the relative path.
        hasher.update(rel_path.as_bytes());
        // Hash a separator.
        hasher.update(b"\0");
        // Hash the file length as 8-byte LE.
        hasher.update((content.len() as u64).to_le_bytes());
        // Hash the file content.
        hasher.update(&content);
    }

    Ok(hasher.finalize().into())
}

/// Collect all files in a directory, sorted, excluding `signature.sig`.
fn collect_files_sorted(dir: &Path) -> Result<Vec<String>, SignatureError> {
    let mut files = BTreeSet::new();
    collect_files_recursive(dir, dir, &mut files)?;
    Ok(files.into_iter().collect())
}

fn collect_files_recursive(
    base: &Path,
    current: &Path,
    files: &mut BTreeSet<String>,
) -> Result<(), SignatureError> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_dir() {
            collect_files_recursive(base, &path, files)?;
        } else {
            let rel = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .to_string();

            // Exclude signature.sig from the hash.
            if rel != "signature.sig" {
                files.insert(rel);
            }
        }
    }
    Ok(())
}

/// Load all trusted Ed25519 public keys from system and user directories.
fn load_trusted_keys() -> Result<Vec<VerifyingKey>, SignatureError> {
    let mut keys = Vec::new();

    for dir in [system_keys_dir(), user_keys_dir()] {
        if !dir.exists() {
            continue;
        }
        let entries = fs::read_dir(&dir)?;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "pub") {
                match load_public_key(&path) {
                    Ok(key) => keys.push(key),
                    Err(e) => {
                        tracing::warn!("skipping key {}: {e}", path.display());
                    }
                }
            }
        }
    }

    Ok(keys)
}

/// Load a single Ed25519 public key from a file.
///
/// Accepts either raw 32-byte format or base64-encoded (44 chars).
fn load_public_key(path: &Path) -> Result<VerifyingKey, SignatureError> {
    let raw = fs::read(path)?;
    let path_str = path.display().to_string();

    let key_bytes: [u8; 32] = if raw.len() == 32 {
        // Raw binary format.
        raw.try_into().unwrap()
    } else {
        // Try base64 (strip whitespace first).
        let text = String::from_utf8_lossy(&raw);
        let trimmed = text.trim();
        decode_base64(trimmed).map_err(|e| SignatureError::InvalidPublicKey {
            path: path_str.clone(),
            reason: e,
        })?
    };

    VerifyingKey::from_bytes(&key_bytes).map_err(|e| SignatureError::InvalidPublicKey {
        path: path_str,
        reason: e.to_string(),
    })
}

/// Minimal base64 decoder for Ed25519 public keys (44 chars -> 32 bytes).
///
/// Uses standard base64 alphabet (A-Za-z0-9+/=).
fn decode_base64(input: &str) -> Result<[u8; 32], String> {
    fn val(c: u8) -> Result<u8, String> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(c - b'a' + 26),
            b'0'..=b'9' => Ok(c - b'0' + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            b'=' => Ok(0),
            _ => Err(format!("invalid base64 character: {}", c as char)),
        }
    }

    let bytes = input.as_bytes();
    if bytes.len() != 44 {
        return Err(format!(
            "expected 44 base64 characters, got {}",
            bytes.len()
        ));
    }

    let mut output = Vec::with_capacity(33);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 4 {
            return Err("incomplete base64 block".into());
        }
        let a = val(chunk[0])?;
        let b = val(chunk[1])?;
        let c = val(chunk[2])?;
        let d = val(chunk[3])?;
        output.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            output.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            output.push((c << 6) | d);
        }
    }

    if output.len() < 32 {
        return Err(format!("decoded {} bytes, expected 32", output.len()));
    }

    let mut result = [0u8; 32];
    result.copy_from_slice(&output[..32]);
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    /// Create a test keypair and return (signing_key, verifying_key_bytes).
    fn test_keypair() -> (SigningKey, [u8; 32]) {
        let sk = SigningKey::from_bytes(&[42u8; 32]);
        let vk_bytes = sk.verifying_key().to_bytes();
        (sk, vk_bytes)
    }

    /// Create a minimal extracted package, sign it, and write the key.
    fn create_signed_package(
        dir: &Path,
        keys_dir: &Path,
    ) -> SigningKey {
        let (sk, vk_bytes) = test_keypair();

        // Write package files.
        fs::write(dir.join("manifest.toml"), "[package]\nid = \"com.test\"\n").unwrap();
        fs::create_dir_all(dir.join("bin")).unwrap();
        fs::write(dir.join("bin/app"), "#!/bin/sh\necho hi").unwrap();

        // Compute hash and sign.
        let hash = compute_content_hash(dir).unwrap();
        let sig = sk.sign(&hash);
        fs::write(dir.join("signature.sig"), sig.to_bytes()).unwrap();

        // Write public key.
        fs::create_dir_all(keys_dir).unwrap();
        fs::write(keys_dir.join("test.pub"), vk_bytes).unwrap();

        sk
    }

    #[test]
    fn test_verify_valid_signature() {
        let pkg = tempfile::TempDir::new().unwrap();
        let keys = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYS_DIR", keys.path());
        std::env::set_var("LUNARIS_SYSTEM_KEYS_DIR", "/nonexistent");

        create_signed_package(pkg.path(), keys.path());
        assert!(verify_signature(pkg.path()).is_ok());

        std::env::remove_var("LUNARIS_USER_KEYS_DIR");
        std::env::remove_var("LUNARIS_SYSTEM_KEYS_DIR");
    }

    #[test]
    fn test_verify_tampered_content() {
        let pkg = tempfile::TempDir::new().unwrap();
        let keys = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYS_DIR", keys.path());
        std::env::set_var("LUNARIS_SYSTEM_KEYS_DIR", "/nonexistent");

        create_signed_package(pkg.path(), keys.path());

        // Tamper with a file after signing.
        fs::write(pkg.path().join("bin/app"), "EVIL BINARY").unwrap();

        let result = verify_signature(pkg.path());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("tampered"));

        std::env::remove_var("LUNARIS_USER_KEYS_DIR");
        std::env::remove_var("LUNARIS_SYSTEM_KEYS_DIR");
    }

    #[test]
    fn test_verify_missing_signature() {
        let pkg = tempfile::TempDir::new().unwrap();
        fs::write(pkg.path().join("manifest.toml"), "test").unwrap();

        let result = verify_signature(pkg.path());
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("not found"));
    }

    #[test]
    fn test_verify_wrong_key() {
        let pkg = tempfile::TempDir::new().unwrap();
        let keys = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYS_DIR", keys.path());
        std::env::set_var("LUNARIS_SYSTEM_KEYS_DIR", "/nonexistent");

        // Sign with one key.
        create_signed_package(pkg.path(), keys.path());

        // Replace trusted key with a different one.
        let other_sk = SigningKey::from_bytes(&[99u8; 32]);
        let other_vk = other_sk.verifying_key().to_bytes();
        fs::write(keys.path().join("test.pub"), other_vk).unwrap();

        let result = verify_signature(pkg.path());
        assert!(result.is_err());

        std::env::remove_var("LUNARIS_USER_KEYS_DIR");
        std::env::remove_var("LUNARIS_SYSTEM_KEYS_DIR");
    }

    #[test]
    fn test_verify_no_trusted_keys() {
        let pkg = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYS_DIR", "/nonexistent-user");
        std::env::set_var("LUNARIS_SYSTEM_KEYS_DIR", "/nonexistent-system");

        fs::write(pkg.path().join("manifest.toml"), "test").unwrap();
        fs::write(pkg.path().join("signature.sig"), [0u8; 64]).unwrap();

        let result = verify_signature(pkg.path());
        assert!(matches!(result, Err(SignatureError::NoTrustedKeys)));

        std::env::remove_var("LUNARIS_USER_KEYS_DIR");
        std::env::remove_var("LUNARIS_SYSTEM_KEYS_DIR");
    }

    #[test]
    fn test_content_hash_deterministic() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("a.txt"), "hello").unwrap();
        fs::write(dir.path().join("b.txt"), "world").unwrap();

        let hash1 = compute_content_hash(dir.path()).unwrap();
        let hash2 = compute_content_hash(dir.path()).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_content_hash_excludes_signature() {
        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("manifest.toml"), "test").unwrap();

        let hash_before = compute_content_hash(dir.path()).unwrap();

        // Adding signature.sig should not change the hash.
        fs::write(dir.path().join("signature.sig"), [0u8; 64]).unwrap();

        let hash_after = compute_content_hash(dir.path()).unwrap();
        assert_eq!(hash_before, hash_after);
    }

    #[test]
    fn test_content_hash_order_independent_of_creation() {
        let dir1 = tempfile::TempDir::new().unwrap();
        // Create in order a, b.
        fs::write(dir1.path().join("a.txt"), "hello").unwrap();
        fs::write(dir1.path().join("b.txt"), "world").unwrap();

        let dir2 = tempfile::TempDir::new().unwrap();
        // Create in order b, a.
        fs::write(dir2.path().join("b.txt"), "world").unwrap();
        fs::write(dir2.path().join("a.txt"), "hello").unwrap();

        let hash1 = compute_content_hash(dir1.path()).unwrap();
        let hash2 = compute_content_hash(dir2.path()).unwrap();
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_base64_key_loading() {
        let pkg = tempfile::TempDir::new().unwrap();
        let keys = tempfile::TempDir::new().unwrap();
        std::env::set_var("LUNARIS_USER_KEYS_DIR", keys.path());
        std::env::set_var("LUNARIS_SYSTEM_KEYS_DIR", "/nonexistent");

        let (sk, vk_bytes) = test_keypair();

        // Write package and sign.
        fs::write(pkg.path().join("manifest.toml"), "test").unwrap();
        let hash = compute_content_hash(pkg.path()).unwrap();
        let sig = sk.sign(&hash);
        fs::write(pkg.path().join("signature.sig"), sig.to_bytes()).unwrap();

        // Write key as base64.
        use std::fmt::Write;
        let mut b64 = String::new();
        // Simple base64 encode for 32 bytes.
        let encoded = encode_base64_simple(&vk_bytes);
        write!(&mut b64, "{encoded}").unwrap();
        fs::write(keys.path().join("test.pub"), &b64).unwrap();

        assert!(verify_signature(pkg.path()).is_ok());

        std::env::remove_var("LUNARIS_USER_KEYS_DIR");
        std::env::remove_var("LUNARIS_SYSTEM_KEYS_DIR");
    }

    /// Simple base64 encoder for tests.
    fn encode_base64_simple(data: &[u8]) -> String {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut result = String::new();
        for chunk in data.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
            let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            result.push(CHARS[((n >> 18) & 0x3F) as usize] as char);
            result.push(CHARS[((n >> 12) & 0x3F) as usize] as char);
            if chunk.len() > 1 {
                result.push(CHARS[((n >> 6) & 0x3F) as usize] as char);
            } else {
                result.push('=');
            }
            if chunk.len() > 2 {
                result.push(CHARS[(n & 0x3F) as usize] as char);
            } else {
                result.push('=');
            }
        }
        result
    }
}
