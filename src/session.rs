use crate::config::AppPaths;
use crate::id::next_id;
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone)]
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

impl SessionEvent {
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
    project_key: String,
    root_path: String,
}

pub struct SessionStore {
    history: HistoryStore,
}

impl HistoryStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        Self::new_for_project(paths, &paths.project_key, &paths.root_dir.display().to_string())
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

            CREATE INDEX IF NOT EXISTS idx_events_session_seq
                ON events(session_id, seq);
            CREATE INDEX IF NOT EXISTS idx_sessions_updated_at
                ON sessions(project_key, updated_at DESC);
            CREATE INDEX IF NOT EXISTS idx_sessions_project_updated
                ON sessions(project_key, updated_at DESC, created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_events_project_session_seq
                ON events(project_key, session_id, seq);
            "#,
        )
        .map_err(|e| e.to_string())
        .and_then(|_| self.migrate_legacy_schema(&conn))
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

    pub fn create_session(&self, project_key: &str, root_path: &str, prompt: &str) -> Result<SessionSummary, String> {
        let conn = self.connect()?;
        let id = next_id();
        let now = now_ts();
        let title = shorten(prompt, 72);
        self.ensure_project(&conn, project_key, root_path, now)?;

        conn.execute(
            "INSERT INTO sessions (id, project_key, title, workspace, created_at, updated_at) VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
            params![id, project_key, title, now, now],
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
                "SELECT id, title, created_at, updated_at FROM sessions WHERE project_key = ?1 ORDER BY updated_at DESC, created_at DESC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![project_key], |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let created_at: i64 = row.get(2)?;
                let updated_at: i64 = row.get(3)?;
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

    pub fn load_session(&self, project_key: &str, session_id: &str) -> Result<Option<SessionSummary>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare("SELECT id, title, created_at, updated_at FROM sessions WHERE id = ?1 AND project_key = ?2")
            .map_err(|e| e.to_string())?;
        let session = stmt
            .query_row(params![session_id, project_key], |row| {
                let id: String = row.get(0)?;
                let title: String = row.get(1)?;
                let created_at: i64 = row.get(2)?;
                let updated_at: i64 = row.get(3)?;
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
        conn.execute(
            "INSERT INTO events (id, session_id, project_key, kind, role, content, command, stdout, stderr, exit_code, created_at, seq)
             VALUES (?1, ?2, ?3, 'message', ?4, ?5, NULL, NULL, NULL, NULL, ?6, ?7)",
            params![next_id(), session_id, project_key, role, content, now, seq],
        )
        .map_err(|e| e.to_string())?;
        conn.execute(
            "UPDATE sessions SET updated_at = ?2, title = COALESCE(NULLIF(title, ''), ?3) WHERE id = ?1",
            params![session_id, now, shorten(content, 72)],
        )
        .map_err(|e| e.to_string())?;
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
        conn.execute(
            "INSERT INTO events (id, session_id, project_key, kind, role, content, command, stdout, stderr, exit_code, created_at, seq)
             VALUES (?1, ?2, ?3, 'command', NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                next_id(),
                session_id,
                project_key,
                status,
                command.join(" "),
                stdout,
                stderr,
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

    pub fn read_messages(&self, project_key: &str, session_id: &str) -> Result<Vec<SessionEvent>, String> {
        let conn = self.connect()?;
        let mut stmt = conn
            .prepare(
                "SELECT role, content FROM events WHERE session_id = ?1 AND project_key = ?2 AND kind = 'message' ORDER BY created_at ASC, seq ASC",
            )
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map(params![session_id, project_key], |row| {
                Ok(SessionEvent {
                    role: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    content: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                })
            })
            .map_err(|e| e.to_string())?;

        let mut events = Vec::new();
        for row in rows {
            events.push(row.map_err(|e| e.to_string())?);
        }
        Ok(events)
    }

    pub fn read_timeline(&self, project_key: &str, session_id: &str) -> Result<Vec<TimelineEvent>, String> {
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
                Ok(TimelineEvent {
                    kind: row.get::<_, String>(0)?,
                    role: row.get::<_, Option<String>>(1)?,
                    content: row.get::<_, Option<String>>(2)?,
                    command: row.get::<_, Option<String>>(3)?,
                    stdout: row.get::<_, Option<String>>(4)?,
                    stderr: row.get::<_, Option<String>>(5)?,
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

    fn next_seq(&self, conn: &Connection, session_id: &str) -> Result<i64, String> {
        conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())
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

    pub fn create(&mut self, project_key: &str, root_path: &str, prompt: &str) -> Result<SessionSummary, String> {
        self.history.create_session(project_key, root_path, prompt)
    }

    pub fn append_event(&mut self, project_key: &str, session_id: &str, event: SessionEvent) -> Result<(), String> {
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
        self.history
            .append_command(project_key, session_id, command, status, exit_code, stdout, stderr)
    }

    pub fn read_events(&self, project_key: &str, session_id: &str) -> Result<Vec<SessionEvent>, String> {
        self.history.read_messages(project_key, session_id)
    }

    pub fn timeline(&self, project_key: &str, session_id: &str) -> Result<Vec<TimelineEvent>, String> {
        self.history.read_timeline(project_key, session_id)
    }

    pub fn list(&self, project_key: &str) -> Result<Vec<SessionSummary>, String> {
        self.history.list_sessions(project_key)
    }

    pub fn resolve(&self, project_key: &str, session_id: Option<String>) -> Result<SessionSummary, String> {
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
    use super::{shorten, SessionEvent};

    #[test]
    fn shorten_keeps_short_text() {
        assert_eq!(shorten("hello", 12), "hello");
    }

    #[test]
    fn event_constructors_work() {
        let event = SessionEvent::user("ping".to_string());
        assert_eq!(event.role, "user");
        assert_eq!(event.content, "ping");
    }
}
