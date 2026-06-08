use crate::config::AppPaths;
use crate::id::next_id;
use crate::safekey::redact_secrets;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct MemoryContextFile {
    #[serde(default = "default_version")]
    v: u32,
    #[serde(default)]
    p: BTreeMap<String, ProjectMemory>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProjectMemory {
    #[serde(default)]
    m: Vec<MemoryItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryItem {
    i: String,
    k: String,
    c: String,
    #[serde(default)]
    t: Vec<String>,
    #[serde(default)]
    s: Option<String>,
    u: i64,
    r: f64,
    o: String,
    #[serde(default)]
    a: u32,
    #[serde(default)]
    e: Option<i64>,
    #[serde(default)]
    p: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct MemoryRecord {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub source_session_id: Option<String>,
    pub origin: String,
    pub updated_at: i64,
    pub rank: f64,
    pub expires_at: Option<i64>,
    pub paths: Vec<String>,
    pub access_count: u32,
}

#[derive(Debug, Clone, Default)]
pub struct MemoryRememberOptions<'a> {
    pub source_session_id: Option<&'a str>,
    pub origin: &'a str,
    pub rank: f64,
    pub ttl_days: Option<u32>,
    pub paths: Vec<String>,
}

pub struct MemoryContextStore {
    path: PathBuf,
}

impl MemoryContextStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        if let Some(parent) = paths.memory_context_file.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if !paths.memory_context_file.exists() {
            fs::write(&paths.memory_context_file, "{\"v\":1,\"p\":{}}\n")
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            path: paths.memory_context_file.clone(),
        })
    }

    pub fn remember(
        &self,
        project_key: &str,
        kind: &str,
        content: &str,
        tags: &[String],
        source_session_id: Option<&str>,
        origin: &str,
        rank: f64,
    ) -> Result<MemoryRecord, String> {
        self.remember_with_options(
            project_key,
            kind,
            content,
            tags,
            MemoryRememberOptions {
                source_session_id,
                origin,
                rank,
                ttl_days: None,
                paths: Vec::new(),
            },
        )
    }

    pub fn remember_with_options(
        &self,
        project_key: &str,
        kind: &str,
        content: &str,
        tags: &[String],
        options: MemoryRememberOptions<'_>,
    ) -> Result<MemoryRecord, String> {
        let mut file = self.load()?;
        let project = file
            .p
            .entry(project_key.to_string())
            .or_insert_with(ProjectMemory::default);
        project.m.retain(|item| !is_expired(item, now_ts()));
        let now = now_ts();
        let kind = normalize_kind(kind);
        let content = compact_text(content, 512);
        let tags = normalize_tags(tags);
        let origin = compact_text(options.origin, 64);
        let rank = options.rank.clamp(0.0, 1.0);
        let expires_at = ttl_to_expires_at(now, options.ttl_days);
        let paths = normalize_paths(&options.paths);
        if let Some(existing) = project
            .m
            .iter_mut()
            .find(|item| item.k == kind && item.c.eq_ignore_ascii_case(&content))
        {
            existing.u = now;
            existing.r = existing.r.max(rank);
            existing.a = existing.a.saturating_add(1);
            merge_tags(&mut existing.t, &tags);
            merge_paths(&mut existing.p, &paths);
            existing.e = merge_expiry(existing.e, expires_at);
            if existing.s.is_none() {
                existing.s = options.source_session_id.map(|value| value.to_string());
            }
            existing.o = origin;
            let record = memory_record_from_item(existing);
            self.save(&file)?;
            return Ok(record);
        }
        let item = MemoryItem {
            i: next_id(),
            k: kind,
            c: content,
            t: tags,
            s: options.source_session_id.map(|value| value.to_string()),
            u: now,
            r: rank,
            o: origin,
            a: 1,
            e: expires_at,
            p: paths,
        };
        project.m.push(item.clone());
        prune_project(&mut project.m);
        self.save(&file)?;
        Ok(memory_record_from_item(&item))
    }

    pub fn recall(
        &self,
        project_key: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, String> {
        let file = self.load()?;
        let project = match file.p.get(project_key) {
            Some(project) => project,
            None => return Ok(Vec::new()),
        };
        let query_tokens = tokenize(query);
        let now = now_ts();
        let mut items = project
            .m
            .iter()
            .filter(|item| !is_expired(item, now))
            .map(|item| {
                let score = score_item(item, &query_tokens, &[]);
                (score, item)
            })
            .collect::<Vec<_>>();
        items.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        let mut out = Vec::new();
        for (score, item) in items.into_iter().take(limit) {
            if score <= 0.0 {
                continue;
            }
            out.push(memory_record_from_item(item));
        }
        Ok(out)
    }

    pub fn recall_for_paths(
        &self,
        project_key: &str,
        query: &str,
        paths: &[String],
        limit: usize,
    ) -> Result<Vec<MemoryRecord>, String> {
        let file = self.load()?;
        let project = match file.p.get(project_key) {
            Some(project) => project,
            None => return Ok(Vec::new()),
        };
        let query_tokens = tokenize(query);
        let path_tokens = normalize_paths(paths);
        let now = now_ts();
        let mut items = project
            .m
            .iter()
            .filter(|item| !is_expired(item, now))
            .map(|item| {
                let score = score_item(item, &query_tokens, &path_tokens);
                (score, item)
            })
            .collect::<Vec<_>>();
        items.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        Ok(items
            .into_iter()
            .take(limit)
            .filter(|(score, _)| *score > 0.0)
            .map(|(_, item)| memory_record_from_item(item))
            .collect())
    }

    pub fn render_compact(
        &self,
        project_key: &str,
        query: &str,
        limit: usize,
    ) -> Result<Option<String>, String> {
        let items = self.recall(project_key, query, limit)?;
        if items.is_empty() {
            return Ok(None);
        }
        let mut out = String::from("Project memory context:\n");
        for item in items {
            out.push_str("- [");
            out.push_str(&item.kind);
            out.push_str("] ");
            out.push_str(&item.content);
            if !item.tags.is_empty() {
                out.push_str(" (tags: ");
                out.push_str(&item.tags.join(", "));
                out.push(')');
            }
            if !item.paths.is_empty() {
                out.push_str(" (paths: ");
                out.push_str(&item.paths.join(", "));
                out.push(')');
            }
            if let Some(expires_at) = item.expires_at {
                out.push_str(" (expires_at: ");
                out.push_str(&expires_at.to_string());
                out.push(')');
            }
            if !item.origin.is_empty() {
                out.push_str(" {");
                out.push_str(&item.origin);
                out.push('}');
            }
            out.push('\n');
        }
        Ok(Some(out))
    }

    fn load(&self) -> Result<MemoryContextFile, String> {
        let raw = fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let mut file: MemoryContextFile =
            serde_json::from_str(&raw).unwrap_or_else(|_| MemoryContextFile::default());
        if file.v == 0 {
            file.v = 1;
        }
        Ok(file)
    }

    fn save(&self, file: &MemoryContextFile) -> Result<(), String> {
        let raw = serde_json::to_string(file).map_err(|e| e.to_string())?;
        fs::write(&self.path, raw).map_err(|e| e.to_string())
    }
}

fn default_version() -> u32 {
    1
}

fn normalize_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|tag| tag.trim().to_ascii_lowercase())
        .filter(|tag| !tag.is_empty())
        .take(8)
        .collect()
}

fn normalize_kind(kind: &str) -> String {
    let normalized = kind.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        "note".to_string()
    } else {
        normalized
    }
}

fn compact_text(value: &str, limit: usize) -> String {
    let mut text = redact_secrets(value).replace(['\n', '\r', '\t'], " ");
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let mut out = String::new();
    for ch in text.chars().take(limit.saturating_sub(1)) {
        out.push(ch);
    }
    out.push('…');
    out
}

fn tokenize(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 2)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn score_item(item: &MemoryItem, query_tokens: &[String], paths: &[String]) -> f64 {
    if query_tokens.is_empty() {
        return item.r + freshness_score(item.u) + path_score(item, paths);
    }

    let mut score = item.r * 4.0 + freshness_score(item.u);
    let haystack = format!(
        "{} {} {}",
        item.k,
        item.c.to_ascii_lowercase(),
        item.t.join(" ")
    );
    for token in query_tokens {
        if haystack.contains(token) {
            score += 2.5;
        }
        if item.t.iter().any(|tag| tag.eq_ignore_ascii_case(token)) {
            score += 1.5;
        }
        if item.k.eq_ignore_ascii_case(token) {
            score += 1.0;
        }
    }
    score += (item.a.min(8) as f64) * 0.2;
    score += path_score(item, paths);
    if item.e.is_some() {
        score -= 0.15;
    }
    score
}

fn path_score(item: &MemoryItem, paths: &[String]) -> f64 {
    if paths.is_empty() || item.p.is_empty() {
        return 0.0;
    }
    let mut score = 0.0;
    for path in paths {
        if item.p.iter().any(|scope| {
            path == scope || path.starts_with(&format!("{}/", scope.trim_end_matches('/')))
        }) {
            score += 2.0;
        }
    }
    score
}

fn freshness_score(updated_at: i64) -> f64 {
    let age = now_ts().saturating_sub(updated_at);
    let days = (age as f64) / 86_400.0;
    1.0 / (1.0 + days.max(0.0))
}

fn prune_project(items: &mut Vec<MemoryItem>) {
    let now = now_ts();
    items.retain(|item| !is_expired(item, now));
    items.sort_by(|a, b| {
        b.r.partial_cmp(&a.r)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.u.cmp(&a.u))
    });
    const MAX_ITEMS: usize = 512;
    if items.len() > MAX_ITEMS {
        items.truncate(MAX_ITEMS);
    }
}

fn normalize_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|path| {
            path.trim()
                .trim_start_matches("./")
                .trim_matches('/')
                .to_string()
        })
        .filter(|path| !path.is_empty() && !path.contains(".."))
        .take(16)
        .collect()
}

fn merge_paths(existing: &mut Vec<String>, incoming: &[String]) {
    for path in incoming {
        if !existing.iter().any(|value| value == path) {
            existing.push(path.clone());
        }
    }
    existing.truncate(24);
}

fn ttl_to_expires_at(now: i64, ttl_days: Option<u32>) -> Option<i64> {
    ttl_days.map(|days| now.saturating_add((days as i64).saturating_mul(86_400)))
}

fn merge_expiry(existing: Option<i64>, incoming: Option<i64>) -> Option<i64> {
    match (existing, incoming) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn is_expired(item: &MemoryItem, now: i64) -> bool {
    item.e.map(|expires_at| expires_at <= now).unwrap_or(false)
}

fn merge_tags(existing: &mut Vec<String>, incoming: &[String]) {
    for tag in incoming {
        if !existing.iter().any(|value| value == tag) {
            existing.push(tag.clone());
        }
    }
    existing.truncate(12);
}

fn memory_record_from_item(item: &MemoryItem) -> MemoryRecord {
    MemoryRecord {
        id: item.i.clone(),
        kind: item.k.clone(),
        content: item.c.clone(),
        tags: item.t.clone(),
        source_session_id: item.s.clone(),
        origin: item.o.clone(),
        updated_at: item.u,
        rank: item.r,
        expires_at: item.e,
        paths: item.p.clone(),
        access_count: item.a,
    }
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::{MemoryContextStore, MemoryRememberOptions};
    use crate::config::AppPaths;
    use crate::id::next_id;
    use std::fs;

    #[test]
    fn remember_deduplicates_existing_memory_context() {
        let paths = test_paths();
        let store = MemoryContextStore::new(&paths).unwrap();
        let tags = vec!["ui".to_string()];

        store
            .remember(
                &paths.project_key,
                "preference",
                "Use pink for tool execution rails.",
                &tags,
                Some("session-a"),
                "test",
                0.6,
            )
            .unwrap();
        store
            .remember(
                &paths.project_key,
                "preference",
                "Use pink for tool execution rails.",
                &["tool".to_string()],
                Some("session-b"),
                "test",
                0.9,
            )
            .unwrap();

        let records = store
            .recall(&paths.project_key, "pink tool execution", 10)
            .unwrap();

        assert_eq!(records.len(), 1);
        assert!(records[0].rank >= 0.9);
        assert!(records[0].tags.iter().any(|tag| tag == "ui"));
        assert!(records[0].tags.iter().any(|tag| tag == "tool"));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn recall_respects_expiration_and_path_scope() {
        let paths = test_paths();
        let store = MemoryContextStore::new(&paths).unwrap();

        store
            .remember_with_options(
                &paths.project_key,
                "rule",
                "Run cargo test for Rust runtime changes.",
                &["rust".to_string()],
                MemoryRememberOptions {
                    source_session_id: Some("session-a"),
                    origin: "test",
                    rank: 0.7,
                    ttl_days: None,
                    paths: vec!["src".to_string()],
                },
            )
            .unwrap();
        store
            .remember_with_options(
                &paths.project_key,
                "temporary",
                "Expired note should not be recalled.",
                &["temp".to_string()],
                MemoryRememberOptions {
                    source_session_id: Some("session-a"),
                    origin: "test",
                    rank: 1.0,
                    ttl_days: Some(0),
                    paths: vec!["docs".to_string()],
                },
            )
            .unwrap();

        let records = store
            .recall_for_paths(
                &paths.project_key,
                "runtime changes expired",
                &["src/main.rs".to_string()],
                10,
            )
            .unwrap();

        assert!(records
            .iter()
            .any(|record| record.content.contains("cargo test")));
        assert!(!records
            .iter()
            .any(|record| record.content.contains("Expired note")));
        assert!(records[0].paths.iter().any(|path| path == "src"));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    fn test_paths() -> AppPaths {
        let root_dir = std::env::temp_dir().join(format!("wirecli-memory-test-{}", next_id()));
        let wire_dir = root_dir.join(".wirecli");
        let config_dir = wire_dir.join("config");
        let data_dir = wire_dir.join("data");
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        AppPaths {
            root_dir: root_dir.clone(),
            project_key: root_dir.display().to_string(),
            wire_dir: wire_dir.clone(),
            config_dir: config_dir.clone(),
            config_file: config_dir.join("config.toml"),
            secret_key_file: config_dir.join("secret.key"),
            theme_file: wire_dir.join("theme.yaml"),
            mcp_file: config_dir.join("mcp_servers.json"),
            hooks_file: wire_dir.join("hooks.json"),
            data_dir: data_dir.clone(),
            history_db: data_dir.join("history.sqlite3"),
            anchor_db: data_dir.join("anchor.sqlite3"),
            memory_context_file: data_dir.join("memory_context.json"),
            sandboxes_dir: wire_dir.join("boxes"),
        }
    }
}
