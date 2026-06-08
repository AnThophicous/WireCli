use crate::id::next_id;
use crate::safekey::redact_secrets;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const WCI_DIR: &str = ".wci";
const FCM_FILE: &str = "mm.fcm";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct FlashCacheFile {
    #[serde(default = "default_version")]
    v: u32,
    #[serde(default)]
    updated_at: i64,
    #[serde(default)]
    entries: Vec<FlashCacheItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FlashCacheItem {
    id: String,
    kind: String,
    content: String,
    #[serde(default)]
    tags: Vec<String>,
    source: String,
    updated_at: i64,
    rank: f64,
    hits: u32,
}

#[derive(Debug, Clone)]
pub struct FlashCacheRecord {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source: String,
    pub updated_at: i64,
    pub rank: f64,
    pub hits: u32,
}

pub struct FlashCacheMemory {
    path: PathBuf,
    max_entries: usize,
}

impl FlashCacheMemory {
    pub fn new(project_root: &Path, max_entries: usize) -> Result<Self, String> {
        let dir = project_root.join(WCI_DIR);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(FCM_FILE);
        if !path.exists() {
            fs::write(&path, "{\"v\":1,\"updated_at\":0,\"entries\":[]}\n")
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            path,
            max_entries: max_entries.clamp(16, 4096),
        })
    }

    pub fn refresh_project_signals(
        &self,
        project_root: &Path,
    ) -> Result<Vec<FlashCacheRecord>, String> {
        let mut records = Vec::new();
        if project_root.join("Cargo.toml").exists() {
            records.push(self.remember(
                "project_signal",
                "Rust workspace detected from Cargo.toml. Prefer cargo fmt/check/test validators for Rust edits.",
                &["rust".to_string(), "validator".to_string()],
                "fcm:project_scan",
                0.72,
            )?);
        }
        if project_root.join("package.json").exists() {
            records.push(self.remember(
                "project_signal",
                "JavaScript/TypeScript package detected from package.json. Inspect scripts before selecting npm/pnpm/yarn validation.",
                &["node".to_string(), "validator".to_string()],
                "fcm:project_scan",
                0.68,
            )?);
        }
        if project_root.join("go.mod").exists() {
            records.push(self.remember(
                "project_signal",
                "Go module detected from go.mod. Prefer go test ./... for Go edits.",
                &["go".to_string(), "validator".to_string()],
                "fcm:project_scan",
                0.68,
            )?);
        }
        if project_root.join("WIRE.md").exists() {
            records.push(self.remember(
                "project_signal",
                "WIRE.md exists in this project. Load WIRE.md rules before editing matching files.",
                &["wire-md".to_string(), "instructions".to_string()],
                "fcm:project_scan",
                0.8,
            )?);
        }
        Ok(records)
    }

    pub fn remember(
        &self,
        kind: &str,
        content: &str,
        tags: &[String],
        source: &str,
        rank: f64,
    ) -> Result<FlashCacheRecord, String> {
        let mut file = self.load()?;
        let now = now_ts();
        let kind = normalize_key(kind, "note");
        let content = compact_text(content, 900);
        let tags = normalize_tags(tags);
        let source = compact_text(source, 96);
        if let Some(existing) = file
            .entries
            .iter_mut()
            .find(|entry| entry.kind == kind && entry.content.eq_ignore_ascii_case(&content))
        {
            existing.updated_at = now;
            existing.rank = existing.rank.max(rank.clamp(0.0, 1.0));
            existing.hits = existing.hits.saturating_add(1);
            merge_tags(&mut existing.tags, &tags);
            existing.source = source;
            let record = record_from_item(existing);
            file.updated_at = now;
            self.prune_and_save(&mut file)?;
            return Ok(record);
        }
        let item = FlashCacheItem {
            id: next_id(),
            kind,
            content,
            tags,
            source,
            updated_at: now,
            rank: rank.clamp(0.0, 1.0),
            hits: 1,
        };
        let record = record_from_item(&item);
        file.entries.push(item);
        file.updated_at = now;
        self.prune_and_save(&mut file)?;
        Ok(record)
    }

    pub fn recall(&self, query: &str, limit: usize) -> Result<Vec<FlashCacheRecord>, String> {
        let mut file = self.load()?;
        let tokens = tokenize(query);
        let mut scored = file
            .entries
            .iter_mut()
            .map(|item| (score_item(item, &tokens), item))
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        let mut records = Vec::new();
        for (score, item) in scored.into_iter().take(limit) {
            if score <= 0.0 {
                continue;
            }
            item.hits = item.hits.saturating_add(1);
            records.push(record_from_item(item));
        }
        if !records.is_empty() {
            file.updated_at = now_ts();
            self.prune_and_save(&mut file)?;
        }
        Ok(records)
    }

    pub fn render_recontextualization(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Option<String>, String> {
        let records = self.recall(query, limit)?;
        if records.is_empty() {
            return Ok(None);
        }
        let mut out = String::from("Flash Cache Memory recontextualization:\n");
        out.push_str(
            "Use this as fast project-local cache evidence. Verify fresh facts with tools when risk is high.\n",
        );
        for record in records {
            out.push_str("- [");
            out.push_str(&record.kind);
            out.push_str("] ");
            out.push_str(&record.content);
            if !record.tags.is_empty() {
                out.push_str(" (tags: ");
                out.push_str(&record.tags.join(", "));
                out.push(')');
            }
            out.push_str(" {");
            out.push_str(&record.source);
            out.push_str(" hits=");
            out.push_str(&record.hits.to_string());
            out.push_str(" rank=");
            out.push_str(&format!("{:.2}", record.rank));
            out.push_str(" updated_at=");
            out.push_str(&record.updated_at.to_string());
            out.push('}');
            out.push('\n');
        }
        Ok(Some(out))
    }

    fn load(&self) -> Result<FlashCacheFile, String> {
        let raw = fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let mut file: FlashCacheFile = serde_json::from_str(&raw).unwrap_or_default();
        if file.v == 0 {
            file.v = 1;
        }
        Ok(file)
    }

    fn prune_and_save(&self, file: &mut FlashCacheFile) -> Result<(), String> {
        file.entries.sort_by(|a, b| {
            b.rank
                .partial_cmp(&a.rank)
                .unwrap_or(Ordering::Equal)
                .then_with(|| b.hits.cmp(&a.hits))
                .then_with(|| b.updated_at.cmp(&a.updated_at))
        });
        file.entries.truncate(self.max_entries);
        let raw = serde_json::to_string(file).map_err(|e| e.to_string())?;
        fs::write(&self.path, raw).map_err(|e| e.to_string())
    }
}

fn normalize_key(value: &str, fallback: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        fallback.to_string()
    } else {
        value
    }
}

fn normalize_tags(tags: &[String]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    tags.iter()
        .map(|tag| tag.trim().to_ascii_lowercase())
        .filter(|tag| !tag.is_empty())
        .filter(|tag| seen.insert(tag.clone()))
        .take(12)
        .collect()
}

fn merge_tags(existing: &mut Vec<String>, incoming: &[String]) {
    for tag in incoming {
        if !existing.iter().any(|value| value == tag) {
            existing.push(tag.clone());
        }
    }
    existing.truncate(16);
}

fn compact_text(value: &str, limit: usize) -> String {
    let text = redact_secrets(value)
        .replace(['\n', '\r', '\t'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let mut out = text
        .chars()
        .take(limit.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 2)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn score_item(item: &FlashCacheItem, tokens: &[String]) -> f64 {
    let mut score =
        item.rank * 4.0 + freshness_score(item.updated_at) + (item.hits.min(12) as f64) * 0.1;
    if tokens.is_empty() {
        return score;
    }
    let haystack = format!(
        "{} {} {} {}",
        item.kind,
        item.content.to_ascii_lowercase(),
        item.tags.join(" "),
        item.source
    );
    for token in tokens {
        if haystack.contains(token) {
            score += 2.0;
        }
        if item.tags.iter().any(|tag| tag == token) {
            score += 1.5;
        }
    }
    score
}

fn freshness_score(updated_at: i64) -> f64 {
    let age = now_ts().saturating_sub(updated_at).max(0) as f64;
    (1.0 / (1.0 + age / 86_400.0)).min(1.0)
}

fn record_from_item(item: &FlashCacheItem) -> FlashCacheRecord {
    FlashCacheRecord {
        id: item.id.clone(),
        kind: item.kind.clone(),
        content: item.content.clone(),
        tags: item.tags.clone(),
        source: item.source.clone(),
        updated_at: item.updated_at,
        rank: item.rank,
        hits: item.hits,
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn default_version() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::FlashCacheMemory;
    use crate::id::next_id;
    use std::fs;

    #[test]
    fn flash_cache_persists_and_recalls_context() {
        let root = std::env::temp_dir().join(format!("wirecli-fcm-{}", next_id()));
        fs::create_dir_all(&root).unwrap();
        let cache = FlashCacheMemory::new(&root, 32).unwrap();
        cache
            .remember(
                "summary",
                "The repo uses cargo test for Rust validation.",
                &["rust".to_string()],
                "test",
                0.9,
            )
            .unwrap();

        let cache = FlashCacheMemory::new(&root, 32).unwrap();
        let rendered = cache
            .render_recontextualization("Rust validation", 4)
            .unwrap()
            .unwrap();

        assert!(rendered.contains("cargo test"));
        assert!(root.join(".wci").join("mm.fcm").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn project_scan_adds_language_signals() {
        let root = std::env::temp_dir().join(format!("wirecli-fcm-scan-{}", next_id()));
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname='x'\n").unwrap();
        let cache = FlashCacheMemory::new(&root, 32).unwrap();
        cache.refresh_project_signals(&root).unwrap();
        let records = cache.recall("cargo validation", 4).unwrap();

        assert!(records.iter().any(|record| record.content.contains("Rust")));

        let _ = fs::remove_dir_all(root);
    }
}
