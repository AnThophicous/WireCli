use crate::config::AppPaths;
use crate::id::next_id;
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
}

impl AnchorStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        if let Some(parent) = paths.anchor_db.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let store = Self {
            db_path: paths.anchor_db.clone(),
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
        Ok(())
    }

    pub fn remember(&self, project_key: &str, input: AnchorInput) -> Result<AnchorRecord, String> {
        let conn = self.connect()?;
        let id = next_id();
        let now = now_ts();
        let tags = normalize_tags(&input.tags);

        conn.execute(
            "INSERT INTO anchors (id, project_key, kind, content, tags, importance, confidence, source_session_id, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                id,
                project_key,
                input.kind,
                input.content,
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
                input.content,
                tags,
            ],
        )
        .map_err(|e| e.to_string())?;

        Ok(AnchorRecord {
            id,
            kind: input.kind,
            content: input.content,
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
                Ok(AnchorRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content: row.get(2)?,
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
                Ok(AnchorRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content: row.get(2)?,
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
                Ok(AnchorRecord {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content: row.get(2)?,
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

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn format_ts(ts: i64) -> String {
    ts.to_string()
}
