use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub fn next_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let raw = (nanos << 16) | (counter & 0xFFFF);
    pad_left_base62(encode_base62(raw), 64)
}

fn encode_base62(mut value: u128) -> String {
    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }

    let mut out = Vec::new();
    while value > 0 {
        let index = (value % 62) as usize;
        out.push(ALPHABET[index] as char);
        value /= 62;
    }
    out.reverse();
    out.into_iter().collect()
}

fn pad_left_base62(value: String, width: usize) -> String {
    if value.len() >= width {
        return value;
    }

    let mut out = String::with_capacity(width);
    out.extend(std::iter::repeat('0').take(width - value.len()));
    out.push_str(&value);
    out
}

#[cfg(test)]
mod tests {
    use super::next_id;

    #[test]
    fn id_is_64_chars_and_alphanumeric() {
        let id = next_id();
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|ch| ch.is_ascii_alphanumeric()));
    }
}
