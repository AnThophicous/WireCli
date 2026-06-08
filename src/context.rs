use crate::config::{AppConfig, AppPaths};
use crate::flash_cache::FlashCacheMemory;
use crate::lab::{LabRecord, LabStore};
use crate::memory::{AnchorInput, AnchorRecord, AnchorStore};
use crate::memory_context::{MemoryContextStore, MemoryRecord};
use crate::models::{compact_number, estimated_tokens, ModelInfo};
use crate::responses_agent;
use crate::safekey::redact_secrets;
use crate::session::{SessionMemoryRecord, SessionStore};
use crate::wire_memory::WireMemoryBundle;

const FULL_PROMPT_SOFT_LIMIT: usize = 72_000;
const FULL_EVENT_LIMIT: usize = 36;
const RECENT_WINDOW: usize = 14;
const RECENT_WINDOW_COMPACT: usize = 8;
const REFERENCE_WINDOW: usize = 8;
const REFERENCE_WINDOW_COMPACT: usize = 6;
const REFERENCE_FOCUSED_ASSISTANT_LIMIT: usize = 48_000;
const REFERENCE_MESSAGE_LIMIT: usize = 4_000;
const REFERENCE_MESSAGE_LIMIT_COMPACT: usize = 2_000;
const RECENT_MESSAGE_LIMIT: usize = 1_600;
const RECENT_COMMAND_LIMIT: usize = 900;
pub const WIRE_GLOBAL_MEMORY_KEY: &str = "__wire_global__";

pub struct Loom {
    anchors: AnchorStore,
    memory_context: MemoryContextStore,
    lab: LabStore,
}

pub struct LoomBundle {
    pub rendered_prompt: String,
    pub anchors_used: Vec<AnchorRecord>,
    pub session_memory_used: Vec<SessionMemoryRecord>,
    pub memory_context_used: Vec<MemoryRecord>,
    pub lab_used: Vec<LabRecord>,
    pub flash_cache_context: Option<String>,
    pub session_summary: Option<String>,
    pub context_status: ContextWindowStatus,
    pub compacted: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ContextWindowStatus {
    pub context_window: Option<u64>,
    pub max_completion_tokens: Option<u64>,
    pub estimated_prompt_tokens: u64,
    pub reserved_completion_tokens: u64,
    pub remaining_tokens: Option<u64>,
    pub auto_compact_threshold_tokens: Option<u64>,
    pub near_limit: bool,
}

impl ContextWindowStatus {
    pub fn from_prompt(prompt: &str, model_info: Option<&ModelInfo>) -> Self {
        let estimated_prompt_tokens = estimated_tokens(prompt);
        let context_window = model_info.and_then(|model| model.context_window);
        let max_completion_tokens = model_info.and_then(|model| model.max_completion_tokens);
        let reserved_completion_tokens =
            reserved_completion_tokens(context_window, max_completion_tokens);
        let used = estimated_prompt_tokens.saturating_add(reserved_completion_tokens);
        let remaining_tokens = context_window.map(|window| window.saturating_sub(used));
        let auto_compact_threshold_tokens = context_window.map(|window| {
            let headroom = auto_compact_headroom(window, reserved_completion_tokens);
            window.saturating_sub(headroom)
        });
        let near_limit = match (context_window, auto_compact_threshold_tokens) {
            (Some(_), Some(threshold)) => used >= threshold,
            _ => prompt.len() > FULL_PROMPT_SOFT_LIMIT,
        };
        Self {
            context_window,
            max_completion_tokens,
            estimated_prompt_tokens,
            reserved_completion_tokens,
            remaining_tokens,
            auto_compact_threshold_tokens,
            near_limit,
        }
    }

    pub fn label(&self) -> String {
        match self.context_window {
            Some(window) => {
                let remaining = self.remaining_tokens.unwrap_or(0);
                let output = self
                    .max_completion_tokens
                    .map(compact_number)
                    .unwrap_or_else(|| "unknown".to_string());
                format!(
                    "ctx remaining {} / {} · prompt est {} · output max {}",
                    compact_number(remaining),
                    compact_number(window),
                    compact_number(self.estimated_prompt_tokens),
                    output
                )
            }
            None => format!(
                "ctx unknown · prompt est {}",
                compact_number(self.estimated_prompt_tokens)
            ),
        }
    }
}

impl Loom {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        Ok(Self {
            anchors: AnchorStore::new(paths)?,
            memory_context: MemoryContextStore::new(paths)?,
            lab: LabStore::new(paths)?,
        })
    }

    pub fn build(
        &self,
        paths: &AppPaths,
        config: &AppConfig,
        session_store: &SessionStore,
        session_id: &str,
        user_prompt: &str,
        model_info: Option<&ModelInfo>,
    ) -> Result<LoomBundle, String> {
        let project_key = &paths.project_key;
        let memories_enabled = config.memories_enabled();
        let afup_enabled = config.afup_enabled();
        let (session_events, session_summary, anchors, session_memory, memory_context, lab) =
            std::thread::scope(|scope| {
                let timeline = scope.spawn(|| session_store.timeline(project_key, session_id));
                let summary = scope.spawn(|| self.anchors.latest_summary(project_key, session_id));
                let anchors = scope.spawn(|| {
                    if memories_enabled {
                        self.anchors.recall(project_key, user_prompt, 6)
                    } else {
                        Ok(Vec::new())
                    }
                });
                let session_memory = scope.spawn(|| {
                    if memories_enabled {
                        session_store.recall_session_memory(project_key, session_id, user_prompt, 6)
                    } else {
                        Ok(Vec::new())
                    }
                });
                let memory_context = scope.spawn(|| {
                    if memories_enabled {
                        self.memory_context.recall(project_key, user_prompt, 6)
                    } else {
                        Ok(Vec::new())
                    }
                });
                let lab = scope.spawn(|| {
                    if afup_enabled {
                        self.lab.recall(project_key, user_prompt, 8)
                    } else {
                        Ok(Vec::new())
                    }
                });

                (
                    timeline
                        .join()
                        .unwrap_or_else(|_| Err("timeline worker panicked".to_string())),
                    summary
                        .join()
                        .unwrap_or_else(|_| Err("summary worker panicked".to_string())),
                    anchors
                        .join()
                        .unwrap_or_else(|_| Err("anchor worker panicked".to_string())),
                    session_memory
                        .join()
                        .unwrap_or_else(|_| Err("session memory worker panicked".to_string())),
                    memory_context
                        .join()
                        .unwrap_or_else(|_| Err("memory context worker panicked".to_string())),
                    lab.join()
                        .unwrap_or_else(|_| Err("lab worker panicked".to_string())),
                )
            });
        let session_events = session_events?;
        let session_summary = session_summary?;
        let anchors = anchors?;
        let mut session_memory = session_memory?;
        let mut memory_context = memory_context?;
        let mut lab = lab?;
        if memories_enabled {
            if let Ok(recent_session_memory) =
                session_store.recall_session_memory(project_key, session_id, "", 6)
            {
                prepend_unique_session_memory(&mut session_memory, recent_session_memory);
            }
            if let Ok(global_memory) =
                self.memory_context
                    .recall(WIRE_GLOBAL_MEMORY_KEY, user_prompt, 4)
            {
                extend_unique_memory_records(&mut memory_context, global_memory);
            }
            if afup_enabled {
                if let Ok(global_lab) = self.lab.recall(WIRE_GLOBAL_MEMORY_KEY, user_prompt, 6) {
                    extend_unique_lab_records(&mut lab, global_lab);
                }
            }
        }

        let flash_cache_context = if config.flash_cache_memory_enabled() {
            match FlashCacheMemory::new(&paths.root_dir, config.feature_context.fcm_max_entries) {
                Ok(cache) => {
                    let _ = cache.refresh_project_signals(&paths.root_dir);
                    cache
                        .render_recontextualization(user_prompt, if afup_enabled { 8 } else { 5 })
                        .ok()
                        .flatten()
                }
                Err(_) => None,
            }
        } else {
            None
        };

        let rendered_prompt = self.render_prompt(
            paths,
            config,
            user_prompt,
            &anchors,
            &session_memory,
            &memory_context,
            &lab,
            flash_cache_context.as_deref(),
            session_summary
                .as_ref()
                .map(|record| record.content.as_str()),
            &session_events,
            false,
        );
        let initial_context_status = ContextWindowStatus::from_prompt(&rendered_prompt, model_info);
        let needs_compaction = config.auto_context_compaction_enabled()
            && self.needs_compaction(&rendered_prompt, &session_events, &initial_context_status);
        let (rendered_prompt, compacted, context_status) = if needs_compaction {
            let compact = self.render_prompt(
                paths,
                config,
                user_prompt,
                &anchors,
                &session_memory,
                &memory_context,
                &lab,
                flash_cache_context.as_deref(),
                session_summary
                    .as_ref()
                    .map(|record| record.content.as_str()),
                &session_events,
                true,
            );
            if compact.len() < rendered_prompt.len() {
                let status = ContextWindowStatus::from_prompt(&compact, model_info);
                (compact, true, status)
            } else {
                (rendered_prompt, false, initial_context_status)
            }
        } else {
            (rendered_prompt, false, initial_context_status)
        };

        Ok(LoomBundle {
            rendered_prompt,
            anchors_used: anchors,
            session_memory_used: session_memory,
            memory_context_used: memory_context,
            lab_used: lab,
            flash_cache_context,
            session_summary: session_summary.map(|record| record.content),
            context_status,
            compacted,
        })
    }

    pub async fn maybe_refresh_summary(
        &self,
        paths: &AppPaths,
        config: &AppConfig,
        session_store: &SessionStore,
        session_id: &str,
        status: &ContextWindowStatus,
    ) -> Result<Option<String>, String> {
        let project_key = &paths.project_key;
        let timeline = session_store.timeline(project_key, session_id)?;
        if timeline.len() < 12 && !status.near_limit {
            return Ok(None);
        }

        let transcript = render_timeline(&timeline);
        let instruction = format!(
            "You are Wire CLI Auto Compact Context.\n\
             Produce a very detailed but compact context summary for a coding agent that must continue this exact session without losing references.\n\
             Preserve: user language preference, current objective, every important user request, assistant decisions, unresolved references, architecture analyses, implementation status, validation status, changed files, commands, failures, security constraints, MCP/skill details, and next steps.\n\
             Keep chronological order where it matters, but group repeated tool noise.\n\
             Explicitly include earlier long assistant messages in condensed form instead of discarding them.\n\
             Mark facts as implemented, validated, unvalidated, or pending when possible.\n\
             Redact credentials, API keys, bearer tokens, passwords, sk-like values, cookies, and private secrets.\n\
             Return plain text only, no markdown table.\n\
             Context budget before compaction: {}.\n\
             \n\
             Transcript:\n{transcript}\n"
            ,
            status.label()
        );

        let summary =
            responses_agent::complete_text_with_model(config, config.acc_model(), instruction)
                .await?;
        let record = self.anchors.remember(
            project_key,
            AnchorInput {
                kind: "session_summary".to_string(),
                content: summary.clone(),
                tags: vec![
                    "session".to_string(),
                    "summary".to_string(),
                    "auto_compact".to_string(),
                ],
                importance: 0.85,
                confidence: 0.7,
                source_session_id: Some(session_id.to_string()),
            },
        )?;
        self.anchors.touch(&record.id)?;
        let _ = self.memory_context.remember(
            project_key,
            "summary",
            &summary,
            &[
                "session".to_string(),
                "summary".to_string(),
                "auto_compact".to_string(),
            ],
            Some(session_id),
            "anchor",
            0.95,
        )?;
        if config.flash_cache_memory_enabled() {
            if let Ok(cache) =
                FlashCacheMemory::new(&paths.root_dir, config.feature_context.fcm_max_entries)
            {
                let _ = cache.remember(
                    "acc_summary",
                    &summary,
                    &[
                        "acc".to_string(),
                        "summary".to_string(),
                        "session".to_string(),
                    ],
                    session_id,
                    0.95,
                );
            }
        }
        Ok(Some(summary))
    }

    fn render_prompt(
        &self,
        paths: &AppPaths,
        config: &AppConfig,
        user_prompt: &str,
        anchors: &[AnchorRecord],
        session_memory: &[SessionMemoryRecord],
        memory_context: &[MemoryRecord],
        lab: &[LabRecord],
        flash_cache_context: Option<&str>,
        session_summary: Option<&str>,
        session_events: &[crate::session::TimelineEvent],
        compact: bool,
    ) -> String {
        let mut out = String::new();
        out.push_str("Wire CLI Loom Context\n");
        out.push_str("==================\n\n");
        out.push_str("Project root:\n");
        out.push_str(&paths.root_dir.display().to_string());
        out.push_str("\n\n");
        out.push_str("Provider:\n");
        out.push_str(&config.provider);
        out.push_str("\nModel:\n");
        out.push_str(&config.model);
        out.push_str("\n\n");
        out.push_str("User language signal:\n");
        out.push_str(infer_user_language(user_prompt, session_events));
        out.push_str(
            "\nReply in that language unless the user explicitly asks for another language.\n\n",
        );

        if likely_message_reference(user_prompt) {
            out.push_str("Reference handling:\n");
            out.push_str(
                "The current user request appears to refer to an earlier message. Resolve phrases like \"aquela mensagem\", \"mensagem anterior\", \"that message\", or \"previous message\" against the conversation reference window before answering. If the referenced assistant message is available, act on it directly instead of saying it is already in the requested language.\n\n",
            );
        }

        if let Some(summary) = session_summary {
            out.push_str("Session summary:\n");
            out.push_str(summary);
            out.push_str("\n\n");
        }

        let wire_memory = WireMemoryBundle::load(&paths.root_dir);
        if let Some(rules) = wire_memory.render_relevant_rules(user_prompt) {
            out.push_str("Project WIRE memory rules:\n");
            out.push_str(&rules);
            out.push('\n');
        }

        if !anchors.is_empty() {
            out.push_str("Durable project memory:\n");
            for anchor in anchors.iter().take(if compact { 3 } else { 6 }) {
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

        if !session_memory.is_empty() {
            out.push_str("Session working memory:\n");
            for memory in session_memory.iter().take(if compact { 4 } else { 8 }) {
                out.push_str("- [");
                out.push_str(&memory.kind);
                out.push_str("] ");
                out.push_str(&memory.content);
                if !memory.tags.is_empty() {
                    out.push_str(" (tags: ");
                    out.push_str(&memory.tags);
                    out.push(')');
                }
                out.push('\n');
            }
            out.push('\n');
        }

        if !memory_context.is_empty() {
            out.push_str("Condensed memory context:\n");
            for memory in memory_context.iter().take(if compact { 4 } else { 6 }) {
                out.push_str("- [");
                out.push_str(&memory.kind);
                out.push_str("] ");
                out.push_str(&memory.content);
                if !memory.tags.is_empty() {
                    out.push_str(" (tags: ");
                    out.push_str(&memory.tags.join(", "));
                    out.push(')');
                }
                if !memory.paths.is_empty() {
                    out.push_str(" (paths: ");
                    out.push_str(&memory.paths.join(", "));
                    out.push(')');
                }
                if let Some(expires_at) = memory.expires_at {
                    out.push_str(" (expires_at: ");
                    out.push_str(&expires_at.to_string());
                    out.push(')');
                }
                out.push('\n');
            }
            out.push('\n');
        }

        if !lab.is_empty() {
            out.push_str("AFUP adaptation context:\n");
            out.push_str(
                "AFUP means Adaptive Framework for User Patterns. Use these learned preferences to adapt style, tooling, UI choices, security posture, and code organization. Do not treat AFUP as proof of external facts.\n",
            );
            for item in lab.iter().take(if compact { 4 } else { 8 }) {
                out.push_str("- [");
                out.push_str(&item.kind);
                out.push_str("] ");
                out.push_str(&item.content);
                if !item.tags.is_empty() {
                    out.push_str(" (tags: ");
                    out.push_str(&item.tags.join(", "));
                    out.push(')');
                }
                out.push('\n');
            }
            out.push('\n');
        }

        if let Some(flash_cache_context) = flash_cache_context {
            out.push_str("FCM project cache:\n");
            out.push_str(flash_cache_context);
            out.push('\n');
        }

        let references = render_message_reference_window(session_events, user_prompt, compact);
        if !references.is_empty() {
            out.push_str("Conversation reference window:\n");
            out.push_str(
                "Messages only. Use this for pronouns and deictic references before using summaries.\n",
            );
            out.push_str(&references);
            out.push_str("\n\n");
        }

        let recent = render_recent_window(
            session_events,
            if compact {
                RECENT_WINDOW_COMPACT
            } else {
                RECENT_WINDOW
            },
        );
        if !recent.is_empty() {
            out.push_str("Recent session window:\n");
            out.push_str("Operational events are shortened here; use the reference window for message content.\n");
            out.push_str(&recent);
            out.push_str("\n\n");
        }

        out.push_str("User request:\n");
        out.push_str(&redact_secrets(user_prompt));
        out.push('\n');
        redact_secrets(&out)
    }

    fn needs_compaction(
        &self,
        rendered_prompt: &str,
        session_events: &[crate::session::TimelineEvent],
        status: &ContextWindowStatus,
    ) -> bool {
        status.near_limit
            || rendered_prompt.len() > FULL_PROMPT_SOFT_LIMIT
            || session_events.len() > FULL_EVENT_LIMIT
    }
}

fn reserved_completion_tokens(
    context_window: Option<u64>,
    max_completion_tokens: Option<u64>,
) -> u64 {
    if let Some(max_completion_tokens) = max_completion_tokens {
        return max_completion_tokens.clamp(1_024, 16_384);
    }
    context_window
        .map(|window| (window / 8).clamp(2_048, 8_192))
        .unwrap_or(4_096)
}

fn auto_compact_headroom(context_window: u64, reserved_completion_tokens: u64) -> u64 {
    let ten_percent = context_window / 10;
    ten_percent
        .max(reserved_completion_tokens.saturating_mul(2))
        .max(8_192)
}

fn extend_unique_memory_records(target: &mut Vec<MemoryRecord>, incoming: Vec<MemoryRecord>) {
    for record in incoming {
        if target.iter().any(|item| {
            item.kind == record.kind && item.content.eq_ignore_ascii_case(&record.content)
        }) {
            continue;
        }
        target.push(record);
    }
}

fn prepend_unique_session_memory(
    target: &mut Vec<SessionMemoryRecord>,
    incoming: Vec<SessionMemoryRecord>,
) {
    let mut combined = Vec::new();
    for record in incoming {
        if !combined
            .iter()
            .any(|item: &SessionMemoryRecord| item.id == record.id)
        {
            combined.push(record);
        }
    }
    for record in std::mem::take(target) {
        if !combined
            .iter()
            .any(|item: &SessionMemoryRecord| item.id == record.id)
        {
            combined.push(record);
        }
    }
    *target = combined;
}

fn extend_unique_lab_records(target: &mut Vec<LabRecord>, incoming: Vec<LabRecord>) {
    for record in incoming {
        if target.iter().any(|item| {
            item.kind == record.kind && item.content.eq_ignore_ascii_case(&record.content)
        }) {
            continue;
        }
        target.push(record);
    }
}

fn render_recent_window(events: &[crate::session::TimelineEvent], limit: usize) -> String {
    let mut items = Vec::new();
    for event in events
        .iter()
        .rev()
        .filter(|event| !is_developer_message(event))
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
                redact_and_limit(
                    &event.content.clone().unwrap_or_default(),
                    RECENT_MESSAGE_LIMIT
                )
            ),
            "command" => format!(
                "[{}] command: {} | status={} | exit={:?}",
                event.created_at,
                event.command.clone().unwrap_or_default(),
                redact_and_limit(
                    &event.content.clone().unwrap_or_default(),
                    RECENT_COMMAND_LIMIT
                ),
                event.exit_code
            ),
            "checkpoint" => format!(
                "[{}] checkpoint: {} | {}",
                event.created_at,
                event.command.clone().unwrap_or_default(),
                redact_and_limit(
                    &event.content.clone().unwrap_or_default(),
                    RECENT_COMMAND_LIMIT
                )
            ),
            other => format!(
                "[{}] {}: {}",
                event.created_at,
                other,
                redact_and_limit(
                    &event.content.clone().unwrap_or_default(),
                    RECENT_COMMAND_LIMIT
                )
            ),
        };
        items.push(line);
    }
    items.join("\n")
}

fn render_message_reference_window(
    events: &[crate::session::TimelineEvent],
    user_prompt: &str,
    compact: bool,
) -> String {
    let wants_reference = likely_message_reference(user_prompt);
    let limit = if compact {
        REFERENCE_WINDOW_COMPACT
    } else {
        REFERENCE_WINDOW
    };
    let mut skipped_current_user = false;
    let mut messages = Vec::new();
    for event in events.iter().rev() {
        if event.kind != "message" || is_developer_message(event) {
            continue;
        }
        let role = event.role.as_deref().unwrap_or_default();
        let content = event.content.as_deref().unwrap_or_default();
        if !skipped_current_user && role == "user" && content.trim() == user_prompt.trim() {
            skipped_current_user = true;
            continue;
        }
        messages.push(event.clone());
        if messages.len() >= limit {
            break;
        }
    }
    if messages.is_empty() {
        return String::new();
    }
    messages.reverse();

    let focused_assistant = if wants_reference {
        messages
            .iter()
            .rposition(|event| event.role.as_deref() == Some("assistant"))
    } else {
        None
    };
    let normal_limit = if compact {
        REFERENCE_MESSAGE_LIMIT_COMPACT
    } else {
        REFERENCE_MESSAGE_LIMIT
    };

    let mut out = String::new();
    for (index, event) in messages.iter().enumerate() {
        let role = event.role.as_deref().unwrap_or("unknown");
        let content = event.content.as_deref().unwrap_or_default();
        let content_limit = if Some(index) == focused_assistant {
            REFERENCE_FOCUSED_ASSISTANT_LIMIT
        } else {
            normal_limit
        };
        out.push_str("[");
        out.push_str(&event.created_at);
        out.push_str("] ");
        out.push_str(role);
        out.push_str(":\n");
        out.push_str(&redact_and_limit(content, content_limit));
        out.push_str("\n\n");
    }
    out
}

fn render_timeline(events: &[crate::session::TimelineEvent]) -> String {
    let mut out = String::new();
    for event in events {
        let line = match event.kind.as_str() {
            "message" => format!(
                "[{}] {}: {}",
                event.created_at,
                event.role.clone().unwrap_or_else(|| "unknown".to_string()),
                redact_secrets(&event.content.clone().unwrap_or_default())
            ),
            "command" => format!(
                "[{}] command: {} | status={} | exit={:?}\nstdout:\n{}\nstderr:\n{}",
                event.created_at,
                event.command.clone().unwrap_or_default(),
                redact_secrets(&event.content.clone().unwrap_or_default()),
                event.exit_code,
                redact_secrets(&event.stdout.clone().unwrap_or_default()),
                redact_secrets(&event.stderr.clone().unwrap_or_default())
            ),
            "checkpoint" => format!(
                "[{}] checkpoint: {} | {}",
                event.created_at,
                event.command.clone().unwrap_or_default(),
                redact_secrets(&event.content.clone().unwrap_or_default())
            ),
            other => format!(
                "[{}] {}: {}",
                event.created_at,
                other,
                redact_secrets(&event.content.clone().unwrap_or_default())
            ),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn language_preference_note(user_prompt: &str) -> Option<String> {
    let lower = user_prompt.to_lowercase();
    if lower.contains("pt-br")
        || lower.contains("portugu")
        || lower.contains("falar em pt")
        || lower.contains("responde em pt")
    {
        return Some(
            "User prefers assistant replies in pt-BR for this session unless explicitly changed."
                .to_string(),
        );
    }
    if lower.contains("english") || lower.contains("ingles") || lower.contains("inglês") {
        return Some(
            "User prefers assistant replies in English for this session unless explicitly changed."
                .to_string(),
        );
    }
    None
}

fn infer_user_language(
    user_prompt: &str,
    events: &[crate::session::TimelineEvent],
) -> &'static str {
    if explicit_pt_br(user_prompt) || portuguese_score(user_prompt) >= 2 {
        return "pt-BR";
    }
    if explicit_english(user_prompt) {
        return "English";
    }

    for event in events.iter().rev().take(12) {
        if event.kind != "message" || event.role.as_deref() != Some("user") {
            continue;
        }
        let content = event.content.as_deref().unwrap_or_default();
        if explicit_pt_br(content) || portuguese_score(content) >= 2 {
            return "pt-BR";
        }
        if explicit_english(content) {
            return "English";
        }
    }

    "unknown; follow the current user's wording"
}

fn likely_message_reference(user_prompt: &str) -> bool {
    let lower = user_prompt.to_lowercase();
    [
        "aquela mensagem",
        "aql mensagem",
        "mensagem ali",
        "mensagem anterior",
        "ultima mensagem",
        "última mensagem",
        "que voce me enviou",
        "que você me enviou",
        "se me enviou",
        "bota ela em",
        "coloca ela em",
        "traduz",
        "traduza",
        "translate",
        "that message",
        "previous message",
        "last message",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn explicit_pt_br(value: &str) -> bool {
    let lower = value.to_lowercase();
    lower.contains("pt-br") || lower.contains("portugu")
}

fn explicit_english(value: &str) -> bool {
    let lower = value.to_lowercase();
    lower.contains("english") || lower.contains("ingles") || lower.contains("inglês")
}

fn portuguese_score(value: &str) -> usize {
    let lower = value.to_lowercase();
    [
        "voce", "você", "nao", "não", "pode", "falar", "mensagem", "bota", "coloca", "aquela",
        "aql", "ali", "melhora", "corrige", "erro", "lembrar", "tradu",
    ]
    .iter()
    .filter(|marker| lower.contains(*marker))
    .count()
}

fn is_developer_message(event: &crate::session::TimelineEvent) -> bool {
    event.kind == "message" && event.role.as_deref() == Some("developer")
}

fn redact_and_limit(value: &str, limit: usize) -> String {
    let redacted = redact_secrets(value);
    if redacted.chars().count() <= limit {
        return redacted;
    }
    let mut out = String::new();
    for ch in redacted.chars().take(limit.saturating_sub(32)) {
        out.push(ch);
    }
    out.push_str("\n[truncated]");
    out
}

#[cfg(test)]
mod tests {
    use super::{language_preference_note, render_message_reference_window};
    use crate::session::TimelineEvent;

    fn message(role: &str, content: &str, seq: usize) -> TimelineEvent {
        TimelineEvent {
            kind: "message".to_string(),
            role: Some(role.to_string()),
            content: Some(content.to_string()),
            command: None,
            stdout: None,
            stderr: None,
            exit_code: None,
            created_at: seq.to_string(),
        }
    }

    #[test]
    fn detects_pt_br_preference_request() {
        let note = language_preference_note("pode falar em pt-br?")
            .expect("expected language preference note");
        assert!(note.contains("pt-BR"));
    }

    #[test]
    fn reference_window_keeps_prior_assistant_message_for_translation() {
        let current = "aql mensagem ali que se me enviou bota ela em pt-br";
        let long_english = "Architecture analysis in English. ".repeat(300);
        let events = vec![
            message("user", "analyze the architecture", 1),
            message("assistant", &long_english, 2),
            message("user", "pode falar em pt-br?", 3),
            message("assistant", "Claro, posso falar em pt-BR.", 4),
            message("user", current, 5),
        ];

        let rendered = render_message_reference_window(&events, current, false);
        assert!(rendered.contains("Architecture analysis in English"));
        assert!(!rendered.contains(current));
    }
}
