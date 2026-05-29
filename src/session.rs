use crate::config::AppPaths;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
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

pub struct SessionStore {
    sessions_dir: PathBuf,
}

impl SessionStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        fs::create_dir_all(&paths.sessions_dir).map_err(|e| e.to_string())?;
        Ok(Self {
            sessions_dir: paths.sessions_dir.clone(),
        })
    }

    pub fn create(&mut self, prompt: &str) -> Result<SessionSummary, String> {
        let id = new_session_id();
        let path = self.session_path(&id);
        let now = now_string();
        let summary = Some(shorten(prompt, 72));

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;

        writeln!(
            file,
            "session\t{}\t{}\t{}",
            id,
            now,
            summary.clone().unwrap_or_default()
        )
        .map_err(|e| e.to_string())?;

        Ok(SessionSummary {
            id,
            created_at: now.clone(),
            updated_at: now,
            summary,
        })
    }

    pub fn append_event(&mut self, session_id: &str, event: SessionEvent) -> Result<(), String> {
        let path = self.session_path(session_id);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;

        writeln!(
            file,
            "event\t{}\t{}",
            event.role,
            escape_tab_newline(&event.content)
        )
        .map_err(|e| e.to_string())
    }

    pub fn read_events(&self, session_id: &str) -> Result<Vec<SessionEvent>, String> {
        let path = self.session_path(session_id);
        let file = fs::File::open(path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);
        let mut events = Vec::new();

        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            let mut parts = line.splitn(3, '\t');
            match parts.next() {
                Some("event") => {
                    let role = parts.next().unwrap_or("").to_string();
                    let content = unescape_tab_newline(parts.next().unwrap_or(""));
                    events.push(SessionEvent { role, content });
                }
                _ => {}
            }
        }

        Ok(events)
    }

    pub fn list(&self) -> Result<Vec<SessionSummary>, String> {
        let mut sessions = Vec::new();
        for entry in fs::read_dir(&self.sessions_dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                let metadata = entry.metadata().map_err(|e| e.to_string())?;
                let updated_at = metadata
                    .modified()
                    .ok()
                    .and_then(system_time_to_string)
                    .unwrap_or_else(now_string);
                let mut summary = self.read_header(&path)?.unwrap_or(SessionSummary {
                    id: name.to_string(),
                    created_at: updated_at.clone(),
                    updated_at: updated_at.clone(),
                    summary: None,
                });
                summary.updated_at = updated_at;
                sessions.push(summary);
            }
        }

        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }

    pub fn resolve(&self, session_id: Option<String>) -> Result<SessionSummary, String> {
        match session_id {
            Some(id) => self.load_summary(&id),
            None => self
                .list()?
                .into_iter()
                .next()
                .ok_or_else(|| "no sessions found".to_string()),
        }
    }

    fn load_summary(&self, session_id: &str) -> Result<SessionSummary, String> {
        let path = self.session_path(session_id);
        let mut summary = self
            .read_header(&path)?
            .ok_or_else(|| "invalid session file".to_string())?;

        let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
        summary.updated_at = metadata
            .modified()
            .ok()
            .and_then(system_time_to_string)
            .unwrap_or_else(now_string);
        summary.id = session_id.to_string();
        Ok(summary)
    }

    fn read_header(&self, path: &PathBuf) -> Result<Option<SessionSummary>, String> {
        let file = fs::File::open(path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            let mut parts = line.splitn(4, '\t');
            if matches!(parts.next(), Some("session")) {
                let id = parts.next().unwrap_or("").to_string();
                let created_at = parts.next().unwrap_or("").to_string();
                let summary = parts.next().unwrap_or("").to_string();
                return Ok(Some(SessionSummary {
                    id,
                    created_at,
                    updated_at: String::new(),
                    summary: if summary.is_empty() {
                        None
                    } else {
                        Some(summary)
                    },
                }));
            }
        }
        Ok(None)
    }

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{session_id}.session"))
    }
}

fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}")
}

fn now_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    secs.to_string()
}

fn system_time_to_string(time: SystemTime) -> Option<String> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs().to_string())
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

fn escape_tab_newline(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
}

fn unescape_tab_newline(value: &str) -> String {
    let mut out = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{escape_tab_newline, shorten, unescape_tab_newline};

    #[test]
    fn shorten_keeps_short_text() {
        assert_eq!(shorten("hello", 12), "hello");
    }

    #[test]
    fn escape_roundtrip_preserves_tabs_and_newlines() {
        let original = "a\tb\nc\\d";
        let encoded = escape_tab_newline(original);
        assert_eq!(unescape_tab_newline(&encoded), original);
    }
}
