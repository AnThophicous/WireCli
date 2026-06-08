use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

const REDACTED: &str = "[REDACTED_SECRET]";
const ENCRYPTED_PREFIX: &str = "wireenc:v1:";
const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;

const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_proj_",
    "sk-proj-",
    "ghp_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "AKIA",
    "ASIA",
];

const SECRET_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "access_token",
    "auth_token",
    "authorization",
    "bearer",
    "client_secret",
    "secret",
    "token",
    "password",
];

pub fn redact_secrets(text: &str) -> String {
    let mut out = Vec::new();
    for line in text.lines() {
        out.push(redact_line(line));
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    joined
}

pub fn protect_secret(key_path: &Path, plaintext: &str) -> Result<String, String> {
    if plaintext.trim().is_empty() || plaintext.starts_with(ENCRYPTED_PREFIX) {
        return Ok(plaintext.to_string());
    }

    let key_bytes = load_or_create_key(key_path)?;
    let key = aead_key(&key_bytes)?;
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    getrandom::fill(&mut nonce_bytes).map_err(|e| e.to_string())?;
    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.as_bytes().to_vec();
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| "failed to encrypt local secret".to_string())?;
    Ok(format!(
        "{ENCRYPTED_PREFIX}{}:{}",
        URL_SAFE_NO_PAD.encode(nonce_bytes),
        URL_SAFE_NO_PAD.encode(in_out)
    ))
}

pub fn reveal_secret(key_path: &Path, value: &str) -> Result<String, String> {
    let Some(payload) = value.strip_prefix(ENCRYPTED_PREFIX) else {
        return Ok(value.to_string());
    };
    let (nonce_text, cipher_text) = payload
        .split_once(':')
        .ok_or_else(|| "malformed encrypted local secret".to_string())?;
    let nonce_vec = URL_SAFE_NO_PAD
        .decode(nonce_text)
        .map_err(|_| "malformed encrypted local secret nonce".to_string())?;
    let nonce_bytes: [u8; NONCE_BYTES] = nonce_vec
        .try_into()
        .map_err(|_| "malformed encrypted local secret nonce".to_string())?;
    let mut in_out = URL_SAFE_NO_PAD
        .decode(cipher_text)
        .map_err(|_| "malformed encrypted local secret payload".to_string())?;
    let key_bytes = load_existing_key(key_path)?;
    let key = aead_key(&key_bytes)?;
    let plaintext = key
        .open_in_place(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::empty(),
            &mut in_out,
        )
        .map_err(|_| "failed to decrypt local secret; run `wirecli login` again".to_string())?;
    String::from_utf8(plaintext.to_vec())
        .map_err(|_| "decrypted local secret is not valid UTF-8".to_string())
}

pub fn is_protected_secret(value: &str) -> bool {
    value.starts_with(ENCRYPTED_PREFIX)
}

pub fn write_private_file(path: &Path, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| e.to_string())?;
        file.write_all(contents).map_err(|e| e.to_string())?;
        file.flush().map_err(|e| e.to_string())?;
        set_private_permissions(path)?;
        Ok(())
    }

    #[cfg(not(unix))]
    {
        fs::write(path, contents).map_err(|e| e.to_string())
    }
}

fn load_or_create_key(path: &Path) -> Result<[u8; KEY_BYTES], String> {
    if path.exists() {
        return load_existing_key(path);
    }
    let mut key = [0u8; KEY_BYTES];
    getrandom::fill(&mut key).map_err(|e| e.to_string())?;
    let mut encoded = URL_SAFE_NO_PAD.encode(key);
    encoded.push('\n');
    write_private_file(path, encoded.as_bytes())?;
    Ok(key)
}

fn load_existing_key(path: &Path) -> Result<[u8; KEY_BYTES], String> {
    let raw = fs::read(path).map_err(|e| e.to_string())?;
    let decoded = if raw.len() == KEY_BYTES {
        raw
    } else {
        let text = String::from_utf8_lossy(&raw);
        URL_SAFE_NO_PAD
            .decode(text.trim())
            .map_err(|_| "local encryption key is malformed".to_string())?
    };
    decoded
        .try_into()
        .map_err(|_| "local encryption key has invalid length".to_string())
}

fn aead_key(key_bytes: &[u8; KEY_BYTES]) -> Result<LessSafeKey, String> {
    let unbound = UnboundKey::new(&AES_256_GCM, key_bytes)
        .map_err(|_| "failed to initialize local encryption key".to_string())?;
    Ok(LessSafeKey::new(unbound))
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())
}

fn redact_line(line: &str) -> String {
    if let Some(redacted) = redact_assignment(line) {
        return redacted;
    }
    redact_tokens(line)
}

fn redact_assignment(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let key_hit = SECRET_KEYS.iter().any(|key| lower.contains(key));
    if !key_hit {
        return None;
    }
    for separator in ['=', ':'] {
        if let Some(index) = line.find(separator) {
            let (prefix, value) = line.split_at(index + 1);
            if value.trim().len() >= 8 {
                return Some(format!("{prefix} {REDACTED}"));
            }
        }
    }
    None
}

fn redact_tokens(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut token = String::new();
    for ch in line.chars() {
        if is_token_char(ch) {
            token.push(ch);
            continue;
        }
        flush_token(&mut out, &mut token);
        out.push(ch);
    }
    flush_token(&mut out, &mut token);
    out
}

fn flush_token(out: &mut String, token: &mut String) {
    if token.is_empty() {
        return;
    }
    if is_secret_token(token) {
        out.push_str(REDACTED);
    } else {
        out.push_str(token);
    }
    token.clear();
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/')
}

fn is_secret_token(token: &str) -> bool {
    let trimmed = token.trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | ',' | ';'));
    SECRET_PREFIXES
        .iter()
        .any(|prefix| trimmed.starts_with(prefix) && trimmed.len() >= prefix.len() + 16)
}

#[cfg(test)]
mod tests {
    use super::{protect_secret, redact_secrets, reveal_secret};
    use std::fs;

    #[test]
    fn redacts_openai_like_keys() {
        let text = "key sk-proj-abcdefghijklmnopqrstuvwxyz123456";
        assert!(!redact_secrets(text).contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn redacts_assignment_values() {
        let text = "api_key = abcdefghijklmnop";
        assert_eq!(redact_secrets(text), "api_key = [REDACTED_SECRET]");
    }

    #[test]
    fn protects_and_reveals_local_secret() {
        let dir =
            std::env::temp_dir().join(format!("wirecli-secret-test-{}", crate::id::next_id()));
        fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("secret.key");
        let secret = "wai_test_secret_value";

        let protected = protect_secret(&key_path, secret).unwrap();
        assert!(protected.starts_with("wireenc:v1:"));
        assert!(!protected.contains(secret));
        assert_eq!(reveal_secret(&key_path, &protected).unwrap(), secret);

        let _ = fs::remove_dir_all(dir);
    }
}
