use crate::config::AppPaths;
use crate::id::next_id;
use crate::safekey::redact_secrets;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct LabInput {
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub confidence: f64,
    pub source_session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LabRecord {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub confidence: f64,
    pub source_session_id: Option<String>,
    pub updated_at: i64,
    pub origin: String,
}

pub struct LabStore {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LabFile {
    #[serde(default = "default_version")]
    v: u32,
    #[serde(default)]
    p: BTreeMap<String, LabProject>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct LabProject {
    #[serde(default)]
    l: Vec<LabItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LabItem {
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
}

impl LabStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        let path = paths.data_dir.join("lab.json");
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if !path.exists() {
            fs::write(&path, "{\"v\":1,\"p\":{}}\n").map_err(|e| e.to_string())?;
        }
        Ok(Self { path })
    }

    pub fn remember(
        &self,
        project_key: &str,
        input: LabInput,
        origin: &str,
    ) -> Result<LabRecord, String> {
        let mut file = self.load()?;
        let project = file
            .p
            .entry(project_key.to_string())
            .or_insert_with(LabProject::default);
        let now = now_ts();
        let kind = normalize_kind(&input.kind);
        let content = compact_text(&input.content, 512);
        let tags = normalize_tags(&input.tags);
        let confidence = input.confidence.clamp(0.0, 1.0);
        if let Some(existing) = project
            .l
            .iter_mut()
            .find(|item| item.k == kind && item.c.eq_ignore_ascii_case(&content))
        {
            existing.u = now;
            existing.r = existing.r.max(confidence);
            merge_tags(&mut existing.t, &tags);
            if existing.s.is_none() {
                existing.s = input.source_session_id.clone();
            }
            existing.o = compact_text(origin, 64);
            let record = record_from_item(existing);
            self.save(&file)?;
            return Ok(record);
        }

        let item = LabItem {
            i: next_id(),
            k: kind,
            c: content,
            t: tags,
            s: input.source_session_id,
            u: now,
            r: confidence,
            o: compact_text(origin, 64),
        };
        let record = record_from_item(&item);
        project.l.push(item);
        prune(&mut project.l);
        self.save(&file)?;
        Ok(record)
    }

    pub fn recall(
        &self,
        project_key: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<LabRecord>, String> {
        let file = self.load()?;
        let Some(project) = file.p.get(project_key) else {
            return Ok(Vec::new());
        };
        let query_tokens = tokenize(query);
        let mut scored = project
            .l
            .iter()
            .map(|item| (score_item(item, &query_tokens), item))
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        Ok(scored
            .into_iter()
            .filter(|(score, _)| *score > 0.0)
            .take(limit)
            .map(|(_, item)| record_from_item(item))
            .collect())
    }

    pub fn render_compact(
        &self,
        project_key: &str,
        query: &str,
        limit: usize,
    ) -> Result<Option<String>, String> {
        let records = self.recall(project_key, query, limit)?;
        if records.is_empty() {
            return Ok(None);
        }
        let mut out = String::from("AFUP adaptation context:\n");
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
            out.push('\n');
        }
        Ok(Some(out))
    }

    pub fn observe_user_prompt(
        &self,
        project_key: &str,
        session_id: &str,
        user_prompt: &str,
    ) -> Result<Vec<LabRecord>, String> {
        let mut records = Vec::new();
        for input in infer_lab_inputs(user_prompt, Some(session_id.to_string())) {
            records.push(self.remember(project_key, input, "observer")?);
        }
        Ok(records)
    }

    fn load(&self) -> Result<LabFile, String> {
        let raw = fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let mut file: LabFile = serde_json::from_str(&raw).unwrap_or_default();
        if file.v == 0 {
            file.v = 1;
        }
        Ok(file)
    }

    fn save(&self, file: &LabFile) -> Result<(), String> {
        let raw = serde_json::to_string(file).map_err(|e| e.to_string())?;
        fs::write(&self.path, raw).map_err(|e| e.to_string())
    }
}

fn infer_lab_inputs(prompt: &str, source_session_id: Option<String>) -> Vec<LabInput> {
    let lower = prompt.to_lowercase();
    let mut inputs = Vec::new();
    if lower.contains("pt-br") || lower.contains("portugu") || portuguese_score(prompt) >= 3 {
        inputs.push(LabInput {
            kind: "preference".to_string(),
            content: "User prefers pt-BR wording when their current messages are in Portuguese."
                .to_string(),
            tags: vec!["language".to_string(), "pt-br".to_string()],
            confidence: 0.72,
            source_session_id: source_session_id.clone(),
        });
    }
    if contains_any(
        &lower,
        &[
            "seguran",
            "security",
            "criptograf",
            "encrypt",
            "token",
            "secret",
            "proteger",
            "privacidade",
        ],
    ) {
        inputs.push(LabInput {
            kind: "preference".to_string(),
            content: "User cares about security, encrypted storage, secret redaction, and explicit privacy boundaries."
                .to_string(),
            tags: vec!["security".to_string(), "privacy".to_string()],
            confidence: 0.8,
            source_session_id: source_session_id.clone(),
        });
    }
    if contains_any(
        &lower,
        &["spam", "separator", "separador", "poluido", "poluído"],
    ) {
        inputs.push(LabInput {
            kind: "preference".to_string(),
            content: "User dislikes noisy tool feeds; prefer concise grouped traces that still show useful discovered output."
                .to_string(),
            tags: vec!["tui".to_string(), "tool-output".to_string()],
            confidence: 0.78,
            source_session_id: source_session_id.clone(),
        });
    }
    if contains_any(
        &lower,
        &[
            "gosto de",
            "gosto muito",
            "prefiro",
            "eu prefiro",
            "nao gosto",
            "não gosto",
            "odeio",
            "quero sempre",
            "usa sempre",
            "não use",
            "nao use",
        ],
    ) {
        inputs.push(LabInput {
            kind: "preference".to_string(),
            content: format!(
                "User stated preference signal: {}",
                compact_text(prompt, 360)
            ),
            tags: vec!["explicit-preference".to_string()],
            confidence: 0.65,
            source_session_id,
        });
    }
    inputs
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn record_from_item(item: &LabItem) -> LabRecord {
    LabRecord {
        id: item.i.clone(),
        kind: item.k.clone(),
        content: item.c.clone(),
        tags: item.t.clone(),
        confidence: item.r,
        source_session_id: item.s.clone(),
        updated_at: item.u,
        origin: item.o.clone(),
    }
}

fn prune(items: &mut Vec<LabItem>) {
    items.sort_by(|a, b| {
        b.r.partial_cmp(&a.r)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.u.cmp(&a.u))
    });
    items.truncate(160);
}

fn merge_tags(existing: &mut Vec<String>, incoming: &[String]) {
    for tag in incoming {
        if !existing.iter().any(|value| value == tag) {
            existing.push(tag.clone());
        }
    }
    existing.truncate(10);
}

fn normalize_kind(kind: &str) -> String {
    let kind = kind.trim().to_ascii_lowercase();
    if kind.is_empty() {
        "preference".to_string()
    } else {
        kind
    }
}

fn normalize_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|tag| tag.trim().to_ascii_lowercase())
        .filter(|tag| !tag.is_empty())
        .take(10)
        .collect()
}

fn compact_text(value: &str, limit: usize) -> String {
    let mut text = redact_secrets(value).replace(['\n', '\r', '\t'], " ");
    text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= limit {
        return text;
    }
    let mut out = text
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
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

fn score_item(item: &LabItem, query_tokens: &[String]) -> f64 {
    if query_tokens.is_empty() {
        return item.r + freshness_score(item.u);
    }
    let haystack = format!(
        "{} {} {}",
        item.k,
        item.c.to_ascii_lowercase(),
        item.t.join(" ")
    );
    let mut score = item.r * 4.0 + freshness_score(item.u);
    for token in query_tokens {
        if haystack.contains(token) {
            score += 2.5;
        }
        if item.t.iter().any(|tag| tag == token) {
            score += 1.5;
        }
    }
    score
}

fn freshness_score(updated_at: i64) -> f64 {
    let age = now_ts().saturating_sub(updated_at).max(0) as f64;
    (1.0 / (1.0 + age / 86_400.0)).min(1.0)
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

fn portuguese_score(value: &str) -> usize {
    let lower = value.to_lowercase();
    [
        "voce",
        "você",
        "nao",
        "não",
        "corrige",
        "segurança",
        "criptografia",
        "ferramenta",
        "diretório",
        "diretorio",
        "arquivo",
        "gosto",
        "prefiro",
    ]
    .iter()
    .filter(|marker| lower.contains(*marker))
    .count()
}

#[cfg(test)]
mod tests {
    use super::LabStore;
    use crate::config::AppPaths;
    use crate::id::next_id;
    use std::fs;

    #[test]
    fn observes_security_and_ui_preferences() {
        let paths = test_paths();
        let store = LabStore::new(&paths).unwrap();
        let learned = store
            .observe_user_prompt(
                &paths.project_key,
                "session",
                "corrige esse spam e melhora a criptografia dos tokens",
            )
            .unwrap();
        assert!(learned.len() >= 2);
        let context = store
            .render_compact(&paths.project_key, "token spam", 8)
            .unwrap()
            .unwrap();
        assert!(context.contains("security") || context.contains("segurança"));
        assert!(context.contains("tool feeds") || context.contains("tool-output"));
        let _ = fs::remove_dir_all(paths.root_dir);
    }

    fn test_paths() -> AppPaths {
        let root_dir = std::env::temp_dir().join(format!("wirecli-lab-test-{}", next_id()));
        let wire_dir = root_dir.join(".wirecli");
        let config_dir = wire_dir.join("config");
        let data_dir = wire_dir.join("data");
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
