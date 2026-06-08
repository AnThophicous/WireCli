use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn now_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    secs.to_string()
}

pub(crate) fn sanitize_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "sandbox".to_string();
    }

    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }

    if out.is_empty() {
        "sandbox".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_name;

    #[test]
    fn sanitize_name_rewrites_spaces() {
        assert_eq!(sanitize_name("My Sandbox"), "My-Sandbox");
    }
}
