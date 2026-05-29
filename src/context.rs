use crate::config::{AppConfig, AppPaths};
use crate::memory::{AnchorInput, AnchorRecord, AnchorStore};
use crate::responses_agent;
use crate::session::SessionStore;

pub struct Loom {
    anchors: AnchorStore,
}

pub struct LoomBundle {
    pub rendered_prompt: String,
    pub anchors_used: Vec<AnchorRecord>,
    pub session_summary: Option<String>,
}

impl Loom {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        Ok(Self {
            anchors: AnchorStore::new(paths)?,
        })
    }

    pub fn build(
        &self,
        paths: &AppPaths,
        config: &AppConfig,
        session_store: &SessionStore,
        session_id: &str,
        user_prompt: &str,
    ) -> Result<LoomBundle, String> {
        let project_key = &paths.project_key;
        let session_events = session_store.timeline(project_key, session_id)?;
        let session_summary = self.anchors.latest_summary(project_key, session_id)?;
        let anchors = self.anchors.recall(project_key, user_prompt, 6)?;

        let rendered_prompt = self.render_prompt(
            paths,
            config,
            user_prompt,
            &anchors,
            session_summary
                .as_ref()
                .map(|record| record.content.as_str()),
            &session_events,
        );

        Ok(LoomBundle {
            rendered_prompt,
            anchors_used: anchors,
            session_summary: session_summary.map(|record| record.content),
        })
    }

    pub async fn maybe_refresh_summary(
        &self,
        paths: &AppPaths,
        config: &AppConfig,
        session_store: &SessionStore,
        session_id: &str,
    ) -> Result<Option<String>, String> {
        let project_key = &paths.project_key;
        let timeline = session_store.timeline(project_key, session_id)?;
        if timeline.len() < 12 {
            return Ok(None);
        }

        let transcript = render_timeline(&timeline);
        let instruction = format!(
            "You are summarizing a coding session for a local agent runtime.\n\
             Produce a deep, structured summary in plain text.\n\
             Keep stable facts, decisions, open tasks, important files, commands, failures, and next steps.\n\
             Remove filler and repetition.\n\
             This summary will be used as long-lived memory for later sessions.\n\
             \n\
             Transcript:\n{transcript}\n"
        );

        let summary = responses_agent::complete_text(config, instruction).await?;
        let record = self.anchors.remember(
            project_key,
            AnchorInput {
                kind: "session_summary".to_string(),
                content: summary.clone(),
                tags: vec!["session".to_string(), "summary".to_string()],
                importance: 0.85,
                confidence: 0.7,
                source_session_id: Some(session_id.to_string()),
            },
        )?;
        self.anchors.touch(&record.id)?;
        Ok(Some(summary))
    }

    fn render_prompt(
        &self,
        paths: &AppPaths,
        config: &AppConfig,
        user_prompt: &str,
        anchors: &[AnchorRecord],
        session_summary: Option<&str>,
        session_events: &[crate::session::TimelineEvent],
    ) -> String {
        let mut out = String::new();
        out.push_str("Rift Loom Context\n");
        out.push_str("==================\n\n");
        out.push_str("Project root:\n");
        out.push_str(&paths.root_dir.display().to_string());
        out.push_str("\n\n");
        out.push_str("Provider:\n");
        out.push_str(&config.provider);
        out.push_str("\nModel:\n");
        out.push_str(&config.model);
        out.push_str("\n\n");

        if let Some(summary) = session_summary {
            out.push_str("Session summary:\n");
            out.push_str(summary);
            out.push_str("\n\n");
        }

        if !anchors.is_empty() {
            out.push_str("Anchor memory:\n");
            for anchor in anchors {
                out.push_str("- [");
                out.push_str(&anchor.kind);
                out.push_str("] ");
                out.push_str(&anchor.content);
                if !anchor.tags.is_empty() {
                    out.push_str(" (tags: ");
                    out.push_str(&anchor.tags);
                    out.push(')');
                }
                out.push('\n');
            }
            out.push('\n');
        }

        let recent = render_recent_window(session_events, 10);
        if !recent.is_empty() {
            out.push_str("Recent session window:\n");
            out.push_str(&recent);
            out.push_str("\n\n");
        }

        out.push_str("User request:\n");
        out.push_str(user_prompt);
        out.push('\n');
        out
    }
}

fn render_recent_window(events: &[crate::session::TimelineEvent], limit: usize) -> String {
    let mut items = Vec::new();
    for event in events
        .iter()
        .rev()
        .take(limit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let line = match event.kind.as_str() {
            "message" => format!(
                "[{}] {}: {}",
                event.created_at,
                event.role.clone().unwrap_or_else(|| "unknown".to_string()),
                event.content.clone().unwrap_or_default()
            ),
            "command" => format!(
                "[{}] command: {} | status={} | exit={:?}",
                event.created_at,
                event.command.clone().unwrap_or_default(),
                event.content.clone().unwrap_or_default(),
                event.exit_code
            ),
            other => format!(
                "[{}] {}: {}",
                event.created_at,
                other,
                event.content.clone().unwrap_or_default()
            ),
        };
        items.push(line);
    }
    items.join("\n")
}

fn render_timeline(events: &[crate::session::TimelineEvent]) -> String {
    let mut out = String::new();
    for event in events {
        let line = match event.kind.as_str() {
            "message" => format!(
                "[{}] {}: {}",
                event.created_at,
                event.role.clone().unwrap_or_else(|| "unknown".to_string()),
                event.content.clone().unwrap_or_default()
            ),
            "command" => format!(
                "[{}] command: {} | status={} | exit={:?}\nstdout:\n{}\nstderr:\n{}",
                event.created_at,
                event.command.clone().unwrap_or_default(),
                event.content.clone().unwrap_or_default(),
                event.exit_code,
                event.stdout.clone().unwrap_or_default(),
                event.stderr.clone().unwrap_or_default()
            ),
            other => format!(
                "[{}] {}: {}",
                event.created_at,
                other,
                event.content.clone().unwrap_or_default()
            ),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}
