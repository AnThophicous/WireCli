use crate::config::AppPaths;
use crate::id::next_id;
use crate::safekey::{is_protected_secret, protect_secret, redact_secrets, reveal_secret};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub summary: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionEvent {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TimelineEvent {
    pub kind: String,
    pub role: Option<String>,
    pub content: Option<String>,
    pub command: Option<String>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub exit_code: Option<i64>,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct SessionMemoryRecord {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub content: String,
    pub tags: String,
    pub created_at: String,
}

impl SessionEvent {
    pub fn developer(content: String) -> Self {
        Self {
            role: "developer".to_string(),
            content,
        }
    }

    pub fn user(content: String) -> Self {
        Self {
            role: "user".to_string(),
            content,
        }
    }

    pub fn assistant(content: String) -> Self {
        Self {
            role: "assistant".to_string(),
            content,
        }
    }
}

pub struct HistoryStore {
    db_path: PathBuf,
    secret_key_file: PathBuf,
    project_key: String,
    root_path: String,
}

pub struct SessionStore {
    history: HistoryStore,
}

impl HistoryStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        Self::new_for_project(
            paths,
            &paths.project_key,
            &paths.root_dir.display().to_string(),
        )
    }

    pub fn new_for_project(
        paths: &AppPaths,
        project_key: &str,
        root_path: &str,
    ) -> Result<Self, String> {
        if let Some(parent) = paths.history_db.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let store = Self {
            db_path: paths.history_db.clone(),
            secret_key_file: paths.secret_key_file.clone(),
            project_key: project_key.to_string(),
            root_path: root_path.to_string(),
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
            CREATE TABLE IF NOT EXISTS projects (
                key TEXT PRIMARY KEY,
                root_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                project_key TEXT NOT NULL REFERENCES projects(key) ON DELETE CASCADE,
                title TEXT NOT NULL,
                workspace TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                project_key TEXT NOT NULL REFERENCES projects(key) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                role TEXT,
                content TEXT,
                command TEXT,
                stdout TEXT,
                stderr TEXT,
                exit_code INTEGER,
                created_at INTEGER NOT NULL,
                seq INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS session_memory (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                project_key TEXT NOT NULL REFERENCES projects(key) ON DELETE CASCADE,
                kind TEXT NOT NULL,
                content TEXT NOT NULL,
                tags TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                last_used_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_events_session_seq
                ON events(session_id, seq);
            CREATE INDEX IF NOT EXISTS idx_sessions_updated_at
                ON sessions(project_key, updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_project_updated
                ON sessions(project_key, updated_at DESC, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_events_project_session_seq
                ON events(project_key, session_id, seq);
            CREATE INDEX IF NOT EXISTS idx_session_memory_project_session_used
                ON session_memory(project_key, session_id, last_used_at DESC, created_at DESC);
            "#,
        )
        .map_err(|e| e.to_string())
        .and_then(|_| self.migrate_legacy_schema(&conn))
        .and_then(|_| self.migrate_plaintext_history(&conn))
        .and_then(|_| self.ensure_project(&conn, &self.project_key, &self.root_path, now_ts()))
    }

    fn migrate_legacy_schema(&self, conn: &Connection) -> Result<(), String> {
        if !table_has_column(conn, "sessions", "project_key")? {
            conn.execute("ALTER TABLE sessions ADD COLUMN project_key TEXT", [])
                .map_err(|e| e.to_string())?;
            conn.execute(
                "UPDATE sessions SET project_key = ?1 WHERE project_key IS NULL OR project_key = ''",
                params![self.project_key],
            )
            .map_err(|e| e.to_string())?;
        }

        if !table_has_column(conn, "events", "project_key")? {
            conn.execute("ALTER TABLE events ADD COLUMN project_key TEXT", [])
                .map_err(|e| e.to_string())?;
            conn.execute(
                r#"
                UPDATE events
                SET project_key = (
                    SELECT project_key FROM sessions WHERE sessions.id = events.session_id
                )
                WHERE project_key IS NULL OR project_key = ''
                "#,
                [],
            )
            .map_err(|e| e.to_string())?;
        }

        Ok(())
    }

    fn migrate_plaintext_history(&self, conn: &Connection) -> Result<(), String> {
        let mut migrated = 0usize;
        migrated += self.migrate_text_column(conn, "sessions", "id", "title")?;
        migrated += self.migrate_text_column(conn, "events", "id", "content")?;
        migrated += self.migrate_text_column(conn, "events", "id", "command")?;
        migrated += self.migrate_text_column(conn, "events", "id", "stdout")?;
        migrated += self.migrate_text_column(conn, "events", "id", "stderr")?;
        migrated += self.migrate_text_column(conn, "session_memory", "id", "content")?;

        if migrated > 0 {
            conn.execute_batch(
                r#"
                PRAGMA wal_checkpoint(TRUNCATE);
                VACUUM;
                PRAGMA wal_checkpoint(TRUNCATE);
                "#,
            )
            .map_err(|e| e.to_string())?;
        }

        Ok(())
    }

    fn migrate_text_column(
        &self,
        conn: &Connection,
        table: &str,
        id_column: &str,
        value_column: &str,
    ) -> Result<usize, String> {
        let select_sql = format!(
            "SELECT {id_column}, {value_column} FROM {table} WHERE {value_column} IS NOT NULL AND {value_column} != ''"
        );
        let update_sql = format!("UPDATE {table} SET {value_column} = ?1 WHERE {id_column} = ?2");
        let mut stmt = conn.prepare(&select_sql).map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        let mut updates = Vec::new();
        for row in rows {
            let (id, value) = row.map_err(|e| e.to_string())?;
            if !is_protected_secret(&value) {
                updates.push((id, self.protect_text(&value)?));
            }
        }
        drop(stmt);

        for (id, value) in &updates {
            conn.execute(&update_sql, params![value, id])
                .map_err(|e| e.to_string())?;
        }

        Ok(updates.len())
    }

    pub fn create_session(
        &self,
        project_key: &str,
        root_path: &str,
        prompt: &str,
    ) -> Result<SessionSummary, String> {
        let conn = self.connect()?;
        let id = next_id();
        let now = now_ts();
        let title = prompt.trim().to_string();
        let stored_title = self.protect_text(&title)?;
        self.ensure_project(&conn, project_key, root_path, now)?;

        conn.execute(
            "INSERT INTO sessions (id, project_key, title, workspace, created_at, updated_at) VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
            params![id, project_key, stored_title, now, now],
        )
        .map_err(|e| e.to_string())?;

        Ok(SessionSummary {
            id,
            created_at: format_ts(now),
            updated_at: format_ts(now),
            summary: if title.is_empty() { None } else { Some(title) },
        })
    }

    pub fn list_sessions(&self, project_key: &str) -> Result<Vec<SessionSummary>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT
                    sessions.id,
                    COALESCE(
                        NULLIF(sessions.title, ''),
                        (
                            SELECT events.content
                            FROM events
                            WHERE events.session_id = sessions.id
                              AND events.project_key = sessions.project_key
                              AND events.kind = 'message'
                              AND events.role = 'user'
                            ORDER BY events.created_at ASC, events.seq ASC
                            LIMIT 1
                        )
                    ) AS title,
                    sessions.created_at,
                    sessions.updated_at
                 FROM sessions
                 WHERE project_key = ?1
                 ORDER BY updated_at DESC, created_at DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![project_key], |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let created_at: i64 = row.get(2)?;
                let updated_at: i64 = row.get(3)?;
                let title = self.reveal_text(&title).unwrap_or(title);
                Ok(SessionSummary {
                    id,
                    created_at: format_ts(created_at),
                    updated_at: format_ts(updated_at),
                    summary: if title.is_empty() { None } else { Some(title) },
                })
            })
            .map_err(|e| e.to_string())?;

        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|e| e.to_string())?);
        }
        Ok(sessions)
    }

    pub fn load_session(
        &self,
        project_key: &str,
        session_id: &str,
    ) -> Result<Option<SessionSummary>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT
                    sessions.id,
                    COALESCE(
                        NULLIF(sessions.title, ''),
                        (
                            SELECT events.content
                            FROM events
                            WHERE events.session_id = sessions.id
                              AND events.project_key = sessions.project_key
                              AND events.kind = 'message'
                              AND events.role = 'user'
                            ORDER BY events.created_at ASC, events.seq ASC
                            LIMIT 1
                        )
                    ) AS title,
                    sessions.created_at,
                    sessions.updated_at
                 FROM sessions
                 WHERE id = ?1 AND project_key = ?2",
            )
            .map_err(|e| e.to_string())?;
        let session = stmt
            .query_row(params![session_id, project_key], |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let created_at: i64 = row.get(2)?;
                let updated_at: i64 = row.get(3)?;
                let title = self.reveal_text(&title).unwrap_or(title);
                Ok(SessionSummary {
                    id,
                    created_at: format_ts(created_at),
                    updated_at: format_ts(updated_at),
                    summary: if title.is_empty() { None } else { Some(title) },
                })
            })
            .optional()
            .map_err(|e| e.to_string())?;
        Ok(session)
    }

    pub fn append_message(
        &self,
        project_key: &str,
        session_id: &str,
        role: &str,
        content: &str,
    ) -> Result<(), String> {
        let conn = self.connect()?;
        let now = now_ts();
        let seq = self.next_seq(&conn, session_id)?;
        let stored_content = self.protect_text(content)?;
        conn.execute(
            "INSERT INTO events (id, session_id, project_key, kind, role, content, command, stdout, stderr, exit_code, created_at, seq)
             VALUES (?1, ?2, ?3, 'message', ?4, ?5, NULL, NULL, NULL, NULL, ?6, ?7)",
            params![next_id(), session_id, project_key, role, stored_content, now, seq],
        )
        .map_err(|e| e.to_string())?;
        if role == "user" {
            let title = self.protect_text(&shorten(content, 72))?;
            conn.execute(
                "UPDATE sessions SET updated_at = ?2, title = COALESCE(NULLIF(title, ''), ?3) WHERE id = ?1",
                params![session_id, now, title],
            )
            .map_err(|e| e.to_string())?;
        } else {
            conn.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![session_id, now],
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn append_command(
        &self,
        project_key: &str,
        session_id: &str,
        command: &[String],
        status: &str,
        exit_code: Option<i64>,
        stdout: &str,
        stderr: &str,
    ) -> Result<(), String> {
        let conn = self.connect()?;
        let now = now_ts();
        let seq = self.next_seq(&conn, session_id)?;
        let stored_status = self.protect_text(status)?;
        let stored_command = self.protect_text(&command.join(" "))?;
        let stored_stdout = self.protect_text(stdout)?;
        let stored_stderr = self.protect_text(stderr)?;
        conn.execute(
            "INSERT INTO events (id, session_id, project_key, kind, role, content, command, stdout, stderr, exit_code, created_at, seq)
             VALUES (?1, ?2, ?3, 'command', NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                next_id(),
                session_id,
                project_key,
                stored_status,
                stored_command,
                stored_stdout,
                stored_stderr,
                exit_code,
                now,
                seq,
            ],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![session_id, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn append_checkpoint(
        &self,
        project_key: &str,
        session_id: &str,
        phase: &str,
        content: &str,
    ) -> Result<(), String> {
        let conn = self.connect()?;
        let now = now_ts();
        let seq = self.next_seq(&conn, session_id)?;
        let stored_phase = self.protect_text(phase)?;
        let stored_content = self.protect_text(&redact_secrets(content))?;
        conn.execute(
            "INSERT INTO events (id, session_id, project_key, kind, role, content, command, stdout, stderr, exit_code, created_at, seq)
             VALUES (?1, ?2, ?3, 'checkpoint', NULL, ?4, ?5, NULL, NULL, NULL, ?6, ?7)",
            params![
                next_id(),
                session_id,
                project_key,
                stored_content,
                stored_phase,
                now,
                seq,
            ],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
            params![session_id, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn read_messages(
        &self,
        project_key: &str,
        session_id: &str,
    ) -> Result<Vec<SessionEvent>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT role, content FROM events WHERE session_id = ?1 AND project_key = ?2 AND kind = 'message' ORDER BY created_at ASC, seq ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![session_id, project_key], |row| {
                let content = row.get::<_, Option<String>>(1)?.unwrap_or_default();
                Ok(SessionEvent {
                    role: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    content: self.reveal_text(&content).unwrap_or(content),
                })
            })
            .map_err(|e| e.to_string())?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|e| e.to_string())?);
        }
        Ok(events)
    }

    pub fn read_timeline(
        &self,
        project_key: &str,
        session_id: &str,
    ) -> Result<Vec<TimelineEvent>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT kind, role, content, command, stdout, stderr, exit_code, created_at
                 FROM events WHERE session_id = ?1 AND project_key = ?2 ORDER BY created_at ASC, seq ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![session_id, project_key], |row| {
                let created_at: i64 = row.get(7)?;
                let content = row.get::<_, Option<String>>(2)?;
                let command = row.get::<_, Option<String>>(3)?;
                let stdout = row.get::<_, Option<String>>(4)?;
                let stderr = row.get::<_, Option<String>>(5)?;
                Ok(TimelineEvent {
                    kind: row.get::<_, String>(0)?,
                    role: row.get::<_, Option<String>>(1)?,
                    content: self.reveal_optional(content),
                    command: self.reveal_optional(command),
                    stdout: self.reveal_optional(stdout),
                    stderr: self.reveal_optional(stderr),
                    exit_code: row.get::<_, Option<i64>>(6)?,
                    created_at: format_ts(created_at),
                })
            })
            .map_err(|e| e.to_string())?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|e| e.to_string())?);
        }
        Ok(events)
    }

    pub fn read_timeline_tail(
        &self,
        project_key: &str,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<TimelineEvent>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT kind, role, content, command, stdout, stderr, exit_code, created_at
                 FROM (
                    SELECT kind, role, content, command, stdout, stderr, exit_code, created_at, seq
                    FROM events
                    WHERE session_id = ?1 AND project_key = ?2
                    ORDER BY seq DESC
                    LIMIT ?3
                 )
                 ORDER BY seq ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![session_id, project_key, limit as i64], |row| {
                let created_at: i64 = row.get(7)?;
                let content = row.get::<_, Option<String>>(2)?;
                let command = row.get::<_, Option<String>>(3)?;
                let stdout = row.get::<_, Option<String>>(4)?;
                let stderr = row.get::<_, Option<String>>(5)?;
                Ok(TimelineEvent {
                    kind: row.get::<_, String>(0)?,
                    role: row.get::<_, Option<String>>(1)?,
                    content: self.reveal_optional(content),
                    command: self.reveal_optional(command),
                    stdout: self.reveal_optional(stdout),
                    stderr: self.reveal_optional(stderr),
                    exit_code: row.get::<_, Option<i64>>(6)?,
                    created_at: format_ts(created_at),
                })
            })
            .map_err(|e| e.to_string())?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|e| e.to_string())?);
        }
        Ok(events)
    }

    pub fn remember_session_memory(
        &self,
        project_key: &str,
        session_id: &str,
        kind: &str,
        content: &str,
        tags: &[String],
    ) -> Result<SessionMemoryRecord, String> {
        let conn = self.connect()?;
        let id = next_id();
        let now = now_ts();
        let tags = normalize_tags(tags);
        let content = redact_secrets(content);
        let stored_content = self.protect_text(&content)?;
        conn.execute(
            "INSERT INTO session_memory (id, session_id, project_key, kind, content, tags, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, session_id, project_key, kind, stored_content, tags, now, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(SessionMemoryRecord {
            id,
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            content,
            tags,
            created_at: format_ts(now),
        })
    }

    pub fn replace_session_memory(
        &self,
        project_key: &str,
        session_id: &str,
        kind: &str,
        content: &str,
        tags: &[String],
    ) -> Result<SessionMemoryRecord, String> {
        let conn = self.connect()?;
        conn.execute(
            "DELETE FROM session_memory WHERE project_key = ?1 AND session_id = ?2 AND kind = ?3",
            params![project_key, session_id, kind],
        )
        .map_err(|e| e.to_string())?;
        let id = next_id();
        let now = now_ts();
        let tags = normalize_tags(tags);
        let content = redact_secrets(content);
        let stored_content = self.protect_text(&content)?;
        conn.execute(
            "INSERT INTO session_memory (id, session_id, project_key, kind, content, tags, created_at, last_used_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![id, session_id, project_key, kind, stored_content, tags, now, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(SessionMemoryRecord {
            id,
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            content,
            tags,
            created_at: format_ts(now),
        })
    }

    pub fn recall_session_memory(
        &self,
        project_key: &str,
        session_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionMemoryRecord>, String> {
        let conn = self.connect()?;
        let normalized = normalize_query(query);
        let mut rows_out = if normalized.is_empty() {
            self.recent_session_memory(&conn, project_key, session_id, limit)?
        } else {
            let candidate_limit = limit.max(50).min(500);
            let candidates =
                self.recent_session_memory(&conn, project_key, session_id, candidate_limit)?;
            let collected = candidates
                .iter()
                .filter(|item| session_memory_matches(item, &normalized))
                .take(limit)
                .cloned()
                .collect::<Vec<_>>();
            if collected.is_empty() {
                candidates.into_iter().take(limit).collect()
            } else {
                collected
            }
        };

        for item in &rows_out {
            self.touch_session_memory(&conn, &item.id)?;
        }

        Ok(std::mem::take(&mut rows_out))
    }

    fn recent_session_memory(
        &self,
        conn: &Connection,
        project_key: &str,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<SessionMemoryRecord>, String> {
        let mut stmt = conn
            .prepare(
                "SELECT id, session_id, kind, content, tags, created_at
                 FROM session_memory
                 WHERE project_key = ?1 AND session_id = ?2
                 ORDER BY last_used_at DESC, created_at DESC
                 LIMIT ?3",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![project_key, session_id, limit as i64], |row| {
                let created_at: i64 = row.get(5)?;
                let content: String = row.get(3)?;
                Ok(SessionMemoryRecord {
                    id: row.get(0)?,
                    session_id: row.get(1)?,
                    kind: row.get(2)?,
                    content: self.reveal_text(&content).unwrap_or(content),
                    tags: row.get(4)?,
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

    fn touch_session_memory(&self, conn: &Connection, memory_id: &str) -> Result<(), String> {
        conn.execute(
            "UPDATE session_memory SET last_used_at = ?2 WHERE id = ?1",
            params![memory_id, now_ts()],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    fn next_seq(&self, conn: &Connection, session_id: &str) -> Result<i64, String> {
        conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())
    }

    fn protect_text(&self, value: &str) -> Result<String, String> {
        protect_secret(&self.secret_key_file, value)
    }

    fn reveal_text(&self, value: &str) -> Result<String, String> {
        reveal_secret(&self.secret_key_file, value)
    }

    fn reveal_optional(&self, value: Option<String>) -> Option<String> {
        value.map(|stored| self.reveal_text(&stored).unwrap_or(stored))
    }

    fn ensure_project(
        &self,
        conn: &Connection,
        project_key: &str,
        root_path: &str,
        now: i64,
    ) -> Result<(), String> {
        conn.execute(
            "INSERT INTO projects (key, root_path, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(key) DO UPDATE SET root_path = excluded.root_path, updated_at = excluded.updated_at",
            params![project_key, root_path, now, now],
        )
        .map_err(|e| e.to_string())?;
        Ok(())
    }
}

impl SessionStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        Ok(Self {
            history: HistoryStore::new(paths)?,
        })
    }

    pub fn create(
        &mut self,
        project_key: &str,
        root_path: &str,
        prompt: &str,
    ) -> Result<SessionSummary, String> {
        self.history.create_session(project_key, root_path, prompt)
    }

    pub fn append_event(
        &mut self,
        project_key: &str,
        session_id: &str,
        event: SessionEvent,
    ) -> Result<(), String> {
        self.history
            .append_message(project_key, session_id, &event.role, &event.content)
    }

    pub fn append_command(
        &mut self,
        project_key: &str,
        session_id: &str,
        command: &[String],
        status: &str,
        exit_code: Option<i64>,
        stdout: &str,
        stderr: &str,
    ) -> Result<(), String> {
        self.history.append_command(
            project_key,
            session_id,
            command,
            status,
            exit_code,
            stdout,
            stderr,
        )
    }

    pub fn append_checkpoint(
        &mut self,
        project_key: &str,
        session_id: &str,
        phase: &str,
        content: &str,
    ) -> Result<(), String> {
        self.history
            .append_checkpoint(project_key, session_id, phase, content)
    }

    pub fn remember_session_memory(
        &mut self,
        project_key: &str,
        session_id: &str,
        kind: &str,
        content: &str,
        tags: &[String],
    ) -> Result<SessionMemoryRecord, String> {
        self.history
            .remember_session_memory(project_key, session_id, kind, content, tags)
    }

    pub fn replace_session_memory(
        &mut self,
        project_key: &str,
        session_id: &str,
        kind: &str,
        content: &str,
        tags: &[String],
    ) -> Result<SessionMemoryRecord, String> {
        self.history
            .replace_session_memory(project_key, session_id, kind, content, tags)
    }

    pub fn recall_session_memory(
        &self,
        project_key: &str,
        session_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SessionMemoryRecord>, String> {
        self.history
            .recall_session_memory(project_key, session_id, query, limit)
    }

    pub fn read_events(
        &self,
        project_key: &str,
        session_id: &str,
    ) -> Result<Vec<SessionEvent>, String> {
        self.history.read_messages(project_key, session_id)
    }

    pub fn timeline(
        &self,
        project_key: &str,
        session_id: &str,
    ) -> Result<Vec<TimelineEvent>, String> {
        self.history.read_timeline(project_key, session_id)
    }

    pub fn timeline_tail(
        &self,
        project_key: &str,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<TimelineEvent>, String> {
        self.history
            .read_timeline_tail(project_key, session_id, limit)
    }

    pub fn list(&self, project_key: &str) -> Result<Vec<SessionSummary>, String> {
        self.history.list_sessions(project_key)
    }

    pub fn resolve(
        &self,
        project_key: &str,
        session_id: Option<String>,
    ) -> Result<SessionSummary, String> {
        match session_id {
            Some(id) => self
                .history
                .load_session(project_key, &id)?
                .ok_or_else(|| "no sessions found".to_string()),
            None => self
                .history
                .list_sessions(project_key)?
                .into_iter()
                .next()
                .ok_or_else(|| "no sessions found".to_string()),
        }
    }
}

fn shorten(value: &str, max: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let mut out = String::new();
    for ch in trimmed.chars().take(max.saturating_sub(1)) {
        out.push(ch);
    }
    out.push('…');
    out
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

fn session_memory_matches(record: &SessionMemoryRecord, normalized_query: &str) -> bool {
    let haystack = normalize_query(&format!(
        "{} {} {}",
        record.kind, record.content, record.tags
    ));
    normalized_query
        .split_whitespace()
        .all(|token| haystack.contains(token))
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| e.to_string())?;
    for row in rows {
        if row.map_err(|e| e.to_string())? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{shorten, SessionEvent, SessionStore};
    use crate::config::AppPaths;
    use crate::id::next_id;
    use std::fs;

    #[test]
    fn stores_session_payloads_encrypted_and_reads_plaintext() {
        let paths = test_paths();
        let mut store = SessionStore::new(&paths).unwrap();
        let secret_prompt = "analyze token wai_session_secret_value";
        let session = store
            .create(
                &paths.project_key,
                &paths.root_dir.display().to_string(),
                secret_prompt,
            )
            .unwrap();
        store
            .append_event(
                &paths.project_key,
                &session.id,
                SessionEvent::user(secret_prompt.to_string()),
            )
            .unwrap();
        store
            .append_command(
                &paths.project_key,
                &session.id,
                &["shell".to_string(), "status".to_string()],
                "ok",
                Some(0),
                "stdout has wai_stdout_secret_value",
                "",
            )
            .unwrap();

        let raw = fs::read(&paths.history_db).unwrap();
        assert!(!bytes_contains(&raw, b"wai_session_secret_value"));
        assert!(!bytes_contains(&raw, b"wai_stdout_secret_value"));

        let messages = store.read_events(&paths.project_key, &session.id).unwrap();
        assert!(messages
            .iter()
            .any(|event| event.content.contains("wai_session_secret_value")));
        let timeline = store.timeline(&paths.project_key, &session.id).unwrap();
        assert!(timeline.iter().any(|event| event
            .stdout
            .as_deref()
            .unwrap_or_default()
            .contains("wai_stdout_secret_value")));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn migrates_existing_plaintext_history_payloads() {
        let paths = test_paths();
        let mut store = SessionStore::new(&paths).unwrap();
        let session = store
            .create(
                &paths.project_key,
                &paths.root_dir.display().to_string(),
                "seed",
            )
            .unwrap();
        store
            .append_event(
                &paths.project_key,
                &session.id,
                SessionEvent::user("seed event".to_string()),
            )
            .unwrap();
        store
            .append_command(
                &paths.project_key,
                &session.id,
                &["shell".to_string(), "status".to_string()],
                "ok",
                Some(0),
                "seed stdout",
                "seed stderr",
            )
            .unwrap();
        store
            .remember_session_memory(
                &paths.project_key,
                &session.id,
                "preference",
                "seed memory",
                &["security".to_string()],
            )
            .unwrap();
        drop(store);

        let conn = rusqlite::Connection::open(&paths.history_db).unwrap();
        conn.execute(
            "UPDATE sessions SET title = ?1 WHERE id = ?2",
            rusqlite::params!["legacy title secret", session.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE events SET content = ?1 WHERE session_id = ?2 AND kind = 'message'",
            rusqlite::params!["legacy message secret", session.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE events SET content = ?1, command = ?2, stdout = ?3, stderr = ?4 WHERE session_id = ?5 AND kind = 'command'",
            rusqlite::params![
                "legacy command status",
                "legacy command secret",
                "legacy stdout secret",
                "legacy stderr secret",
                session.id
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE session_memory SET content = ?1 WHERE session_id = ?2",
            rusqlite::params!["legacy memory secret", session.id],
        )
        .unwrap();
        drop(conn);

        let store = SessionStore::new(&paths).unwrap();
        let raw = history_storage_bytes(&paths);
        for secret in [
            b"legacy title secret".as_slice(),
            b"legacy message secret".as_slice(),
            b"legacy command secret".as_slice(),
            b"legacy stdout secret".as_slice(),
            b"legacy stderr secret".as_slice(),
            b"legacy memory secret".as_slice(),
        ] {
            assert!(!bytes_contains(&raw, secret));
        }

        let sessions = store.list(&paths.project_key).unwrap();
        assert_eq!(
            sessions.first().and_then(|item| item.summary.as_deref()),
            Some("legacy title secret")
        );
        let messages = store.read_events(&paths.project_key, &session.id).unwrap();
        assert!(messages
            .iter()
            .any(|event| event.content == "legacy message secret"));
        let timeline = store.timeline(&paths.project_key, &session.id).unwrap();
        assert!(timeline.iter().any(|event| event
            .command
            .as_deref()
            .unwrap_or_default()
            .contains("legacy command secret")));
        let memory = store
            .recall_session_memory(&paths.project_key, &session.id, "legacy memory", 5)
            .unwrap();
        assert!(memory
            .iter()
            .any(|item| item.content == "legacy memory secret"));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn shorten_keeps_short_text() {
        assert_eq!(shorten("hello", 12), "hello");
    }

    #[test]
    fn checkpoint_events_are_timeline_visible_and_encrypted() {
        let paths = test_paths();
        let mut store = SessionStore::new(&paths).unwrap();
        let session = store
            .create(
                &paths.project_key,
                &paths.root_dir.display().to_string(),
                "seed",
            )
            .unwrap();
        store
            .append_checkpoint(
                &paths.project_key,
                &session.id,
                "model_request",
                r#"{"state":"checkpoint_plaintext_marker"}"#,
            )
            .unwrap();

        let timeline = store.timeline(&paths.project_key, &session.id).unwrap();
        let checkpoint = timeline
            .iter()
            .find(|event| event.kind == "checkpoint")
            .expect("checkpoint event should be persisted");
        assert_eq!(checkpoint.command.as_deref(), Some("model_request"));
        assert!(checkpoint
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("checkpoint_plaintext_marker"));

        let raw = history_storage_bytes(&paths);
        assert!(!bytes_contains(&raw, b"checkpoint_plaintext_marker"));
        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn replace_session_memory_keeps_latest_operational_state() {
        let paths = test_paths();
        let mut store = SessionStore::new(&paths).unwrap();
        let session = store
            .create(
                &paths.project_key,
                &paths.root_dir.display().to_string(),
                "seed",
            )
            .unwrap();

        store
            .replace_session_memory(
                &paths.project_key,
                &session.id,
                "agent_state",
                "snapshot 1",
                &["agent_state".to_string()],
            )
            .unwrap();
        store
            .replace_session_memory(
                &paths.project_key,
                &session.id,
                "agent_state",
                "snapshot 2",
                &["agent_state".to_string()],
            )
            .unwrap();

        let memory = store
            .recall_session_memory(&paths.project_key, &session.id, "", 10)
            .unwrap();

        assert_eq!(
            memory
                .iter()
                .filter(|item| item.kind == "agent_state")
                .count(),
            1
        );
        assert!(memory
            .iter()
            .any(|item| item.kind == "agent_state" && item.content == "snapshot 2"));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn event_constructors_work() {
        let event = SessionEvent::user("ping".to_string());
        assert_eq!(event.role, "user");
        assert_eq!(event.content, "ping");

        let developer = SessionEvent::developer("rules".to_string());
        assert_eq!(developer.role, "developer");
        assert_eq!(developer.content, "rules");
    }

    fn bytes_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    fn history_storage_bytes(paths: &AppPaths) -> Vec<u8> {
        let mut bytes = Vec::new();
        for path in [
            paths.history_db.clone(),
            std::path::PathBuf::from(format!("{}-wal", paths.history_db.display())),
            std::path::PathBuf::from(format!("{}-shm", paths.history_db.display())),
        ] {
            if let Ok(mut chunk) = fs::read(path) {
                bytes.append(&mut chunk);
            }
        }
        bytes
    }

    fn test_paths() -> AppPaths {
        let root_dir = std::env::temp_dir().join(format!("wirecli-session-test-{}", next_id()));
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
