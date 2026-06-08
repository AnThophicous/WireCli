use crate::config::AppPaths;
use crate::id::next_id;
use crate::safekey::{is_protected_secret, protect_secret, redact_secrets, reveal_secret};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct AnchorRecord {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub tags: String,
    pub importance: f64,
    pub confidence: f64,
    pub source_session_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct AnchorInput {
    pub kind: String,
    pub content: String,
    pub tags: Vec<String>,
    pub importance: f64,
    pub confidence: f64,
    pub source_session_id: Option<String>,
}

pub struct AnchorStore {
    db_path: PathBuf,
    secret_key_file: PathBuf,
}

impl AnchorStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        if let Some(parent) = paths.anchor_db.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let store = Self {
            db_path: paths.anchor_db.clone(),
            secret_key_file: paths.secret_key_file.clone(),
        };
        store.init()?;
        Ok(store)
    }

    fn connect(&self) -> Result<Connection, String> {
        let conn = Connection::open(&self.db_path).map_err(|e| e.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(3))
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(|e| e.to_string())?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(|e| e.to_string())?;
        Ok(conn)
    }

    fn init(&self) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS anchors (
                id TEXT PRIMARY KEY,
                project_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                content TEXT NOT NULL,
                tags TEXT NOT NULL,
                importance REAL NOT NULL,
                confidence REAL NOT NULL,
                source_session_id TEXT,
                created_at INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_anchors_project_kind
                ON anchors(project_key, kind, last_used_at DESC, created_at DESC);

            CREATE VIRTUAL TABLE IF NOT EXISTS anchors_fts
                USING fts5(id UNINDEXED, project_key UNINDEXED, kind UNINDEXED, content, tags, tokenize='unicode61');
            "#,
        )
        .map_err(|e| e.to_string())?;
        self.migrate_plaintext_anchors(&conn)?;
        Ok(())
    }

    fn migrate_plaintext_anchors(&self, conn: &Connection) -> Result<(), String> {
        let mut stmt = conn
            .prepare(
                "SELECT id, content, tags FROM anchors WHERE content IS NOT NULL AND content != ''",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut updates = Vec::new();
        for row in rows {
            let (id, content, tags) = row.map_err(|e| e.to_string())?;
            let plain = if is_protected_secret(&content) {
                self.reveal_text(&content).unwrap_or_default()
            } else {
                content.clone()
            };
            let stored = if is_protected_secret(&content) {
                content
            } else {
                self.protect_text(&plain)?
            };
            updates.push((id, stored, search_document(&plain, &tags)));
        }
        drop(stmt);

        for (id, stored, index_text) in updates {
            conn.execute(
                "UPDATE anchors SET content = ?1 WHERE id = ?2",
                params![stored, id],
            )
            .map_err(|e| e.to_string())?;
            conn.execute("DELETE FROM anchors_fts WHERE id = ?1", params![id])
                .map_err(|e| e.to_string())?;
            conn.execute(
                "INSERT INTO anchors_fts (id, project_key, kind, content, tags)
                 SELECT id, project_key, kind, ?2, tags FROM anchors WHERE id = ?1",
                params![id, index_text],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn remember(&self, project_key: &str, input: AnchorInput) -> Result<AnchorRecord, String> {
        let conn = self.connect()?;
        let id = next_id();
        let now = now_ts();
        let tags = normalize_tags(&input.tags);
        let content = redact_secrets(&input.content);
        let stored_content = self.protect_text(&content)?;
        let index_text = search_document(&content, &tags);

        conn.execute(
            "INSERT INTO anchors (id, project_key, kind, content, tags, importance, confidence, source_session_id, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                project_key,
                input.kind,
                stored_content,
                tags,
                input.importance,
                input.confidence,
                input.source_session_id,
                now,
                now,
            ],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT INTO anchors_fts (id, project_key, kind, content, tags) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                project_key,
                input.kind,
                index_text,
                tags,
            ],
        )
        .map_err(|e| e.to_string())?;

        Ok(AnchorRecord {
            id,
            kind: input.kind,
            content,
            tags,
            importance: input.importance,
            confidence: input.confidence,
            source_session_id: input.source_session_id,
            created_at: format_ts(now),
        })
    }

    pub fn recall(
        &self,
        project_key: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<AnchorRecord>, String> {
        let normalized = normalize_query(query);
        if normalized.is_empty() {
            return self.recent(project_key, limit);
        }
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                r#"
                SELECT a.id, a.kind, a.content, a.tags, a.importance, a.confidence, a.source_session_id, a.created_at
                FROM anchors_fts f
                JOIN anchors a ON a.id = f.id
                WHERE f.project_key = ?1 AND anchors_fts MATCH ?2
                ORDER BY a.last_used_at DESC, a.created_at DESC
                LIMIT ?3
                "#,
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![project_key, normalized, limit as i64], |row| {
                let created_at: i64 = row.get(7)?;
                let content: String = row.get(2)?;
                Ok(AnchorRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content: self.reveal_text(&content).unwrap_or(content),
                    tags: row.get(3)?,
                    importance: row.get(4)?,
                    confidence: row.get(5)?,
                    source_session_id: row.get(6)?,
                    created_at: format_ts(created_at),
                })
            })
            .map_err(|e| e.to_string())?;

        let mut items = Vec::new();
        for row in rows {
            let item = row.map_err(|e| e.to_string())?;
            let _ = self.touch(&item.id);
            items.push(item);
        }
        Ok(items)
    }

    fn recent(&self, project_key: &str, limit: usize) -> Result<Vec<AnchorRecord>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, content, tags, importance, confidence, source_session_id, created_at
                 FROM anchors WHERE project_key = ?1
                 ORDER BY last_used_at DESC, created_at DESC
                 LIMIT ?2",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![project_key, limit as i64], |row| {
                let created_at: i64 = row.get(7)?;
                let content: String = row.get(2)?;
                Ok(AnchorRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content: self.reveal_text(&content).unwrap_or(content),
                    tags: row.get(3)?,
                    importance: row.get(4)?,
                    confidence: row.get(5)?,
                    source_session_id: row.get(6)?,
                    created_at: format_ts(created_at),
                })
            })
            .map_err(|e| e.to_string())?;

        let mut items = Vec::new();
        for row in rows {
            items.push(row.map_err(|e| e.to_string())?);
        }
        Ok(items)
    }

    pub fn latest_summary(
        &self,
        project_key: &str,
        session_id: &str,
    ) -> Result<Option<AnchorRecord>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, kind, content, tags, importance, confidence, source_session_id, created_at
                 FROM anchors WHERE project_key = ?1 AND kind = 'session_summary' AND source_session_id = ?2
                 ORDER BY created_at DESC LIMIT 1",
            )
            .map_err(|e| e.to_string())?;
        let row = stmt
            .query_row(params![project_key, session_id], |row| {
                let created_at: i64 = row.get(7)?;
                let content: String = row.get(2)?;
                Ok(AnchorRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content: self.reveal_text(&content).unwrap_or(content),
                    tags: row.get(3)?,
                    importance: row.get(4)?,
                    confidence: row.get(5)?,
                    source_session_id: row.get(6)?,
                    created_at: format_ts(created_at),
                })
            })
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(row)
    }

    pub fn touch(&self, anchor_id: &str) -> Result<(), String> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE anchors SET last_used_at = ?2 WHERE id = ?1",
            params![anchor_id, now_ts()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn protect_text(&self, value: &str) -> Result<String, String> {
        protect_secret(&self.secret_key_file, value)
    }

    fn reveal_text(&self, value: &str) -> Result<String, String> {
        reveal_secret(&self.secret_key_file, value)
    }
}

fn normalize_tags(tags: &[String]) -> String {
    tags.iter()
        .map(|tag| tag.trim())
        .filter(|tag| !tag.is_empty())
        .map(|tag| tag.to_ascii_lowercase())
        .collect::<Vec<_>>()
        .join(",")
}

fn normalize_query(query: &str) -> String {
    let tokens = query
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 2)
        .map(|token| token.to_ascii_lowercase())
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        String::new()
    } else {
        tokens.join(" ")
    }
}

fn search_document(content: &str, tags: &str) -> String {
    let mut tokens = Vec::new();
    for token in content
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .chain(tags.split(|ch: char| !ch.is_ascii_alphanumeric()))
    {
        let token = token.trim().to_ascii_lowercase();
        if token.len() < 2 || tokens.iter().any(|existing: &String| existing == &token) {
            continue;
        }
        tokens.push(token);
        if tokens.len() >= 160 {
            break;
        }
    }
    tokens.join(" ")
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn format_ts(ts: i64) -> String {
    ts.to_string()
}
