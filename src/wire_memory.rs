use crate::safekey::redact_secrets;
use std::fs;
use std::path::Path;

const WIRE_MEMORY_FILES: &[&str] = &["WIRE.md", "wire.md", "AGENTS.md", "agents.md"];
const DOCUMENT_LIMIT: usize = 32 * 1024;

#[derive(Debug, Clone, Default)]
pub struct WireMemoryBundle {
    pub documents: Vec<WireMemoryDocument>,
    pub rules: Vec<WireMemoryRule>,
}

#[derive(Debug, Clone)]
pub struct WireMemoryDocument {
    pub name: String,
    pub body: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireMemoryRule {
    pub source: String,
    pub line: usize,
    pub scope: WireRuleScope,
    pub pattern: String,
    pub instruction: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireRuleScope {
    Path,
    Type,
    Global,
}

impl WireMemoryBundle {
    pub fn load(root: &Path) -> Self {
        let mut documents = Vec::new();
        let mut rules = Vec::new();
        for name in WIRE_MEMORY_FILES {
            let path = root.join(name);
            if !path.exists() {
                continue;
            }
            let raw = match fs::read_to_string(&path) {
                Ok(raw) => raw,
                Err(_) => continue,
            };
            let raw = redact_secrets(&raw);
            for (index, line) in raw.lines().enumerate() {
                if let Some(rule) = parse_rule_line(name, index + 1, line) {
                    rules.push(rule);
                }
            }
            let truncated = raw.len() > DOCUMENT_LIMIT;
            let body = if truncated {
                raw.chars().take(DOCUMENT_LIMIT).collect::<String>()
            } else {
                raw
            };
            documents.push(WireMemoryDocument {
                name: (*name).to_string(),
                body,
                truncated,
            });
        }
        Self { documents, rules }
    }

    pub fn render_developer_context(&self) -> Option<String> {
        if self.documents.is_empty() {
            return None;
        }
        let mut out = String::new();
        for document in &self.documents {
            out.push_str("### ");
            out.push_str(&document.name);
            if document.truncated {
                out.push_str(" (truncated)");
            }
            out.push('\n');
            out.push_str(&document.body);
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push('\n');
        }
        Some(out)
    }

    pub fn render_relevant_rules(&self, query: &str) -> Option<String> {
        let candidates = path_candidates(query);
        let mut selected = Vec::new();
        for rule in &self.rules {
            if rule_matches_query(rule, &candidates) {
                selected.push(rule);
            }
        }
        if selected.is_empty() {
            return None;
        }
        let mut out = String::from("WIRE.md path/type rules matched for this request:\n");
        for rule in selected.into_iter().take(16) {
            out.push_str("- ");
            out.push_str(&rule.source);
            out.push(':');
            out.push_str(&rule.line.to_string());
            out.push_str(" [");
            out.push_str(match rule.scope {
                WireRuleScope::Path => "path",
                WireRuleScope::Type => "type",
                WireRuleScope::Global => "global",
            });
            out.push(' ');
            out.push_str(&rule.pattern);
            out.push_str("] ");
            out.push_str(&rule.instruction);
            out.push('\n');
        }
        Some(out)
    }
}

fn parse_rule_line(source: &str, line: usize, raw: &str) -> Option<WireMemoryRule> {
    let mut text = raw.trim();
    text = text
        .strip_prefix('-')
        .or_else(|| text.strip_prefix('*'))
        .unwrap_or(text)
        .trim();
    let lower = text.to_ascii_lowercase();
    let (scope, key) = if lower.starts_with("path:")
        || lower.starts_with("paths:")
        || lower.starts_with("path=")
        || lower.starts_with("paths=")
    {
        (
            WireRuleScope::Path,
            if lower.starts_with("paths") {
                "paths"
            } else {
                "path"
            },
        )
    } else if lower.starts_with("type:")
        || lower.starts_with("filetype:")
        || lower.starts_with("ext:")
        || lower.starts_with("type=")
        || lower.starts_with("filetype=")
        || lower.starts_with("ext=")
    {
        (WireRuleScope::Type, type_key(&lower))
    } else if lower.starts_with("global:") || lower.starts_with("global=") {
        (WireRuleScope::Global, "global")
    } else {
        return None;
    };

    let separator = if lower.starts_with(&format!("{key}=")) {
        '='
    } else {
        ':'
    };
    let after_key = text
        .split_once(separator)
        .map(|(_, rest)| rest.trim())
        .unwrap_or_default();
    let (pattern, instruction) = split_pattern_instruction(after_key);
    let instruction = instruction.trim();
    if pattern.trim().is_empty() || instruction.is_empty() {
        return None;
    }
    Some(WireMemoryRule {
        source: source.to_string(),
        line,
        scope,
        pattern: normalize_pattern(pattern),
        instruction: compact_rule_instruction(instruction),
    })
}

fn type_key(lower: &str) -> &'static str {
    if lower.starts_with("filetype") {
        "filetype"
    } else if lower.starts_with("ext") {
        "ext"
    } else {
        "type"
    }
}

fn split_pattern_instruction(value: &str) -> (&str, &str) {
    if let Some((pattern, instruction)) = value.split_once("->") {
        return (pattern, instruction);
    }
    if let Some((pattern, instruction)) = value.split_once('|') {
        return (pattern, instruction);
    }
    if let Some((pattern, instruction)) = value.split_once(';') {
        return (pattern, instruction);
    }
    value.split_once(' ').unwrap_or((value, ""))
}

fn normalize_pattern(value: &str) -> String {
    value
        .trim()
        .trim_matches('`')
        .trim_matches('"')
        .trim_matches('\'')
        .to_string()
}

fn compact_rule_instruction(value: &str) -> String {
    const LIMIT: usize = 360;
    let text = redact_secrets(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= LIMIT {
        return text;
    }
    let mut out = text
        .chars()
        .take(LIMIT.saturating_sub(1))
        .collect::<String>();
    out.push_str("...");
    out
}

fn path_candidates(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(|token| {
            token
                .trim_matches(|ch: char| {
                    matches!(
                        ch,
                        '`' | '"' | '\'' | ',' | ';' | ':' | ')' | '(' | '[' | ']' | '{' | '}'
                    )
                })
                .to_string()
        })
        .filter(|token| token.contains('/') || token.contains('.') || token.starts_with("*."))
        .take(48)
        .collect()
}

fn rule_matches_query(rule: &WireMemoryRule, candidates: &[String]) -> bool {
    if matches!(rule.scope, WireRuleScope::Global) {
        return true;
    }
    if candidates.is_empty() {
        return false;
    }
    candidates
        .iter()
        .any(|candidate| pattern_matches(&rule.pattern, candidate))
}

fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return false;
    }
    if pattern == candidate {
        return true;
    }
    if let Some(ext) = pattern.strip_prefix("*.") {
        return candidate.ends_with(&format!(".{ext}"));
    }
    if let Some(ext) = pattern.strip_prefix('.') {
        return candidate.ends_with(&format!(".{ext}"));
    }
    if let Some(prefix) = pattern
        .strip_suffix("/**")
        .or_else(|| pattern.strip_suffix("/*"))
    {
        return candidate == prefix
            || candidate.starts_with(&format!("{}/", prefix.trim_end_matches('/')));
    }
    if let Some((prefix, suffix)) = pattern.split_once("**/*") {
        return candidate.starts_with(prefix) && candidate.ends_with(suffix);
    }
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        return candidate.starts_with(prefix) && candidate.ends_with(suffix);
    }
    candidate.starts_with(pattern.trim_end_matches('/'))
}

#[cfg(test)]
mod tests {
    use super::{path_candidates, pattern_matches, WireMemoryBundle};
    use crate::id::next_id;
    use std::fs;

    #[test]
    fn parses_wire_rules_and_matches_paths() {
        let root = std::env::temp_dir().join(format!("wire-memory-rules-{}", next_id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("WIRE.md"),
            "- path: src/**/*.rs -> run cargo test after Rust changes\n- type: *.md -> keep docs concise\n",
        )
        .unwrap();

        let bundle = WireMemoryBundle::load(&root);
        assert_eq!(bundle.rules.len(), 2);
        let rendered = bundle
            .render_relevant_rules("edit src/main.rs and README.md")
            .unwrap();
        assert!(rendered.contains("cargo test"));
        assert!(rendered.contains("docs concise"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn path_candidate_and_glob_matching_are_conservative() {
        assert!(path_candidates("touch src/lib.rs").contains(&"src/lib.rs".to_string()));
        assert!(pattern_matches("src/**", "src/lib.rs"));
        assert!(pattern_matches("*.rs", "src/lib.rs"));
        assert!(!pattern_matches("docs/**", "src/lib.rs"));
    }
}
