use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);
static SEED: OnceLock<u64> = OnceLock::new();

pub fn next_id() -> String {
    let now = now_nanos();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = process::id() as u64;
    let seed = *SEED.get_or_init(|| mix64(now ^ pid ^ counter ^ addr_salt()));

    let mut state = mix64(seed ^ now ^ counter.rotate_left(13) ^ pid.rotate_left(7));
    let mut out = String::with_capacity(64);
    for round in 0..4u64 {
        state =
            mix64(state ^ now.rotate_left((round as u32 * 11) % 64) ^ counter.wrapping_add(round));
        out.push_str(&encode_base62_fixed(state, 16));
    }
    out
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn addr_salt() -> u64 {
    let ptr = &COUNTER as *const AtomicU64 as usize as u64;
    mix64(ptr ^ 0x9E3779B97F4A7C15)
}

fn mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E3779B97F4A7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D049BB133111EB);
    value ^ (value >> 31)
}

fn encode_base62_fixed(mut value: u64, width: usize) -> String {
    const ALPHABET: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut chars = vec!['0'; width];
    for slot in chars.iter_mut().rev() {
        let index = (value % 62) as usize;
        *slot = ALPHABET[index] as char;
        value /= 62;
    }
    chars.into_iter().collect()
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

    #[test]
    fn ids_are_distinct() {
        let first = next_id();
        let second = next_id();
        assert_ne!(first, second);
    }
}
