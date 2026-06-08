mod terminal;

use crate::commands::parser::split_command_line;
use crate::config::{save_global_model_status, AppConfig, AppPaths, PermissionMode, ThemeConfig};
use crate::mcp::{McpRegistry, McpServerConfig, McpToolSpec};
use crate::model_catalog;
use crate::models::{compact_number, ModelInfo};
use crate::policy::CommandPolicy;
use crate::providers::{
    active_provider, apply_provider_preset, available_providers, provider_model_mismatch,
    provider_uses_openrouter_pkce, ProviderProfile, ProviderProtocol,
};
use crate::responses_agent::{self, AgentControl, PromptImage, PromptInput, TokenUsage};
use crate::safekey::redact_secrets;
use crate::session::{SessionStore, SessionSummary};
use crate::skills::SkillStore;
use crate::wire_auth::login_with_openrouter_progress;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use pulldown_cmark::{CodeBlockKind, Event as MdEvent, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Terminal;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use terminal::{init_terminal, restore_terminal};

const KEY_SCROLL_ROWS: i32 = 3;
const MOUSE_SCROLL_ROWS: i32 = 6;
const SELECTION_EDGE_SCROLL_ROWS: i32 = 2;

#[derive(Clone)]
struct ChatMessage {
    role: MessageRole,
    title: Option<String>,
    content: String,
}

#[derive(Clone)]
enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
    Queued,
    Shell,
}

enum UiEvent {
    SessionBound(String),
    Delta(String),
    ToolDelta {
        name: Option<String>,
        arguments_delta: String,
    },
    Status(String),
    ToolStart {
        name: String,
        summary: String,
    },
    ToolResult {
        name: String,
        output: String,
    },
    Usage(TokenUsage),
    Done {
        session_id: String,
        output: String,
    },
    Error(String),
}

enum LoginEvent {
    Status(String),
    Success {
        api_key: String,
        user_id: Option<String>,
        base_url: String,
        model: String,
    },
    Error(String),
}

enum ModelLoadEvent {
    Loaded(Vec<ModelInfo>),
    Failed(String),
}

struct McpLoadEvent {
    tools: Vec<McpToolSpec>,
    errors: Vec<String>,
}

#[derive(Clone)]
struct BackendHealth {
    alive: bool,
    message: String,
    checked_at: Instant,
}

enum Overlay {
    None,
    LoginGate {
        selected: usize,
        status: String,
    },
    ProviderPicker {
        selected: usize,
    },
    ModelCostWarning {
        selected: usize,
        model: String,
        warning: String,
        recommendation: String,
    },
    ModelPicker {
        selected: usize,
        scroll: usize,
        query: String,
    },
    FilePicker {
        query: String,
        directory: PathBuf,
        entries: Vec<FilePickerEntry>,
        selected: usize,
        scroll: usize,
    },
    McpPanel {
        servers: Vec<McpServerConfig>,
        tools: Vec<McpToolSpec>,
        errors: Vec<String>,
        loading: bool,
        scroll: usize,
    },
    PermissionPicker {
        selected: usize,
    },
}

#[derive(Clone)]
struct AttachedFile {
    path: PathBuf,
    label: String,
    kind: AttachmentKind,
}

#[derive(Clone)]
enum AttachmentKind {
    Text(String),
    Image(PromptImage),
}

#[derive(Clone)]
struct PastedImage {
    index: usize,
    label: String,
    path: PathBuf,
    prompt_image: PromptImage,
}

#[derive(Clone)]
struct PastedContent {
    index: usize,
    content: String,
}

struct Toast {
    title: String,
    body: String,
    created_at: Instant,
    ttl: Duration,
}

#[derive(Clone)]
struct FilePickerEntry {
    path: PathBuf,
    label: String,
    kind: FilePickerEntryKind,
    mention: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
enum FilePickerEntryKind {
    Parent,
    Directory,
    File,
    Skill,
    McpServer,
}

struct PromptPlan {
    display_prompt: String,
    model_prompt: String,
    images: Vec<PromptImage>,
}

struct QueuedPrompt {
    plan: PromptPlan,
}

struct FilePickerState {
    original_prompt: String,
    pinned: BTreeMap<String, PathBuf>,
}

pub fn run_tui(paths: AppPaths) -> Result<(), String> {
    let config = AppConfig::load(&paths)?;
    let theme = ThemeConfig::load_or_create(&paths.theme_file)?;
    let models = vec![fallback_model(&config.model)];
    let logo = load_logo(&paths);
    let startup_report = crate::startup::bootstrap(&paths);
    let startup_notice = startup_notice_from_report(&startup_report);

    let mut app = App::new(paths, config, models, logo, theme, startup_notice);
    app.run()
}

pub fn run_login_tui(paths: AppPaths) -> Result<(), String> {
    let config = AppConfig::load(&paths)?;
    let theme = ThemeConfig::load_or_create(&paths.theme_file)?;
    let models = vec![fallback_model(&config.model)];
    let logo = load_logo(&paths);
    let startup_report = crate::startup::bootstrap(&paths);
    let startup_notice = startup_notice_from_report(&startup_report);

    let mut app = App::new(paths, config, models, logo, theme, startup_notice);
    app.login_required = true;
    app.open_login_gate_overlay();
    app.run()
}

pub fn run_sessions_tui(paths: AppPaths) -> Result<(), String> {
    let config = AppConfig::load(&paths)?;
    let _loaded_theme = ThemeConfig::load_or_create(&paths.theme_file)?;
    let theme = ThemeConfig::default();
    let sessions = SessionStore::new(&paths)?.list(&paths.project_key)?;
    let mut picker = SessionPickerApp::new(paths.clone(), config.clone(), sessions, theme);
    if let Some(session) = picker.run()? {
        run_session_view_tui(paths, config, session.id)
    } else {
        Ok(())
    }
}

fn startup_notice_from_report(report: &crate::startup::StartupReport) -> Option<String> {
    report.notice().or_else(|| report.inventory_notice())
}

struct App {
    paths: AppPaths,
    config: AppConfig,
    login_required: bool,
    model_selection_required: bool,
    models: Vec<ModelInfo>,
    selected_model: usize,
    messages: Vec<ChatMessage>,
    input: String,
    cursor: usize,
    input_scroll: usize,
    status: String,
    running: bool,
    activity: String,
    should_quit: bool,
    rx: Option<mpsc::Receiver<UiEvent>>,
    login_rx: Option<mpsc::Receiver<LoginEvent>>,
    control: Option<AgentControl>,
    model_rx: Option<mpsc::Receiver<ModelLoadEvent>>,
    mcp_rx: Option<mpsc::Receiver<McpLoadEvent>>,
    models_loading: bool,
    force_model_picker_after_load: bool,
    health_rx: mpsc::Receiver<BackendHealth>,
    backend_health: BackendHealth,
    session_id: Option<String>,
    overlay: Overlay,
    logo: Vec<String>,
    theme: ThemeConfig,
    startup_notice: Option<String>,
    started_at: Instant,
    pending_prompt: Option<FilePickerState>,
    feed_scroll: usize,
    feed_max_scroll: usize,
    feed_area: Rect,
    follow_latest: bool,
    usage: TokenUsage,
    pasted_images: Vec<PastedImage>,
    pasted_contents: Vec<PastedContent>,
    queued_prompts: VecDeque<QueuedPrompt>,
    pending_cost_plan: Option<PromptPlan>,
    acknowledged_large_models: BTreeSet<String>,
    tool_summaries: BTreeMap<String, VecDeque<String>>,
    toasts: VecDeque<Toast>,
}

impl App {
    fn new(
        paths: AppPaths,
        config: AppConfig,
        models: Vec<ModelInfo>,
        logo: Vec<String>,
        theme: ThemeConfig,
        startup_notice: Option<String>,
    ) -> Self {
        let login_required = config.requires_login();
        let model_selection_required = !login_required && config.requires_model_selection();
        let health_rx = empty_health_rx();
        let model_rx = None;
        let models_loading = false;
        let backend_health = if login_required {
            BackendHealth {
                alive: false,
                message: "login required".to_string(),
                checked_at: Instant::now(),
            }
        } else {
            BackendHealth {
                alive: startup_notice.is_none(),
                message: startup_notice
                    .clone()
                    .unwrap_or_else(|| "ready".to_string()),
                checked_at: Instant::now(),
            }
        };
        let selected_model = models
            .iter()
            .position(|model| model.id == config.model)
            .unwrap_or(0);
        Self {
            paths,
            config,
            login_required,
            model_selection_required,
            models,
            selected_model,
            messages: Vec::new(),
            input: String::new(),
            cursor: 0,
            input_scroll: 0,
            status: "ready".to_string(),
            running: false,
            activity: String::new(),
            should_quit: false,
            rx: None,
            login_rx: None,
            control: None,
            model_rx,
            mcp_rx: None,
            models_loading,
            force_model_picker_after_load: model_selection_required,
            health_rx,
            backend_health,
            session_id: None,
            overlay: Overlay::None,
            logo,
            theme,
            startup_notice,
            started_at: Instant::now(),
            pending_prompt: None,
            feed_scroll: 0,
            feed_max_scroll: 0,
            feed_area: Rect::new(0, 0, 0, 0),
            follow_latest: true,
            usage: TokenUsage::default(),
            pasted_images: Vec::new(),
            pasted_contents: Vec::new(),
            queued_prompts: VecDeque::new(),
            pending_cost_plan: None,
            acknowledged_large_models: BTreeSet::new(),
            tool_summaries: BTreeMap::new(),
            toasts: VecDeque::new(),
        }
    }

    fn run(&mut self) -> Result<(), String> {
        self.maybe_open_login_gate_on_start();
        let mut terminal = init_terminal()?;
        let result = self.event_loop(&mut terminal);
        let restore_result = restore_terminal(&mut terminal);
        match (result, restore_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(err), Ok(())) => Err(err),
            (Ok(()), Err(err)) => Err(err),
            (Err(err), Err(_restore_err)) => Err(err),
        }
    }

    fn restore_latest_session(&mut self) {
        let Ok(store) = SessionStore::new(&self.paths) else {
            return;
        };
        let Ok(session) = store.resolve(&self.paths.project_key, None) else {
            return;
        };
        let Ok(timeline) = store.timeline_tail(&self.paths.project_key, &session.id, 160) else {
            return;
        };
        if timeline.is_empty() {
            return;
        }
        self.session_id = Some(session.id);
        self.status = session
            .summary
            .unwrap_or_else(|| "session restored".to_string());
        self.messages = timeline_to_chat_messages(&timeline);
        self.follow_latest = true;
        self.scroll_feed_to_bottom();
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        loop {
            self.drain_events();
            terminal
                .draw(|frame| self.draw(frame))
                .map_err(|e| e.to_string())?;

            if self.should_quit {
                break;
            }

            if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Key(key) => self.handle_key(key),
                    Event::Paste(data) => self.handle_paste(data),
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    Event::Resize(_, _) => {
                        if self.config.features.terminal_resize_reflow && self.follow_latest {
                            self.scroll_feed_to_bottom();
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.login_required {
            if overlay_has_modal_focus(&self.overlay) && self.handle_overlay_key(key) {
                return;
            }
            if self.handle_login_gate_key(key) {
                return;
            }
        }
        if self.handle_overlay_key(key) {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('k')) {
            return;
        }

        let mut changed_input = false;
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.copy_transcript_to_clipboard();
            }
            KeyCode::PageUp => self.scroll_feed(-MOUSE_SCROLL_ROWS),
            KeyCode::PageDown => self.scroll_feed(MOUSE_SCROLL_ROWS),
            KeyCode::Up if key.modifiers.is_empty() => self.scroll_feed(-KEY_SCROLL_ROWS),
            KeyCode::Down if key.modifiers.is_empty() => self.scroll_feed(KEY_SCROLL_ROWS),
            KeyCode::End if self.running => self.scroll_feed_to_bottom(),
            KeyCode::Esc if self.running => {
                self.interrupt_current_run_with_input();
            }
            KeyCode::Esc => {
                if self.input.is_empty() {
                    self.should_quit = true;
                } else {
                    self.clear_prompt_input();
                    changed_input = true;
                }
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_newline();
            }
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::Tab if !self.running => self.cycle_model(1),
            KeyCode::BackTab if !self.running => self.cycle_model(-1),
            KeyCode::Left => {
                self.cursor = prev_char_boundary(&self.input, self.cursor);
            }
            KeyCode::Right => {
                self.cursor = next_char_boundary(&self.input, self.cursor);
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => {
                self.cursor = self.input.len();
                self.scroll_feed_to_bottom();
            }
            KeyCode::Backspace => {
                if self.cursor > 0 && self.cursor <= self.input.len() {
                    let prev = prev_char_boundary(&self.input, self.cursor);
                    if prev < self.cursor && prev < self.input.len() {
                        self.input.drain(prev..self.cursor);
                        self.cursor = prev;
                        changed_input = true;
                    }
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    let next = next_char_boundary(&self.input, self.cursor);
                    if next > self.cursor {
                        self.input.drain(self.cursor..next);
                        changed_input = true;
                    }
                }
            }
            KeyCode::Char(ch) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.input.insert(self.cursor, ch);
                    self.cursor += ch.len_utf8();
                    changed_input = true;
                }
            }
            _ => {}
        }
        if changed_input {
            self.ensure_input_cursor_visible();
            self.sync_file_picker_overlay();
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollDown if self.mouse_in_prompt(mouse.row) => self.scroll_input(3),
            MouseEventKind::ScrollUp if self.mouse_in_prompt(mouse.row) => self.scroll_input(-3),
            MouseEventKind::ScrollDown => self.scroll_feed(MOUSE_SCROLL_ROWS),
            MouseEventKind::ScrollUp => self.scroll_feed(-MOUSE_SCROLL_ROWS),
            MouseEventKind::Drag(_) => self.scroll_feed_near_selection_edge(mouse.row),
            _ => {}
        }
    }

    fn scroll_feed_near_selection_edge(&mut self, row: u16) {
        if self.messages.is_empty() || self.feed_area.height == 0 {
            return;
        }
        let top = self.feed_area.y;
        let bottom = self
            .feed_area
            .y
            .saturating_add(self.feed_area.height.saturating_sub(1));
        if row <= top.saturating_add(1) {
            self.scroll_feed(-SELECTION_EDGE_SCROLL_ROWS);
        } else if row >= bottom.saturating_sub(1) {
            self.scroll_feed(SELECTION_EDGE_SCROLL_ROWS);
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> bool {
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::None => false,
            Overlay::ModelPicker {
                mut selected,
                mut scroll,
                mut query,
            } => {
                let mut choose: Option<String> = None;
                let mut close = false;
                let required = self.model_selection_required;
                let model_indexes = self.filtered_model_indexes(&query);
                match key.code {
                    KeyCode::Esc => {
                        if required {
                            self.status = "choose a model to continue".to_string();
                        } else {
                            close = true;
                        }
                    }
                    KeyCode::Enter => {
                        choose = model_indexes
                            .get(selected)
                            .and_then(|idx| self.models.get(*idx))
                            .map(|model| model.id.clone());
                    }
                    KeyCode::Up => {
                        if selected > 0 {
                            selected -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if selected + 1 < model_indexes.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::PageUp => {
                        selected = selected.saturating_sub(8);
                    }
                    KeyCode::PageDown => {
                        selected = (selected + 8).min(model_indexes.len().saturating_sub(1));
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => {
                        selected = model_indexes.len().saturating_sub(1);
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        selected = 0;
                        scroll = 0;
                    }
                    KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        query.push(ch);
                        selected = 0;
                        scroll = 0;
                    }
                    _ => {}
                }

                let visible = 8usize;
                if selected < scroll {
                    scroll = selected;
                } else if selected >= scroll.saturating_add(visible) {
                    scroll = selected.saturating_add(1).saturating_sub(visible);
                }

                if let Some(model) = choose {
                    if let Some(index) = self.models.iter().position(|m| m.id == model) {
                        self.selected_model = index;
                    }
                    self.config.model = model;
                    self.model_selection_required = false;
                    let _ = self.config.save(&self.paths);
                    let _ = save_global_model_status(&self.config);
                    self.status = format!("model {}", self.config.model);
                    self.start_next_queued_prompt();
                } else if close {
                    self.overlay = Overlay::None;
                } else {
                    self.overlay = Overlay::ModelPicker {
                        selected,
                        scroll,
                        query,
                    };
                }
                true
            }
            Overlay::FilePicker {
                query,
                directory,
                entries,
                mut selected,
                mut scroll,
            } => {
                let mut close = false;
                let mut next_directory = directory.clone();
                let mut next_query = query.clone();
                let mut chosen_file: Option<PathBuf> = None;
                let mut chosen_mention: Option<String> = None;
                match key.code {
                    KeyCode::Esc => {
                        close = true;
                        self.pending_prompt = None;
                    }
                    KeyCode::Enter => {
                        if let Some(entry) = entries.get(selected).cloned() {
                            match entry.kind {
                                FilePickerEntryKind::Parent | FilePickerEntryKind::Directory => {
                                    next_directory = entry.path;
                                    selected = 0;
                                    scroll = 0;
                                }
                                FilePickerEntryKind::File => {
                                    chosen_file = Some(entry.path);
                                    close = true;
                                }
                                FilePickerEntryKind::Skill | FilePickerEntryKind::McpServer => {
                                    chosen_mention = entry.mention;
                                    close = true;
                                }
                            }
                        }
                    }
                    KeyCode::Up => {
                        if selected > 0 {
                            selected -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if selected + 1 < entries.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::PageUp => {
                        selected = selected.saturating_sub(8);
                    }
                    KeyCode::PageDown => {
                        selected = (selected + 8).min(entries.len().saturating_sub(1));
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => {
                        selected = entries.len().saturating_sub(1);
                    }
                    KeyCode::Backspace => {
                        if self.delete_prev_char() {
                            if let Some((_, _, live_query)) = self.active_attachment_token() {
                                next_query = live_query;
                            } else {
                                close = true;
                            }
                        }
                    }
                    KeyCode::Delete => {
                        let _ = self.delete_next_char();
                    }
                    KeyCode::Char(ch) => {
                        if !key.modifiers.contains(KeyModifiers::CONTROL) {
                            self.insert_text_at_cursor(&ch.to_string());
                            if let Some((_, _, live_query)) = self.active_attachment_token() {
                                next_query = live_query;
                            }
                        }
                    }
                    _ => {}
                }

                let entries = if close {
                    Vec::new()
                } else {
                    list_mention_picker_entries(&self.paths, &next_directory, &next_query)
                        .unwrap_or_default()
                };
                selected = selected.min(entries.len().saturating_sub(1));
                let visible = 8usize;
                if selected < scroll {
                    scroll = selected;
                } else if selected >= scroll.saturating_add(visible) {
                    scroll = selected.saturating_add(1).saturating_sub(visible);
                }

                let had_choice = chosen_file.is_some() || chosen_mention.is_some();
                if let Some(mention) = chosen_mention.clone() {
                    let replacement = format!("@{mention}");
                    if let Some(state) = self.pending_prompt.take() {
                        let prompt =
                            replace_mention_query(&state.original_prompt, &query, &replacement);
                        self.input = prompt.clone();
                        self.cursor = self.input.len();
                        self.input_scroll = 0;
                        match self.build_prompt_plan(&prompt, &state.pinned) {
                            Ok(PromptPlanOutcome::Ready(plan)) => {
                                self.start_prompt_submission(plan);
                                return true;
                            }
                            Ok(PromptPlanOutcome::NeedPicker(next)) => {
                                self.pending_prompt = Some(next.state);
                                self.overlay = Overlay::FilePicker {
                                    query: next.query,
                                    directory: next.directory,
                                    entries: next.entries,
                                    selected: 0,
                                    scroll: 0,
                                };
                                return true;
                            }
                            Err(err) => {
                                self.push_toast("Attachment error", &err);
                            }
                        }
                    } else if let Err(err) = self.replace_active_attachment_token_with(&replacement)
                    {
                        self.push_toast("Attachment error", &err);
                    }
                } else if let Some(path) = chosen_file.clone() {
                    if let Some(state) = self.pending_prompt.take() {
                        let mut pinned = state.pinned.clone();
                        pinned.insert(query.clone(), path);
                        match self.build_prompt_plan(&state.original_prompt, &pinned) {
                            Ok(PromptPlanOutcome::Ready(plan)) => {
                                self.start_prompt_submission(plan);
                                return true;
                            }
                            Ok(PromptPlanOutcome::NeedPicker(next)) => {
                                self.pending_prompt = Some(next.state);
                                self.overlay = Overlay::FilePicker {
                                    query: next.query,
                                    directory: next.directory,
                                    entries: next.entries,
                                    selected: 0,
                                    scroll: 0,
                                };
                                return true;
                            }
                            Err(err) => {
                                self.push_toast("Attachment error", &err);
                            }
                        }
                    } else if let Err(err) = self.replace_active_attachment_token(&path) {
                        self.push_toast("Attachment error", &err);
                    }
                }

                if !close {
                    self.overlay = Overlay::FilePicker {
                        query: next_query,
                        directory: next_directory,
                        entries,
                        selected,
                        scroll,
                    };
                } else {
                    self.overlay = Overlay::None;
                }

                if !had_choice && close {
                    self.overlay = Overlay::None;
                }
                true
            }
            Overlay::McpPanel {
                servers,
                tools,
                errors,
                loading,
                mut scroll,
            } => {
                let mut close = false;
                match key.code {
                    KeyCode::Esc | KeyCode::Enter => close = true,
                    KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::Down => scroll = scroll.saturating_add(1),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(6),
                    KeyCode::PageDown => scroll = scroll.saturating_add(6),
                    KeyCode::Home => scroll = 0,
                    KeyCode::End => scroll = usize::MAX,
                    _ => {}
                }

                if close {
                    self.overlay = Overlay::None;
                } else {
                    self.overlay = Overlay::McpPanel {
                        servers,
                        tools,
                        errors,
                        loading,
                        scroll,
                    };
                }
                true
            }
            Overlay::PermissionPicker { mut selected } => {
                let modes = permission_modes();
                let mut choose = None;
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        choose = modes.get(selected).copied();
                    }
                    KeyCode::Up => {
                        selected = selected.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        selected = (selected + 1).min(modes.len().saturating_sub(1));
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = modes.len().saturating_sub(1),
                    _ => {}
                }

                if let Some(mode) = choose {
                    self.set_permission_mode(mode);
                } else if matches!(key.code, KeyCode::Esc) {
                    self.overlay = Overlay::None;
                } else {
                    self.overlay = Overlay::PermissionPicker { selected };
                }
                true
            }
            Overlay::ProviderPicker { mut selected } => {
                let providers = available_providers(&self.config);
                let mut choose = None;
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        choose = providers.get(selected).map(|provider| provider.id.clone());
                    }
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Down => {
                        selected = (selected + 1).min(providers.len().saturating_sub(1));
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => selected = providers.len().saturating_sub(1),
                    _ => {}
                }

                if let Some(provider_id) = choose {
                    self.select_provider(&provider_id);
                } else if matches!(key.code, KeyCode::Esc) {
                    self.overlay = Overlay::None;
                } else {
                    self.overlay = Overlay::ProviderPicker { selected };
                }
                true
            }
            Overlay::ModelCostWarning {
                mut selected,
                model,
                warning,
                recommendation,
            } => {
                match key.code {
                    KeyCode::Esc => {
                        self.pending_cost_plan = None;
                        self.status = "prompt cancelled".to_string();
                    }
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                        selected = 1usize.saturating_sub(selected.min(1));
                        self.overlay = Overlay::ModelCostWarning {
                            selected,
                            model,
                            warning,
                            recommendation,
                        };
                    }
                    KeyCode::Enter => {
                        if selected == 0 {
                            self.acknowledged_large_models.insert(model);
                            if let Some(plan) = self.pending_cost_plan.take() {
                                self.start_prompt_submission(plan);
                            }
                        } else {
                            self.pending_cost_plan = None;
                            self.status = "prompt cancelled".to_string();
                        }
                    }
                    _ => {
                        self.overlay = Overlay::ModelCostWarning {
                            selected,
                            model,
                            warning,
                            recommendation,
                        };
                    }
                }
                true
            }
            Overlay::LoginGate { .. } => true,
        }
    }

    fn maybe_open_login_gate_on_start(&mut self) {
        if self.login_required {
            self.open_login_gate_overlay();
        }
    }

    fn handle_login_gate_key(&mut self, key: KeyEvent) -> bool {
        if !self.login_required {
            return false;
        }

        let (mut next_selected, current_status) = match &self.overlay {
            Overlay::LoginGate { selected, status } => (*selected, status.clone()),
            _ => {
                self.open_login_gate_overlay();
                return true;
            }
        };
        match key.code {
            KeyCode::Esc => {
                self.should_quit = true;
                return true;
            }
            KeyCode::Up => next_selected = next_selected.saturating_sub(1),
            KeyCode::Down => next_selected = (next_selected + 1).min(2),
            KeyCode::Tab => next_selected = (next_selected + 1) % 3,
            KeyCode::BackTab => next_selected = (next_selected + 2) % 3,
            KeyCode::Enter => {
                if next_selected == 0 {
                    self.should_quit = true;
                    return true;
                }
                if next_selected == 2 {
                    self.overlay = Overlay::ProviderPicker {
                        selected: self.current_provider_index(),
                    };
                    return true;
                }
                if !provider_uses_openrouter_pkce(&self.config.provider) {
                    self.select_provider("openrouter");
                }
                self.start_wire_login_flow();
                return true;
            }
            _ => {}
        }

        self.overlay = Overlay::LoginGate {
            selected: next_selected,
            status: current_status,
        };
        true
    }

    fn open_login_gate_overlay(&mut self) {
        self.overlay = Overlay::LoginGate {
            selected: 1,
            status: "OpenRouter is ready to connect in your browser.".to_string(),
        };
    }

    fn start_wire_login_flow(&mut self) {
        if self.login_rx.is_some() {
            return;
        }
        if !provider_uses_openrouter_pkce(&self.config.provider) {
            self.overlay = Overlay::ProviderPicker {
                selected: self.current_provider_index(),
            };
            self.push_toast(
                "Provider key required",
                "OpenRouter connects in the browser. Other providers use a configured key.",
            );
            return;
        }

        let (tx, rx) = mpsc::channel();
        self.login_rx = Some(rx);
        self.overlay = Overlay::LoginGate {
            selected: 1,
            status: "Opening OpenRouter in your browser...".to_string(),
        };
        self.status = "opening login".to_string();

        thread::spawn(move || {
            let status_tx = tx.clone();
            match login_with_openrouter_progress(|status| {
                let _ = status_tx.send(LoginEvent::Status(status));
            }) {
                Ok(result) => {
                    let _ = tx.send(LoginEvent::Success {
                        api_key: result.api_key,
                        user_id: result.user_id,
                        base_url: result.base_url,
                        model: result.model,
                    });
                }
                Err(err) => {
                    let _ = tx.send(LoginEvent::Error(err));
                }
            }
        });
    }

    fn complete_wire_login(
        &mut self,
        api_key: String,
        user_id: Option<String>,
        base_url: String,
        _model: String,
    ) {
        self.config.provider = "openrouter".to_string();
        self.config.base_url = base_url;
        self.config.model.clear();
        self.config.api_key = Some(api_key);
        self.config.wire_session_token = None;
        self.config.account_id = user_id;
        self.config.account_name = Some("OpenRouter".to_string());
        self.config.account_email = None;
        self.config.api_key_env = None;
        self.config.protocol = ProviderProtocol::ChatCompletions;
        let _ = self.config.save(&self.paths);

        self.login_required = self.config.requires_login();
        self.model_selection_required = true;
        self.models.clear();
        self.selected_model = 0;
        self.health_rx = empty_health_rx();
        self.backend_health = BackendHealth {
            alive: false,
            message: "loading models".to_string(),
            checked_at: Instant::now(),
        };
        self.startup_notice = None;
        self.login_rx = None;
        self.force_model_picker_after_load = true;
        self.overlay = Overlay::None;
        self.status = "OpenRouter connected; choose a model".to_string();
        self.start_model_refresh("loading models");
    }

    fn require_wire_login(&mut self, status: &str) {
        if self.config.api_key.is_some() || self.config.account_id.is_some() {
            self.config.clear_saved_login();
            let _ = self.config.save(&self.paths);
        }
        self.login_required = true;
        self.models_loading = false;
        self.model_rx = None;
        self.health_rx = empty_health_rx();
        self.backend_health.alive = false;
        self.backend_health.message = "login required".to_string();
        self.backend_health.checked_at = Instant::now();
        self.startup_notice = Some(status.to_string());
        self.overlay = Overlay::LoginGate {
            selected: 1,
            status: status.to_string(),
        };
        self.status = "provider credentials required".to_string();
    }

    fn submit_prompt(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            return;
        }

        if let Some(command) = prompt.strip_prefix('!') {
            self.submit_shell_command(command.trim());
            return;
        }

        let command = normalize_tui_command(&prompt);

        if prompt.starts_with('/') && command.is_none() {
            self.push_toast(
                "Unknown command",
                &format!(
                    "{prompt} · try /new, /providers, /login, /models, /permissions, /mcp, or /status"
                ),
            );
            self.clear_prompt_input();
            return;
        }

        if command.as_deref() == Some("/models") {
            if self.running {
                self.push_toast(
                    "Models blocked",
                    "model switching is blocked while the agent is working",
                );
                self.clear_prompt_input();
                return;
            }
            self.start_model_refresh("loading models");
            self.overlay = Overlay::ModelPicker {
                selected: self.selected_model.min(self.models.len().saturating_sub(1)),
                scroll: 0,
                query: String::new(),
            };
            self.clear_prompt_input();
            return;
        }

        if command.as_deref() == Some("/login") {
            self.clear_prompt_input();
            self.start_wire_login_flow();
            return;
        }

        if command.as_deref() == Some("/providers") {
            if self.running {
                self.push_toast(
                    "Providers blocked",
                    "provider switching is blocked while the agent is working",
                );
                self.clear_prompt_input();
                return;
            }
            self.overlay = Overlay::ProviderPicker {
                selected: self.current_provider_index(),
            };
            self.clear_prompt_input();
            return;
        }

        if command.as_deref() == Some("/new") {
            if self.running {
                self.push_toast(
                    "New session blocked",
                    "cannot open a new session while the agent is running",
                );
            } else {
                self.session_id = None;
                self.messages.clear();
                self.usage = TokenUsage::default();
                self.status = "new session".to_string();
                self.activity.clear();
            }
            self.clear_prompt_input();
            self.scroll_feed_to_bottom();
            return;
        }

        if command.as_deref() == Some("/mcp") {
            match McpRegistry::load(&self.paths) {
                Ok(registry) => {
                    let servers = registry.servers().to_vec();
                    self.mcp_rx = Some(spawn_mcp_discovery(registry));
                    self.overlay = Overlay::McpPanel {
                        servers,
                        tools: Vec::new(),
                        errors: Vec::new(),
                        loading: true,
                        scroll: 0,
                    };
                }
                Err(err) => {
                    self.push_toast("MCP error", &err);
                }
            }
            self.clear_prompt_input();
            return;
        }

        if command.as_deref() == Some("/permissions") {
            if self.running {
                self.push_toast(
                    "Permissions blocked",
                    "permission switching is blocked while the agent is working",
                );
                self.clear_prompt_input();
                return;
            }
            self.overlay = Overlay::PermissionPicker {
                selected: self.current_permission_index(),
            };
            self.clear_prompt_input();
            return;
        }

        if command.as_deref() == Some("/status") {
            self.messages.push(ChatMessage {
                role: MessageRole::System,
                title: Some("status".to_string()),
                content: self.status_report(),
            });
            self.clear_prompt_input();
            self.scroll_feed_to_bottom();
            return;
        }

        if self.model_selection_required || self.config.requires_model_selection() {
            if !self.models_loading && !self.login_required {
                self.start_model_refresh("loading models");
            }
            self.overlay = Overlay::ModelPicker {
                selected: self.selected_model.min(self.models.len().saturating_sub(1)),
                scroll: 0,
                query: String::new(),
            };
            self.push_toast("Choose a model", "Select a model before sending prompts.");
            self.status = "choose a model".to_string();
            return;
        }

        let pinned = BTreeMap::new();
        match self.build_prompt_plan(&prompt, &pinned) {
            Ok(PromptPlanOutcome::Ready(plan)) => {
                if self.running {
                    self.queue_prompt(plan);
                } else {
                    self.start_prompt_submission(plan);
                }
            }
            Ok(PromptPlanOutcome::NeedPicker(next)) => {
                self.pending_prompt = Some(next.state);
                self.overlay = Overlay::FilePicker {
                    query: next.query,
                    directory: next.directory,
                    entries: next.entries,
                    selected: 0,
                    scroll: 0,
                };
            }
            Err(err) => {
                self.push_toast("Prompt error", &err);
                self.status = "failed".to_string();
            }
        }
    }

    fn queue_prompt(&mut self, plan: PromptPlan) {
        self.messages.push(ChatMessage {
            role: MessageRole::Queued,
            title: Some("queued".to_string()),
            content: format!(
                "{}\n\nwaiting for the current agent run to finish",
                plan.display_prompt
            ),
        });
        self.queued_prompts.push_back(QueuedPrompt { plan });
        self.status = format!("queued {}", self.queued_prompts.len());
        self.clear_prompt_input();
        self.follow_latest = true;
        self.scroll_feed_to_bottom();
    }

    fn interrupt_current_run_with_input(&mut self) {
        if !self.running {
            return;
        }

        let prompt = self.input.trim().to_string();
        if prompt.is_empty() {
            self.cancel_current_run();
            return;
        }

        if prompt.starts_with('/') || prompt.starts_with('!') {
            self.push_toast(
                "Interrupt",
                "Esc interrupt only submits normal prompts. Use commands after the current run stops.",
            );
            self.cancel_current_run();
            return;
        }

        match self.build_prompt_plan(&prompt, &BTreeMap::new()) {
            Ok(PromptPlanOutcome::Ready(plan)) => {
                self.cancel_current_run();
                self.messages.push(ChatMessage {
                    role: MessageRole::Queued,
                    title: Some("interrupt queued".to_string()),
                    content: format!(
                        "{}\n\ncurrent run is being interrupted; this prompt will start next",
                        plan.display_prompt
                    ),
                });
                self.queued_prompts.push_front(QueuedPrompt { plan });
                self.clear_prompt_input();
                self.follow_latest = true;
                self.scroll_feed_to_bottom();
            }
            Ok(PromptPlanOutcome::NeedPicker(_)) => {
                self.push_toast(
                    "Interrupt",
                    "resolve @file attachments before interrupting with Esc",
                );
            }
            Err(err) => {
                self.push_toast("Interrupt error", &err);
            }
        }
    }

    fn cancel_current_run(&mut self) {
        if let Some(control) = &self.control {
            control.cancel();
        }
        self.status = "interrupting".to_string();
        self.activity = "interrupt requested".to_string();
    }

    fn start_next_queued_prompt(&mut self) {
        if self.running {
            return;
        }
        if let Some(queued) = self.queued_prompts.pop_front() {
            self.start_prompt_submission(queued.plan);
        }
    }

    fn submit_shell_command(&mut self, command: &str) {
        if self.running {
            self.push_toast(
                "Shell blocked",
                "shell mode is disabled while the agent is working",
            );
            self.clear_prompt_input();
            return;
        }
        if command.is_empty() {
            return;
        }
        if matches!(command.trim(), "clear" | "cls") {
            self.messages.clear();
            self.clear_prompt_input();
            self.status = "screen cleared".to_string();
            return;
        }
        let output = run_shell_command(&self.paths.root_dir, &self.config, command);
        self.messages.push(ChatMessage {
            role: MessageRole::Shell,
            title: Some(format!("Ran {}", truncate_display(command, 96))),
            content: if output.trim().is_empty() {
                "└ no output".to_string()
            } else {
                output
            },
        });
        self.clear_prompt_input();
        self.status = "shell complete".to_string();
        self.scroll_feed_to_bottom();
    }

    fn start_prompt_submission(&mut self, plan: PromptPlan) {
        if self.model_selection_required || self.config.requires_model_selection() {
            self.queue_prompt(plan);
            if !self.models_loading && !self.login_required {
                self.start_model_refresh("loading models");
            }
            self.overlay = Overlay::ModelPicker {
                selected: self.selected_model.min(self.models.len().saturating_sub(1)),
                scroll: 0,
                query: String::new(),
            };
            self.push_toast(
                "Choose a model",
                "Select a model before sending queued prompts.",
            );
            self.status = "choose a model".to_string();
            return;
        }

        self.config.model = self.current_model();
        if let Some(message) = provider_model_mismatch(&self.config) {
            self.queue_prompt(plan);
            self.config.model.clear();
            self.model_selection_required = true;
            self.force_model_picker_after_load = true;
            let profile = active_provider(&self.config);
            self.models = provider_models(&profile, "");
            self.selected_model = 0;
            self.overlay = Overlay::ModelPicker {
                selected: 0,
                scroll: 0,
                query: String::new(),
            };
            self.push_toast("Choose a provider model", &message);
            self.status = "choose a model".to_string();
            let _ = self.config.save(&self.paths);
            return;
        }
        if let Some((model, warning, recommendation)) = self.current_model_warning() {
            if !self.acknowledged_large_models.contains(&model) {
                self.pending_cost_plan = Some(plan);
                self.status = "model cost warning".to_string();
                self.overlay = Overlay::ModelCostWarning {
                    selected: 1,
                    model,
                    warning,
                    recommendation,
                };
                return;
            }
        }

        self.messages.push(ChatMessage {
            role: MessageRole::User,
            title: None,
            content: plan.display_prompt.clone(),
        });
        self.messages.push(ChatMessage {
            role: MessageRole::Assistant,
            title: None,
            content: String::new(),
        });
        self.status = "running".to_string();
        self.running = true;
        self.activity = "sending request".to_string();
        self.started_at = Instant::now();
        self.follow_latest = true;
        self.scroll_feed_to_bottom();
        self.clear_prompt_input();

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let control = AgentControl::default();
        self.control = Some(control.clone());
        let paths = self.paths.clone();
        let config = self.config.clone();
        let session_id = self.session_id.clone();
        let prompt = PromptInput {
            text: plan.model_prompt,
            images: plan.images,
        };

        thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = tx.send(UiEvent::Error(err.to_string()));
                    return;
                }
            };

            let mut observer = TuiObserver { tx: tx.clone() };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                runtime.block_on(responses_agent::run_prompt_input_in_session_with_observer(
                    &paths,
                    &config,
                    session_id,
                    prompt,
                    control,
                    CommandPolicy::standard(),
                    &mut observer,
                ))
            }))
            .unwrap_or_else(|payload| {
                let reason = payload
                    .downcast_ref::<&str>()
                    .map(|value| value.to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "agent worker panicked".to_string());
                Err(reason)
            });
            runtime.shutdown_background();

            match result {
                Ok((session_id, output)) => {
                    let _ = tx.send(UiEvent::Done { session_id, output });
                }
                Err(err) => {
                    let _ = tx.send(UiEvent::Error(err));
                }
            }
        });
    }

    fn drain_events(&mut self) {
        self.expire_toasts();
        let mut model_events = Vec::new();
        if let Some(rx) = &self.model_rx {
            while let Ok(event) = rx.try_recv() {
                model_events.push(event);
            }
        }
        for event in model_events {
            self.models_loading = false;
            self.model_rx = None;
            match event {
                ModelLoadEvent::Loaded(models) if !models.is_empty() => {
                    let current = self.config.model.clone();
                    self.models = models;
                    self.selected_model = if self.model_selection_required
                        || self.config.requires_model_selection()
                    {
                        0
                    } else {
                        self.models
                            .iter()
                            .position(|model| model.id == current)
                            .or_else(|| {
                                self.models
                                    .iter()
                                    .position(|model| model.id == self.config.model)
                            })
                            .unwrap_or(0)
                    };
                    self.backend_health.alive = true;
                    self.backend_health.message = "backend healthy".to_string();
                    self.backend_health.checked_at = Instant::now();
                    self.startup_notice = None;
                    if self.force_model_picker_after_load {
                        self.force_model_picker_after_load = false;
                        self.overlay = Overlay::ModelPicker {
                            selected: self.selected_model,
                            scroll: 0,
                            query: String::new(),
                        };
                    }
                }
                ModelLoadEvent::Loaded(_) => {
                    self.force_model_picker_after_load = false;
                    self.backend_health.alive = false;
                    self.backend_health.message = "backend returned no models".to_string();
                    self.backend_health.checked_at = Instant::now();
                    self.startup_notice = Some("models unavailable".to_string());
                }
                ModelLoadEvent::Failed(err) => {
                    self.force_model_picker_after_load = false;
                    if err == "login required" {
                        if provider_uses_openrouter_pkce(&self.config.provider) {
                            self.require_wire_login(
                                "OpenRouter session expired. Press Enter to reconnect.",
                            );
                        } else {
                            self.backend_health.alive = false;
                            self.backend_health.message = "provider key required".to_string();
                            self.backend_health.checked_at = Instant::now();
                            self.startup_notice = Some("provider API key required".to_string());
                        }
                    } else {
                        self.backend_health.alive = false;
                        self.backend_health.message = err.clone();
                        self.backend_health.checked_at = Instant::now();
                        self.startup_notice = Some(format!("models unavailable: {err}"));
                    }
                }
            }
        }

        let mut login_events = Vec::new();
        if let Some(rx) = &self.login_rx {
            while let Ok(event) = rx.try_recv() {
                login_events.push(event);
            }
        }
        for event in login_events {
            match event {
                LoginEvent::Status(status) => {
                    if let Overlay::LoginGate {
                        status: overlay_status,
                        ..
                    } = &mut self.overlay
                    {
                        *overlay_status = status.clone();
                    }
                    self.status = status;
                }
                LoginEvent::Success {
                    api_key,
                    user_id,
                    base_url,
                    model,
                } => {
                    self.complete_wire_login(api_key, user_id, base_url, model);
                }
                LoginEvent::Error(err) => {
                    if let Overlay::LoginGate { status, .. } = &mut self.overlay {
                        *status = format!("login failed: {err}");
                    } else {
                        self.overlay = Overlay::LoginGate {
                            selected: 1,
                            status: format!("login failed: {err}"),
                        };
                    }
                    self.status = "login failed".to_string();
                    self.login_rx = None;
                }
            }
        }

        let mut mcp_events = Vec::new();
        if let Some(rx) = &self.mcp_rx {
            while let Ok(event) = rx.try_recv() {
                mcp_events.push(event);
            }
        }
        for event in mcp_events {
            self.mcp_rx = None;
            let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
            if let Overlay::McpPanel {
                servers, scroll, ..
            } = overlay
            {
                self.overlay = Overlay::McpPanel {
                    servers,
                    tools: event.tools,
                    errors: event.errors,
                    loading: false,
                    scroll,
                };
            } else {
                self.overlay = overlay;
            }
        }

        while let Ok(health) = self.health_rx.try_recv() {
            let was_alive = self.backend_health.alive;
            self.backend_health = health;
            if self.backend_health.alive {
                self.startup_notice = None;
                if !was_alive && !self.models_loading {
                    self.start_model_refresh("loading models");
                }
            } else {
                self.startup_notice = Some(self.backend_health.message.clone());
            }
        }

        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }

        for event in events {
            match event {
                UiEvent::SessionBound(session_id) => {
                    self.session_id = Some(session_id.clone());
                    if self.status == "ready" || self.status.starts_with("session ") {
                        self.status = format!("session {}", compact_session_id(&session_id));
                    }
                }
                UiEvent::Delta(delta) => {
                    if self
                        .messages
                        .last()
                        .map(|message| !matches!(message.role, MessageRole::Assistant))
                        .unwrap_or(true)
                    {
                        self.messages.push(ChatMessage {
                            role: MessageRole::Assistant,
                            title: None,
                            content: String::new(),
                        });
                    }
                    if let Some(last) = self.messages.last_mut() {
                        if matches!(last.role, MessageRole::Assistant) {
                            last.content.push_str(&delta);
                        }
                    }
                    if self.follow_latest {
                        self.scroll_feed_to_bottom();
                    }
                }
                UiEvent::ToolDelta {
                    name,
                    arguments_delta,
                } => {
                    self.activity = name
                        .as_deref()
                        .map(|name| format!("receiving {}", tool_activity_label(name)))
                        .unwrap_or_else(|| "receiving tool call".to_string());
                    remove_trailing_empty_assistant(&mut self.messages);
                    let index = latest_streaming_tool_message_index(&self.messages);
                    let title = streaming_tool_title(name.as_deref());
                    if let Some(index) = index {
                        if self.messages[index]
                            .title
                            .as_deref()
                            .map(|current| current == "Receiving tool call")
                            .unwrap_or(false)
                            && name.is_some()
                        {
                            self.messages[index].title = Some(title);
                        }
                        append_streaming_tool_delta(
                            &mut self.messages[index].content,
                            name.as_deref(),
                            &arguments_delta,
                        );
                    } else {
                        let mut content = String::new();
                        append_streaming_tool_delta(
                            &mut content,
                            name.as_deref(),
                            &arguments_delta,
                        );
                        self.messages.push(ChatMessage {
                            role: MessageRole::Tool,
                            title: Some(title),
                            content,
                        });
                    }
                    if self.follow_latest {
                        self.scroll_feed_to_bottom();
                    }
                }
                UiEvent::Status(status) => {
                    self.activity = if status.trim().is_empty() {
                        "Thinking".to_string()
                    } else {
                        status
                    };
                }
                UiEvent::ToolStart { name, summary } => {
                    self.remember_tool_summary(&name, &summary);
                    let label = tool_start_title(&name, &summary);
                    self.activity = visible_tool_intent(&name, Some(&summary));
                    remove_trailing_empty_assistant(&mut self.messages);
                    if let Some(index) = latest_streaming_tool_message_index(&self.messages) {
                        self.messages[index].title = Some(label);
                        self.messages[index].content =
                            format!("└ {}", visible_tool_intent(&name, Some(&summary)));
                        if self.follow_latest {
                            self.scroll_feed_to_bottom();
                        }
                        continue;
                    }
                    if is_plan_title(&label) && latest_plan_message_index(&self.messages).is_some()
                    {
                        if self.follow_latest {
                            self.scroll_feed_to_bottom();
                        }
                        continue;
                    }
                    self.messages.push(ChatMessage {
                        role: MessageRole::Tool,
                        title: Some(label),
                        content: if is_plan_title(&tool_label(&name)) {
                            String::new()
                        } else {
                            format!("└ {}", visible_tool_intent(&name, Some(&summary)))
                        },
                    });
                    if self.follow_latest {
                        self.scroll_feed_to_bottom();
                    }
                }
                UiEvent::ToolResult { name, output } => {
                    let summary = self.take_tool_summary(&name);
                    let label = tool_result_title(&name, summary.as_deref(), &output);
                    let ui_output = tool_result_body(&name, summary.as_deref(), &output);
                    self.activity = format!("done {}", tool_activity_label(&name));
                    if is_plan_title(&label) {
                        if let Some(index) = latest_plan_message_index(&self.messages) {
                            self.messages[index].content = ui_output;
                            self.messages[index].title = Some(label);
                        } else {
                            self.messages.push(ChatMessage {
                                role: MessageRole::Tool,
                                title: Some(label),
                                content: ui_output,
                            });
                        }
                    } else if let Some(index) =
                        latest_empty_tool_message_index(&self.messages, &label)
                            .or_else(|| latest_empty_tool_any_index(&self.messages))
                            .or_else(|| latest_tool_message_index(&self.messages, &label))
                    {
                        self.messages[index].content = ui_output;
                        self.messages[index].title = Some(label);
                    } else {
                        self.messages.push(ChatMessage {
                            role: MessageRole::Tool,
                            title: Some(label),
                            content: ui_output,
                        });
                    }
                    if self.follow_latest {
                        self.scroll_feed_to_bottom();
                    }
                }
                UiEvent::Usage(usage) => {
                    self.usage.input_tokens =
                        self.usage.input_tokens.saturating_add(usage.input_tokens);
                    self.usage.output_tokens =
                        self.usage.output_tokens.saturating_add(usage.output_tokens);
                    self.usage.total_tokens =
                        self.usage.total_tokens.saturating_add(usage.total_tokens);
                }
                UiEvent::Done { session_id, output } => {
                    self.session_id = Some(session_id.clone());
                    remove_trailing_empty_assistant(&mut self.messages);
                    if self
                        .messages
                        .last()
                        .map(|message| !matches!(message.role, MessageRole::Assistant))
                        .unwrap_or(true)
                    {
                        self.messages.push(ChatMessage {
                            role: MessageRole::Assistant,
                            title: None,
                            content: output,
                        });
                    } else if let Some(last) = self.messages.last_mut() {
                        last.content = output;
                    }
                    self.status = format!("session {}", compact_session_id(&session_id));
                    self.running = false;
                    self.activity.clear();
                    self.rx = None;
                    self.control = None;
                    self.follow_latest = true;
                    self.scroll_feed_to_bottom();
                    self.start_next_queued_prompt();
                }
                UiEvent::Error(err) => {
                    let show_error_card = err != "cancelled";
                    let err_lower = err.to_ascii_lowercase();
                    if err == "cancelled" {
                        self.push_toast("Interrupted", "current run interrupted");
                        self.status = "interrupted".to_string();
                    } else if err.contains("requires environment variable")
                        || err.contains("configured api_key")
                    {
                        self.status = "provider key missing".to_string();
                    } else if err == "login required" || err.starts_with("login required") {
                        if provider_uses_openrouter_pkce(&self.config.provider) {
                            self.require_wire_login(
                                "OpenRouter session expired. Press Enter to reconnect.",
                            );
                        } else {
                            self.push_toast(
                                "Provider key missing",
                                "Set the provider API key env var or api_key in config.",
                            );
                            self.status = "provider key missing".to_string();
                        }
                    } else if err.contains("Invalid API key")
                        || err.contains("invalid_api_key")
                        || err.contains("401 Unauthorized")
                    {
                        if provider_uses_openrouter_pkce(&self.config.provider) {
                            self.require_wire_login(
                                "OpenRouter key expired. Press Enter to reconnect.",
                            );
                        } else {
                            self.push_toast(
                                "Provider key invalid",
                                "Check the provider API key configured for this project.",
                            );
                            self.status = "provider key invalid".to_string();
                        }
                    } else if err.contains("402 Payment Required")
                        || err.contains("returned 402")
                        || err.contains("requires more credits")
                        || err_lower.contains("credit")
                    {
                        self.push_toast("No credits", "The connected account has no credits, or the token limit is too high for the current balance.");
                        self.status = "account has no credits".to_string();
                    } else if err.contains("429 Too Many Requests")
                        || err.contains("returned 429")
                        || err.contains("\"code\":429")
                        || err.contains("\"status\":429")
                        || err.contains("temporarily rate-limited upstream")
                        || err_lower.contains("rate limit")
                        || err_lower.contains("rate-limit")
                        || err_lower.contains("rate-limited")
                    {
                        self.push_toast(
                            "Rate limited",
                            "The connected account is temporarily rate limited. Try again shortly.",
                        );
                        self.status = "account rate limited".to_string();
                    } else if err_lower.contains("empty response without text or tool calls")
                        || err_lower.contains("empty response without a final text response")
                        || err_lower.contains("completed without visible output")
                    {
                        self.push_toast(
                            "Empty response",
                            "The provider ended without text or tool calls. Try again shortly.",
                        );
                        self.status = "provider empty response".to_string();
                    } else {
                        self.push_toast("Error", &err);
                        self.status = "failed".to_string();
                    }
                    if self.session_id.is_some() && self.status == "failed" {
                        self.status = "session failed".to_string();
                    }
                    if show_error_card {
                        let (title, body) = provider_error_card(&err);
                        replace_pending_assistant_with_error(&mut self.messages, &title, &body);
                    }
                    self.running = false;
                    self.activity.clear();
                    self.rx = None;
                    self.control = None;
                    self.follow_latest = true;
                    self.scroll_feed_to_bottom();
                    self.start_next_queued_prompt();
                }
            }
        }
    }

    fn draw(&mut self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        self.feed_area = Rect::new(0, 0, 0, 0);
        frame.render_widget(
            Block::default().style(Style::default().bg(self.theme.background)),
            area,
        );
        let area = if self.backend_banner_message().is_some() {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(0)])
                .split(area);
            self.draw_backend_banner(frame, layout[0]);
            layout[1]
        } else {
            area
        };
        if self.login_required {
            self.draw_login_gate(frame, area);
        } else if self.messages.is_empty() {
            self.draw_welcome(frame, area);
            self.draw_prompt(frame, welcome_prompt_rect(area));
        } else {
            let header_height =
                if self.running || !self.activity.is_empty() || !self.queued_prompts.is_empty() {
                    2
                } else {
                    0
                };
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(header_height),
                    Constraint::Min(0),
                    Constraint::Length(self.prompt_height()),
                ])
                .split(area);
            if header_height > 0 {
                self.draw_observability_strip(frame, layout[0]);
            }
            self.draw_feed(frame, layout[1]);
            self.draw_prompt(frame, layout[2]);
        }

        match &self.overlay {
            Overlay::None => {}
            Overlay::LoginGate { selected, status } => {
                self.draw_login_gate_overlay(frame, *selected, status);
            }
            Overlay::ProviderPicker { selected } => {
                self.draw_provider_picker(frame, *selected);
            }
            Overlay::ModelCostWarning {
                selected,
                model,
                warning,
                recommendation,
            } => {
                self.draw_model_cost_warning(frame, *selected, model, warning, recommendation);
            }
            Overlay::ModelPicker {
                selected,
                scroll,
                query,
            } => {
                self.draw_model_picker(frame, *selected, *scroll, query);
            }
            Overlay::FilePicker {
                query,
                directory,
                entries,
                selected,
                scroll,
            } => {
                self.draw_file_picker(frame, query, directory, entries, *selected, *scroll);
            }
            Overlay::McpPanel {
                servers,
                tools,
                errors,
                loading,
                scroll,
            } => {
                self.draw_mcp_panel(frame, servers, tools, errors, *loading, *scroll);
            }
            Overlay::PermissionPicker { selected } => {
                self.draw_permission_picker(frame, *selected);
            }
        }
        self.draw_toasts(frame);
    }

    fn remember_tool_summary(&mut self, name: &str, summary: &str) {
        self.tool_summaries
            .entry(name.to_string())
            .or_default()
            .push_back(summary.to_string());
    }

    fn take_tool_summary(&mut self, name: &str) -> Option<String> {
        self.tool_summaries
            .get_mut(name)
            .and_then(|items| items.pop_front())
            .filter(|summary| !summary.trim().is_empty())
    }

    fn push_toast(&mut self, title: &str, body: &str) {
        self.toasts.push_back(Toast {
            title: title.to_string(),
            body: body.to_string(),
            created_at: Instant::now(),
            ttl: Duration::from_secs(6),
        });
        while self.toasts.len() > 4 {
            self.toasts.pop_front();
        }
    }

    fn expire_toasts(&mut self) {
        let now = Instant::now();
        self.toasts
            .retain(|toast| now.duration_since(toast.created_at) < toast.ttl);
    }

    fn draw_login_gate(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let panel = centered_rect(78.min(area.width.saturating_sub(6)), 12, area);
        frame.render_widget(Clear, panel);
        let block = Block::default()
            .title("wire cli")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.border));
        let inner = block.inner(panel);
        frame.render_widget(block, panel);

        let header = vec![
            Line::from(Span::styled(
                "Connect a model",
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("OpenRouter connects in your browser and becomes the default."),
            Line::from("Other providers use keys configured in the project."),
            Line::from("Issued keys are stored encrypted on this machine."),
        ];
        frame.render_widget(
            Paragraph::new(header).wrap(Wrap { trim: false }),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 5,
            },
        );
    }

    fn draw_login_gate_overlay(
        &self,
        frame: &mut ratatui::Frame<'_>,
        selected: usize,
        status: &str,
    ) {
        let area = centered_rect(
            84.min(frame.area().width.saturating_sub(6)),
            13,
            frame.area(),
        );
        frame.render_widget(Clear, area);
        let block = Block::default()
            .title("wire cli")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.border));
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let header = vec![
            Line::from(vec![
                Span::styled(
                    "WIRE CLI",
                    Style::default()
                        .fg(self.theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  connect a model", Style::default().fg(self.theme.text)),
            ]),
            Line::from(Span::styled(
                "OpenRouter by default  ·  browser login  ·  protected local keys",
                Style::default().fg(self.theme.muted),
            )),
        ];
        frame.render_widget(
            Paragraph::new(header).wrap(Wrap { trim: false }),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: 2,
            },
        );

        let rows = vec![
            ("Exit", "Close Wire CLI".to_string()),
            (
                "Connect",
                "Sign in with OpenRouter in your browser".to_string(),
            ),
            (
                "Providers",
                "Choose OpenAI, Claude, Google, or custom".to_string(),
            ),
        ];
        let mut y = inner.y + 3;
        for (i, (name, value)) in rows.iter().enumerate() {
            frame.render_widget(
                Paragraph::new(format!("{name}  ·  {value}")).style(if i == selected {
                    selected_style(&self.theme)
                } else {
                    Style::default().fg(self.theme.text)
                }),
                Rect {
                    x: inner.x,
                    y,
                    width: inner.width,
                    height: 1,
                },
            );
            y += 1;
        }

        frame.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled(
                    "After approval, Wire CLI stores only the encrypted issued key.",
                    Style::default().fg(self.theme.muted),
                )),
                Line::from(Span::styled(
                    status.to_string(),
                    Style::default().fg(self.theme.text),
                )),
            ])
            .style(Style::default().fg(self.theme.muted)),
            Rect {
                x: inner.x,
                y: inner.y + inner.height.saturating_sub(2),
                width: inner.width,
                height: 2,
            },
        );
    }

    fn backend_banner_message(&self) -> Option<String> {
        None
    }

    fn draw_backend_banner(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let message = self.backend_banner_message().unwrap_or_default();
        let banner = Paragraph::new(vec![
            Line::from(vec![
                Span::styled(
                    "BACKEND OFFLINE",
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "  Wire CLI is still usable; requests will fail until proxy returns.",
                    Style::default().fg(Color::White).bg(Color::Black),
                ),
            ]),
            Line::from(Span::styled(
                message,
                Style::default().fg(Color::White).bg(Color::Black),
            )),
        ])
        .style(Style::default().fg(Color::White).bg(Color::Black))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::White).bg(Color::Black))
                .style(Style::default().fg(Color::White).bg(Color::Black))
                .border_type(BorderType::Plain),
        )
        .wrap(Wrap { trim: false });
        frame.render_widget(banner, area);
    }

    fn draw_toasts(&self, frame: &mut ratatui::Frame<'_>) {
        if self.toasts.is_empty() {
            return;
        }
        let area = frame.area();
        let width = area.width.saturating_sub(6).min(72).max(24);
        let mut y = area.y.saturating_add(1);
        for toast in self.toasts.iter().rev().take(3) {
            let body = truncate_display(&toast.body, width.saturating_sub(4) as usize);
            let height = if body.is_empty() { 3 } else { 4 };
            let x = area
                .x
                .saturating_add(area.width.saturating_sub(width).saturating_sub(2));
            let rect = Rect {
                x,
                y,
                width,
                height,
            };
            frame.render_widget(Clear, rect);
            let block = Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(self.theme.danger))
                .border_type(BorderType::Plain)
                .style(Style::default().fg(self.theme.text).bg(Color::Black));
            let inner = block.inner(rect);
            frame.render_widget(block, rect);
            let mut lines = vec![Line::from(vec![
                Span::styled(
                    "!",
                    Style::default()
                        .fg(self.theme.danger)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(
                    toast.title.clone(),
                    Style::default()
                        .fg(self.theme.text)
                        .add_modifier(Modifier::BOLD),
                ),
            ])];
            if !body.is_empty() {
                lines.push(Line::from(Span::styled(
                    body,
                    Style::default().fg(self.theme.muted),
                )));
            }
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            y = y.saturating_add(height.saturating_add(1));
            if y >= area.height.saturating_sub(2) {
                break;
            }
        }
    }

    fn draw_welcome(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let logo_height = self.logo.len() as u16;
        let logo_area = centered_rect(
            area.width.saturating_sub(18).min(72),
            logo_height,
            Rect {
                x: area.x,
                y: area.y.saturating_add(1),
                width: area.width,
                height: area.height.saturating_sub(10),
            },
        );
        let logo_lines = self
            .logo
            .iter()
            .map(|line| {
                Line::from(Span::styled(
                    line.clone(),
                    Style::default().fg(self.theme.muted),
                ))
            })
            .collect::<Vec<_>>();
        let logo = Paragraph::new(logo_lines)
            .alignment(Alignment::Center)
            .block(Block::default());
        frame.render_widget(logo, logo_area);

        let hint = Paragraph::new(Line::from(vec![
            Span::styled(
                "Enter",
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" send  "),
            Span::styled(
                "Shift+Enter",
                Style::default()
                    .fg(self.theme.text)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" newline"),
        ]))
        .alignment(Alignment::Center)
        .style(Style::default().fg(self.theme.muted));
        let hint_area = Rect {
            x: area.x + 2,
            y: logo_area.y + logo_area.height,
            width: area.width.saturating_sub(4),
            height: if self.startup_notice.is_some() { 2 } else { 1 },
        };
        frame.render_widget(
            hint,
            Rect {
                height: 1,
                ..hint_area
            },
        );
        if let Some(notice) = &self.startup_notice {
            let notice_area = Rect {
                x: hint_area.x,
                y: hint_area.y + 1,
                width: hint_area.width,
                height: 1,
            };
            frame.render_widget(
                Paragraph::new(notice.clone())
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(self.theme.muted)),
                notice_area,
            );
        }
    }

    fn draw_feed(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        self.feed_area = area;
        let mut lines = Vec::new();
        for (idx, message) in self.messages.iter().enumerate() {
            let is_working_card = self.running
                && idx + 1 == self.messages.len()
                && matches!(message.role, MessageRole::Assistant | MessageRole::Tool)
                && message.content.is_empty();
            let is_latest_message = idx + 1 == self.messages.len();
            let compact =
                matches!(message.role, MessageRole::Tool) && !is_working_card && !is_latest_message;
            let card_title = message_title(message, is_working_card, self.started_at.elapsed());
            let title_color = card_color(&message.role, &card_title, &self.theme);
            let text_color = body_color(&message.role, &card_title, &self.theme);
            lines.extend(render_card(
                &message.role,
                &card_title,
                title_color,
                text_color,
                &message.content,
                matches!(message.role, MessageRole::Assistant | MessageRole::Tool),
                is_working_card,
                matches!(message.role, MessageRole::Tool)
                    && is_diff_tool_title(message.title.as_deref()),
                compact,
                area.width as usize,
                self.started_at.elapsed(),
                &self.theme,
            ));
            if should_draw_task_separator(message) {
                lines.push(render_task_separator(area.width as usize, &self.theme));
            }
            lines.push(Line::from(""));
        }

        let viewport_rows = area.height as usize;
        let max_scroll = lines.len().saturating_sub(viewport_rows);
        self.feed_max_scroll = max_scroll;
        if self.follow_latest {
            self.feed_scroll = max_scroll;
        } else {
            self.feed_scroll = self.feed_scroll.min(max_scroll);
        }
        let scroll_rows = self.feed_scroll;
        let visible = lines
            .into_iter()
            .skip(scroll_rows)
            .take(viewport_rows)
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible).wrap(Wrap { trim: false }), area);
    }

    fn draw_observability_strip(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        if area.height == 0 {
            return;
        }
        let queue = self.queued_prompts.len();
        let activity = if self.activity.is_empty() {
            "idle"
        } else {
            self.activity.as_str()
        };
        let session = self
            .session_id
            .as_deref()
            .map(compact_session_id)
            .unwrap_or_else(|| "-".to_string());
        let context_left = self
            .models
            .get(self.selected_model)
            .and_then(|model| model.context_window)
            .map(|window| window.saturating_sub(self.usage.total_tokens))
            .map(compact_number)
            .unwrap_or_else(|| "unknown".to_string());
        let line = Line::from(vec![
            Span::styled(
                "wire",
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
            Span::styled(
                self.config.provider_status_label(),
                Style::default().fg(self.theme.text),
            ),
            Span::raw(" · "),
            Span::styled(self.current_model(), Style::default().fg(self.theme.text)),
            Span::raw(" · "),
            Span::styled(
                format!("queue {queue}"),
                Style::default().fg(self.theme.muted),
            ),
            Span::raw(" · "),
            Span::styled(activity.to_string(), Style::default().fg(self.theme.muted)),
            Span::raw(" · "),
            Span::styled(
                format!("ctx {context_left}"),
                Style::default().fg(self.theme.muted),
            ),
            Span::raw(" · "),
            Span::styled(
                format!("session {session}"),
                Style::default().fg(self.theme.muted),
            ),
            Span::raw(" · "),
            Span::styled(
                format!(
                    "{} / {} / {}",
                    self.usage.input_tokens, self.usage.output_tokens, self.usage.total_tokens
                ),
                Style::default().fg(self.theme.muted),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line).wrap(Wrap { trim: true }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Plain)
                    .border_style(Style::default().fg(self.theme.border)),
            ),
            area,
        );
    }

    fn draw_prompt(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let prompt_area = area.inner(Margin {
            vertical: 0,
            horizontal: 1,
        });
        let input_area = Rect {
            x: prompt_area.x + 1,
            y: prompt_area.y + 1,
            width: prompt_area.width.saturating_sub(2),
            height: prompt_area.height.saturating_sub(2),
        };
        let title = if self.running {
            format!(
                "prompt  · {}  · {}  · {}  · working ({}s)",
                self.config.provider_status_label(),
                self.current_model(),
                self.config.permission_mode.title(),
                self.started_at.elapsed().as_secs()
            )
        } else {
            format!(
                "prompt  · {}  · {}  · {}",
                self.config.provider_status_label(),
                self.current_model(),
                self.config.permission_mode.title(),
            )
        };
        let text = if self.input.is_empty() {
            vec![Line::from("")]
        } else {
            self.input
                .split('\n')
                .map(|line| Line::from(Span::raw(line.to_string())))
                .collect::<Vec<_>>()
        };

        let input = Paragraph::new(text)
            .block(
                Block::default()
                    .title(Line::from(vec![
                        Span::styled(
                            "wirecli ",
                            Style::default()
                                .fg(self.theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(title, Style::default().fg(self.theme.muted)),
                    ]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(
                        if self.input.trim_start().starts_with('!') {
                            self.theme.danger
                        } else {
                            self.theme.border
                        },
                    ))
                    .border_type(BorderType::Plain),
            )
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Left);

        frame.render_widget(Clear, prompt_area);
        frame.render_widget(
            Block::default().style(Style::default().bg(self.theme.background)),
            prompt_area,
        );
        frame.render_widget(input, prompt_area);
        frame.render_widget(Clear, input_area);
        frame.render_widget(
            Block::default().style(Style::default().bg(self.theme.background)),
            input_area,
        );

        let (cursor_row, cursor_col) = cursor_position(&self.input, self.cursor);
        let max_visible_start = self
            .input_line_count()
            .saturating_sub(input_area.height.max(1) as usize);
        let visible_start = self.input_scroll.min(max_visible_start);

        if self.input.is_empty() {
            frame.render_widget(
                Paragraph::new("type a prompt")
                    .alignment(Alignment::Left)
                    .style(Style::default().fg(self.theme.muted)),
                Rect {
                    x: input_area.x,
                    y: input_area.y,
                    width: input_area.width,
                    height: 1,
                },
            );
        } else {
            let text = self
                .input
                .split('\n')
                .skip(visible_start)
                .take(input_area.height as usize)
                .map(|line| Line::from(Span::raw(line.to_string())))
                .collect::<Vec<_>>();
            let body = Paragraph::new(text)
                .alignment(Alignment::Left)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(self.theme.text));
            frame.render_widget(body, input_area);
        }

        let cursor_x =
            input_area.x + cursor_col.min(input_area.width.saturating_sub(1) as usize) as u16;
        let visible_cursor_row = cursor_row.saturating_sub(visible_start);
        let cursor_y = input_area.y
            + visible_cursor_row.min(input_area.height.saturating_sub(1) as usize) as u16;
        frame.set_cursor_position((cursor_x, cursor_y));

        let suggestions = command_suggestions(&self.input);
        if !suggestions.is_empty() {
            let line = format!("commands: {}", suggestions.join("  ·  "));
            frame.render_widget(
                Paragraph::new(line).style(Style::default().fg(self.theme.muted)),
                Rect {
                    x: prompt_area.x + 1,
                    y: prompt_area.y.saturating_sub(1),
                    width: prompt_area.width.saturating_sub(2),
                    height: 1,
                },
            );
        }
    }

    fn draw_model_picker(
        &self,
        frame: &mut ratatui::Frame<'_>,
        selected: usize,
        scroll: usize,
        query: &str,
    ) {
        let model_indexes = self.filtered_model_indexes(query);
        let width = self
            .models
            .iter()
            .map(|m| m.title().len().max(m.subtitle().len()))
            .max()
            .unwrap_or(12)
            .saturating_add(8)
            .min(72) as u16;
        let height = (model_indexes.len().min(9) as u16).saturating_add(4);
        let area = centered_rect(width, height, frame.area());
        frame.render_widget(Clear, area);

        let visible = area.height.saturating_sub(3).max(1) as usize;
        let start = scroll.min(model_indexes.len().saturating_sub(1));
        let end = (start + visible).min(model_indexes.len());
        let slice = &model_indexes[start..end];

        let items = slice
            .iter()
            .filter_map(|idx| self.models.get(*idx))
            .map(|model| {
                let title_color = if model.is_large_or_premium() {
                    self.theme.danger
                } else {
                    self.theme.text
                };
                ListItem::new(vec![
                    Line::from(Span::styled(
                        model.title(),
                        Style::default().fg(title_color),
                    )),
                    Line::from(Span::styled(
                        model.subtitle(),
                        Style::default().fg(self.theme.muted),
                    )),
                ])
            })
            .collect::<Vec<_>>();

        let mut state = ListState::default();
        state.select(Some(
            selected
                .saturating_sub(start)
                .min(slice.len().saturating_sub(1)),
        ));

        let list = List::new(items)
            .block(
                Block::default()
                    .title(Line::from(vec![Span::styled(
                        if self.models_loading {
                            "models loading"
                        } else if query.is_empty() {
                            "models"
                        } else {
                            "models filtered"
                        },
                        Style::default()
                            .fg(self.theme.accent)
                            .add_modifier(Modifier::BOLD),
                    )]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.border))
                    .border_type(BorderType::Plain),
            )
            .highlight_style(selected_style(&self.theme))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut state);

        let hint_text = if self.model_selection_required {
            format!("search: {}  · enter select  · model required", query)
        } else {
            format!("search: {}  · enter select  esc close", query)
        };
        let hint = Paragraph::new(hint_text)
            .alignment(Alignment::Right)
            .style(Style::default().fg(self.theme.muted));
        frame.render_widget(
            hint,
            Rect {
                x: area.x + 1,
                y: area.y + area.height.saturating_sub(2),
                width: area.width.saturating_sub(2),
                height: 1,
            },
        );
    }

    fn draw_provider_picker(&self, frame: &mut ratatui::Frame<'_>, selected: usize) {
        let providers = available_providers(&self.config);
        let width = frame.area().width.saturating_sub(4).min(112).max(72);
        let height = (providers.len().min(12) as u16)
            .saturating_mul(3)
            .saturating_add(8)
            .min(frame.area().height.saturating_sub(4).max(16));
        let area = centered_rect(width, height, frame.area());
        frame.render_widget(Clear, area);

        let items = providers
            .iter()
            .map(|provider| {
                let action = provider_action_label(provider, &self.config);
                let model_count = provider.models.len().max(1);
                let model_line = if model_count > 1 {
                    format!("{model_count} available models  ·  choose after connecting")
                } else {
                    "choose model after connecting".to_string()
                };
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            provider.label.clone(),
                            Style::default()
                                .fg(self.theme.text)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(action, Style::default().fg(self.theme.muted)),
                    ]),
                    Line::from(Span::styled(
                        model_line,
                        Style::default().fg(self.theme.muted),
                    )),
                    Line::from(Span::styled(
                        provider_hint(provider),
                        Style::default().fg(self.theme.muted),
                    )),
                ])
            })
            .collect::<Vec<_>>();

        let mut state = ListState::default();
        state.select(Some(selected.min(providers.len().saturating_sub(1))));
        let list = List::new(items)
            .block(
                Block::default()
                    .title(Line::from(vec![Span::styled(
                        "Choose a provider",
                        Style::default()
                            .fg(self.theme.accent)
                            .add_modifier(Modifier::BOLD),
                    )]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.border))
                    .border_type(BorderType::Plain),
            )
            .highlight_style(selected_style(&self.theme))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut state);

        let hint = Paragraph::new("enter choose  ·  /login connects OpenRouter  ·  esc close")
            .alignment(Alignment::Right)
            .style(Style::default().fg(self.theme.muted));
        frame.render_widget(
            hint,
            Rect {
                x: area.x + 1,
                y: area.y + area.height.saturating_sub(2),
                width: area.width.saturating_sub(2),
                height: 1,
            },
        );
    }

    fn draw_model_cost_warning(
        &self,
        frame: &mut ratatui::Frame<'_>,
        selected: usize,
        model: &str,
        warning: &str,
        recommendation: &str,
    ) {
        let width = frame.area().width.saturating_sub(8).min(92).max(60);
        let area = centered_rect(width, 11, frame.area());
        frame.render_widget(Clear, area);

        let proceed_style = if selected == 0 {
            Style::default()
                .fg(Color::Black)
                .bg(self.theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.theme.text)
        };
        let cancel_style = if selected == 1 {
            Style::default()
                .fg(Color::Black)
                .bg(self.theme.accent)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.theme.text)
        };
        let lines = vec![
            Line::from(Span::styled(
                "Beta cost warning",
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                model.to_string(),
                Style::default().fg(self.theme.text),
            )),
            Line::from(Span::styled(
                warning.to_string(),
                Style::default().fg(self.theme.text),
            )),
            Line::from(Span::styled(
                recommendation.to_string(),
                Style::default().fg(self.theme.muted),
            )),
            Line::from(""),
            Line::from(vec![
                Span::styled(" Proceed anyway ", proceed_style),
                Span::raw("  "),
                Span::styled(" Cancel ", cancel_style),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "Use Left/Right to choose, Enter to confirm, Esc to cancel.",
                Style::default().fg(self.theme.muted),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.danger))
                    .border_type(BorderType::Plain),
            ),
            area,
        );
    }

    fn filtered_model_indexes(&self, query: &str) -> Vec<usize> {
        let q = query.trim().to_ascii_lowercase();
        if q.is_empty() {
            return (0..self.models.len()).collect();
        }
        self.models
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.id.to_ascii_lowercase().contains(&q)
                    || m.title().to_ascii_lowercase().contains(&q)
                    || m.subtitle().to_ascii_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn draw_file_picker(
        &self,
        frame: &mut ratatui::Frame<'_>,
        query: &str,
        directory: &Path,
        entries: &[FilePickerEntry],
        selected: usize,
        scroll: usize,
    ) {
        let width = frame.area().width.saturating_sub(10).min(100).max(50);
        let height = (entries.len().min(10) as u16).saturating_add(5);
        let area = centered_rect(width, height, frame.area());
        frame.render_widget(Clear, area);

        let visible = area.height.saturating_sub(4).max(1) as usize;
        let start = scroll.min(entries.len().saturating_sub(1));
        let end = (start + visible).min(entries.len());
        let slice = &entries[start..end];

        let items = slice
            .iter()
            .map(|entry| {
                let color = match entry.kind {
                    FilePickerEntryKind::Parent | FilePickerEntryKind::Directory => {
                        self.theme.accent
                    }
                    FilePickerEntryKind::File => self.theme.text,
                    FilePickerEntryKind::Skill => self.theme.success,
                    FilePickerEntryKind::McpServer => self.theme.emphasis,
                };
                ListItem::new(Line::from(Span::styled(
                    entry.label.clone(),
                    Style::default().fg(color),
                )))
            })
            .collect::<Vec<_>>();

        let mut state = ListState::default();
        state.select(Some(
            selected
                .saturating_sub(start)
                .min(slice.len().saturating_sub(1)),
        ));

        let list = List::new(items)
            .block(
                Block::default()
                    .title(Line::from(vec![
                        Span::styled(
                            "mention",
                            Style::default()
                                .fg(self.theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(format!("@{query}"), Style::default().fg(self.theme.muted)),
                        Span::raw("  "),
                        Span::styled(
                            display_relative_path(&self.paths.root_dir, directory),
                            Style::default().fg(self.theme.muted),
                        ),
                    ]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.border))
                    .border_type(BorderType::Plain),
            )
            .highlight_style(selected_style(&self.theme))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut state);

        let hint = Paragraph::new("Enter open/select  ·  Esc close")
            .alignment(Alignment::Center)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_widget(
            hint,
            Rect {
                x: area.x + 1,
                y: area.y + area.height.saturating_sub(2),
                width: area.width.saturating_sub(2),
                height: 1,
            },
        );
    }

    fn draw_mcp_panel(
        &self,
        frame: &mut ratatui::Frame<'_>,
        servers: &[McpServerConfig],
        tools: &[McpToolSpec],
        errors: &[String],
        loading: bool,
        scroll: usize,
    ) {
        let width = frame.area().width.saturating_sub(12).min(92).max(48);
        let height = frame.area().height.saturating_sub(8).min(20).max(10);
        let area = centered_rect(width, height, frame.area());
        frame.render_widget(Clear, area);

        let mut lines = Vec::new();
        if servers.is_empty() {
            lines.push(Line::from(Span::styled(
                "No MCP servers",
                Style::default().fg(self.theme.muted),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "Servers",
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            for server in servers {
                lines.push(Line::from(vec![
                    Span::styled("- ", Style::default().fg(self.theme.border)),
                    Span::styled(server.name.clone(), Style::default().fg(self.theme.text)),
                    Span::raw("  "),
                    Span::styled(
                        server.command.clone(),
                        Style::default().fg(self.theme.muted),
                    ),
                ]));
                if !server.args.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled("  args: ", Style::default().fg(self.theme.border)),
                        Span::styled(server.args.join(" "), Style::default().fg(self.theme.muted)),
                    ]));
                }
            }
            if loading {
                lines.push(Line::from(Span::styled(
                    "discovering tools...",
                    Style::default().fg(self.theme.muted),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Tools",
                Style::default()
                    .fg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )));
            if tools.is_empty() {
                lines.push(Line::from(Span::styled(
                    "No MCP tools discovered",
                    Style::default().fg(self.theme.muted),
                )));
            } else {
                for tool in tools {
                    lines.push(Line::from(vec![
                        Span::styled("- ", Style::default().fg(self.theme.border)),
                        Span::styled(
                            tool.function_name.clone(),
                            Style::default().fg(self.theme.success),
                        ),
                        Span::raw("  "),
                        Span::styled(
                            format!("{}::{}", tool.server_name, tool.tool_name),
                            Style::default().fg(self.theme.muted),
                        ),
                    ]));
                }
            }
            if !errors.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Warnings",
                    Style::default()
                        .fg(self.theme.danger)
                        .add_modifier(Modifier::BOLD),
                )));
                for error in errors.iter().take(6) {
                    lines.push(Line::from(vec![
                        Span::styled("- ", Style::default().fg(self.theme.border)),
                        Span::styled(error.clone(), Style::default().fg(self.theme.muted)),
                    ]));
                }
            }
        }

        let visible_rows = area.height.saturating_sub(4).max(1) as usize;
        let max_scroll = lines.len().saturating_sub(visible_rows);
        let scroll = scroll.min(max_scroll);
        let visible = lines
            .into_iter()
            .skip(scroll)
            .take(visible_rows)
            .collect::<Vec<_>>();

        let title = Line::from(vec![Span::styled(
            if loading { "mcp loading" } else { "mcp" },
            Style::default()
                .fg(self.theme.accent)
                .add_modifier(Modifier::BOLD),
        )]);
        frame.render_widget(
            Paragraph::new(visible)
                .block(
                    Block::default()
                        .title(title)
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(self.theme.border))
                        .border_type(BorderType::Plain),
                )
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_permission_picker(&self, frame: &mut ratatui::Frame<'_>, selected: usize) {
        let modes = permission_modes();
        let width = frame.area().width.saturating_sub(12).min(76).max(46);
        let height = (modes.len() as u16).saturating_mul(2).saturating_add(4);
        let area = centered_rect(width, height, frame.area());
        frame.render_widget(Clear, area);

        let items = modes
            .iter()
            .map(|mode| {
                let warning = if *mode == PermissionMode::FullAccess {
                    "Not recommended. Host commands are unrestricted."
                } else {
                    mode.description()
                };
                ListItem::new(vec![
                    Line::from(Span::styled(
                        mode.title(),
                        Style::default()
                            .fg(if *mode == PermissionMode::FullAccess {
                                self.theme.danger
                            } else {
                                self.theme.text
                            })
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        warning.to_string(),
                        Style::default().fg(self.theme.muted),
                    )),
                ])
            })
            .collect::<Vec<_>>();

        let mut state = ListState::default();
        state.select(Some(selected.min(modes.len().saturating_sub(1))));

        let list = List::new(items)
            .block(
                Block::default()
                    .title(Line::from(vec![Span::styled(
                        "permissions",
                        Style::default()
                            .fg(self.theme.accent)
                            .add_modifier(Modifier::BOLD),
                    )]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.border))
                    .border_type(BorderType::Plain),
            )
            .highlight_style(selected_style(&self.theme))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut state);

        let hint = Paragraph::new("enter apply  esc close")
            .alignment(Alignment::Right)
            .style(Style::default().fg(self.theme.muted));
        frame.render_widget(
            hint,
            Rect {
                x: area.x + 1,
                y: area.y + area.height.saturating_sub(2),
                width: area.width.saturating_sub(2),
                height: 1,
            },
        );
    }

    fn insert_newline(&mut self) {
        self.input.insert(self.cursor, '\n');
        self.cursor += '\n'.len_utf8();
        self.ensure_input_cursor_visible();
    }

    fn scroll_feed(&mut self, delta: i32) {
        if self.messages.is_empty() {
            return;
        }
        if delta.is_negative() {
            self.follow_latest = false;
        }
        let base = if self.follow_latest {
            self.feed_max_scroll
        } else {
            self.feed_scroll
        };
        let next = if delta < 0 {
            base.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            base.saturating_add(delta as usize)
                .min(self.feed_max_scroll)
        };
        self.feed_scroll = next;
        if self.feed_scroll >= self.feed_max_scroll {
            self.follow_latest = true;
        }
    }

    fn scroll_feed_to_bottom(&mut self) {
        self.feed_scroll = self.feed_max_scroll;
        self.follow_latest = true;
    }

    fn copy_transcript_to_clipboard(&mut self) {
        let transcript = render_transcript_text(&self.messages);
        if transcript.trim().is_empty() {
            return;
        }
        match write_osc52_clipboard(&transcript) {
            Ok(()) => {
                self.status = "copied transcript".to_string();
            }
            Err(err) => {
                self.push_toast("Copy failed", &err);
            }
        }
    }

    fn current_model(&self) -> String {
        if self.config.model.trim().is_empty() {
            return "choose model".to_string();
        }
        self.models
            .get(self.selected_model)
            .map(|model| model.id.clone())
            .unwrap_or_else(|| self.config.model.clone())
    }

    fn current_model_info(&self) -> Option<&ModelInfo> {
        self.models.get(self.selected_model)
    }

    fn current_model_warning(&self) -> Option<(String, String, String)> {
        let model = self.current_model_info()?;
        if !model.is_large_or_premium() {
            return None;
        }
        let mut reasons = Vec::new();
        if model.id.to_ascii_lowercase().contains("opus")
            || model
                .name
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains("opus")
        {
            reasons.push("Opus-class model".to_string());
        }
        if let Some(context_window) = model.context_window.filter(|window| *window >= 500_000) {
            reasons.push(format!("{} context", compact_number(context_window)));
        }
        if let Some(price) = model.price_label() {
            reasons.push(price);
        }
        if reasons.is_empty() {
            reasons.push("large or premium model".to_string());
        }
        let warning = format!(
            "Wire CLI is still in beta. Large or premium models are not recommended yet: {}.",
            reasons.join(", ")
        );
        let recommendation = self
            .recommended_smaller_models(&model.id)
            .map(|models| format!("Recommended smaller models: {models}."))
            .unwrap_or_else(|| {
                "Recommended: switch to a smaller or cheaper model for day-to-day work.".to_string()
            });
        Some((model.id.clone(), warning, recommendation))
    }

    fn recommended_smaller_models(&self, current_id: &str) -> Option<String> {
        let mut scored = self
            .models
            .iter()
            .filter(|model| model.id != current_id && !model.is_large_or_premium())
            .map(|model| (small_model_score(model), model.id.clone()))
            .collect::<Vec<_>>();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let values = scored
            .into_iter()
            .map(|(_, id)| id)
            .take(3)
            .collect::<Vec<_>>();
        if values.is_empty() {
            None
        } else {
            Some(values.join(", "))
        }
    }

    fn clear_prompt_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.input_scroll = 0;
    }

    fn prompt_height(&self) -> u16 {
        let line_count = self.input.lines().count().max(1) as u16;
        line_count.saturating_add(4).clamp(6, 14)
    }

    fn input_view_rows(&self) -> usize {
        self.prompt_height().saturating_sub(2).max(1) as usize
    }

    fn input_line_count(&self) -> usize {
        self.input.lines().count().max(1)
    }

    fn scroll_input(&mut self, delta: i32) {
        let max_scroll = self
            .input_line_count()
            .saturating_sub(self.input_view_rows());
        let next = if delta.is_negative() {
            self.input_scroll
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.input_scroll
                .saturating_add(delta as usize)
                .min(max_scroll)
        };
        self.input_scroll = next;
    }

    fn ensure_input_cursor_visible(&mut self) {
        let (cursor_row, _) = cursor_position(&self.input, self.cursor);
        let rows = self.input_view_rows();
        if cursor_row < self.input_scroll {
            self.input_scroll = cursor_row;
        } else if cursor_row >= self.input_scroll.saturating_add(rows) {
            self.input_scroll = cursor_row.saturating_add(1).saturating_sub(rows);
        }
        let max_scroll = self.input_line_count().saturating_sub(rows);
        self.input_scroll = self.input_scroll.min(max_scroll);
    }

    fn mouse_in_prompt(&self, row: u16) -> bool {
        crossterm::terminal::size()
            .map(|(_, height)| row >= height.saturating_sub(self.prompt_height()))
            .unwrap_or(false)
    }

    fn start_model_refresh(&mut self, notice: &str) {
        self.models_loading = true;
        self.model_rx = Some(spawn_model_loader(self.config.clone()));
        self.startup_notice = Some(notice.to_string());
    }

    fn refresh_models_from_backend(&mut self) {
        match load_models(&self.config) {
            Ok(models) if !models.is_empty() => {
                let current = self.config.model.clone();
                self.models = models;
                self.selected_model =
                    if self.model_selection_required || self.config.requires_model_selection() {
                        0
                    } else {
                        self.models
                            .iter()
                            .position(|model| model.id == current)
                            .or_else(|| {
                                self.models
                                    .iter()
                                    .position(|model| model.id == self.config.model)
                            })
                            .unwrap_or(0)
                    };
                self.backend_health.alive = true;
                self.backend_health.message = "backend healthy".to_string();
                self.backend_health.checked_at = Instant::now();
                self.startup_notice = None;
            }
            Ok(_) => {
                self.backend_health.alive = false;
                self.backend_health.message = "backend returned no models".to_string();
                self.backend_health.checked_at = Instant::now();
            }
            Err(err) => {
                self.backend_health.alive = false;
                self.backend_health.message = err;
                self.backend_health.checked_at = Instant::now();
            }
        }
    }

    fn cycle_model(&mut self, step: isize) {
        if self.model_selection_required || self.config.requires_model_selection() {
            self.overlay = Overlay::ModelPicker {
                selected: self.selected_model.min(self.models.len().saturating_sub(1)),
                scroll: 0,
                query: String::new(),
            };
            self.status = "choose a model".to_string();
            return;
        }
        if self.models.is_empty() {
            return;
        }
        let len = self.models.len() as isize;
        let next = (self.selected_model as isize + step).rem_euclid(len);
        self.selected_model = next as usize;
        self.config.model = self.current_model();
        let _ = self.config.save(&self.paths);
        let _ = save_global_model_status(&self.config);
    }

    fn current_permission_index(&self) -> usize {
        permission_modes()
            .iter()
            .position(|mode| *mode == self.config.permission_mode)
            .unwrap_or(0)
    }

    fn set_permission_mode(&mut self, mode: PermissionMode) {
        self.config.permission_mode = mode;
        self.overlay = Overlay::None;
        let save_result = self.config.save(&self.paths);
        let content = match save_result {
            Ok(()) => format!("{}\n{}", mode.title(), mode.description()),
            Err(err) => format!(
                "{}\n{}\n\nConfig save failed: {err}",
                mode.title(),
                mode.description()
            ),
        };
        self.messages.push(ChatMessage {
            role: MessageRole::System,
            title: Some("permissions".to_string()),
            content,
        });
        self.status = format!("permissions {}", mode.as_str());
        self.scroll_feed_to_bottom();
    }

    fn current_provider_index(&self) -> usize {
        available_providers(&self.config)
            .iter()
            .position(|provider| provider.id == self.config.provider)
            .unwrap_or(0)
    }

    fn select_provider(&mut self, provider_id: &str) {
        match apply_provider_preset(&mut self.config, provider_id) {
            Ok(profile) => {
                self.config.model.clear();
                self.model_selection_required = true;
                let save_result = self.config.save(&self.paths);
                self.login_required = self.config.requires_login();
                self.models = provider_models(&profile, "");
                self.selected_model = 0;
                self.overlay = Overlay::None;
                if provider_uses_openrouter_pkce(&profile.id) {
                    if let Err(err) = save_result {
                        self.push_toast("Config save failed", &err);
                    }
                    self.status = "connecting OpenRouter".to_string();
                    self.start_wire_login_flow();
                    return;
                } else {
                    self.health_rx = empty_health_rx();
                    self.model_rx = None;
                    self.models_loading = false;
                    self.force_model_picker_after_load = false;
                    if self.login_required {
                        self.push_toast(
                            "Provider key required",
                            "Set this provider API key before choosing a model.",
                        );
                    } else {
                        self.overlay = Overlay::ModelPicker {
                            selected: self.selected_model,
                            scroll: 0,
                            query: String::new(),
                        };
                    }
                }
                if let Err(err) = save_result {
                    self.push_toast("Config save failed", &err);
                }
                self.push_toast("Provider selected", &profile.label);
                self.status = format!("provider {}", profile.id);
            }
            Err(err) => self.push_toast("Provider error", &err),
        }
    }

    fn current_model_supports_vision(&self) -> bool {
        self.models
            .get(self.selected_model)
            .map(|model| {
                model
                    .capabilities
                    .iter()
                    .any(|capability| capability == "vision")
            })
            .unwrap_or(false)
    }

    fn handle_paste(&mut self, data: String) {
        match save_pasted_image(&data, self.pasted_images.len() + 1) {
            Ok(Some(pasted)) => {
                let marker = format!("[Pasted Image #{}]", pasted.index);
                self.pasted_images.push(pasted);
                self.insert_text_at_cursor(&marker);
            }
            Ok(None) => {
                let content = data.trim_matches('\0').to_string();
                if !content.is_empty() {
                    let index = self.pasted_contents.len() + 1;
                    let marker = format!("[Pasted Content #{}]", index);
                    self.pasted_contents.push(PastedContent { index, content });
                    self.insert_text_at_cursor(&marker);
                }
            }
            Err(err) => {
                self.push_toast("Paste error", &err);
            }
        }
        self.ensure_input_cursor_visible();
        self.sync_file_picker_overlay();
    }

    fn sync_file_picker_overlay(&mut self) {
        if self.pending_prompt.is_some() {
            return;
        }
        let Some((_, _, query)) = self.active_attachment_token() else {
            if matches!(self.overlay, Overlay::FilePicker { .. }) {
                self.overlay = Overlay::None;
            }
            return;
        };
        let directory = match &self.overlay {
            Overlay::FilePicker { directory, .. } => directory.clone(),
            _ => self.paths.root_dir.clone(),
        };
        let entries =
            list_mention_picker_entries(&self.paths, &directory, &query).unwrap_or_default();
        self.overlay = Overlay::FilePicker {
            query,
            directory,
            entries,
            selected: 0,
            scroll: 0,
        };
    }

    fn active_attachment_token(&self) -> Option<(usize, usize, String)> {
        active_attachment_token(&self.input, self.cursor)
    }

    fn insert_text_at_cursor(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.ensure_input_cursor_visible();
    }

    fn delete_prev_char(&mut self) -> bool {
        if self.cursor == 0 || self.cursor > self.input.len() {
            return false;
        }
        let prev = prev_char_boundary(&self.input, self.cursor);
        if prev < self.cursor && prev < self.input.len() {
            self.input.drain(prev..self.cursor);
            self.cursor = prev;
            return true;
        }
        false
    }

    fn delete_next_char(&mut self) -> bool {
        if self.cursor >= self.input.len() {
            return false;
        }
        let next = next_char_boundary(&self.input, self.cursor);
        if next > self.cursor {
            self.input.drain(self.cursor..next);
            return true;
        }
        false
    }

    fn replace_active_attachment_token(&mut self, path: &Path) -> Result<(), String> {
        if self.active_attachment_token().is_none() {
            return Ok(());
        }
        let root =
            fs::canonicalize(&self.paths.root_dir).unwrap_or_else(|_| self.paths.root_dir.clone());
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let rel = path
            .strip_prefix(&root)
            .map_err(|e| e.to_string())?
            .display()
            .to_string();
        let replacement = format!("@{}", rel.replace(' ', "\\ "));
        self.replace_active_attachment_token_with(&replacement)
    }

    fn replace_active_attachment_token_with(&mut self, replacement: &str) -> Result<(), String> {
        let Some((start, end, _query)) = self.active_attachment_token() else {
            return Ok(());
        };
        self.input.replace_range(start..end, &replacement);
        self.cursor = start + replacement.len();
        Ok(())
    }

    fn status_report(&self) -> String {
        let mut lines = vec![
            format!("provider: {}", self.config.provider_status_label()),
            format!("protocol: {}", self.config.protocol.as_str()),
            format!("account: {}", self.config.account_summary()),
            format!("base_url: {}", self.config.base_url),
            format!("model: {}", self.current_model()),
            format!(
                "provider key: {}",
                if self.config.api_key.is_some() {
                    "configured"
                } else {
                    "not configured"
                }
            ),
            format!("permissions: {}", self.config.permission_mode.title()),
            format!("input tokens: {}", self.usage.input_tokens),
            format!("output tokens: {}", self.usage.output_tokens),
            format!("total tokens: {}", self.usage.total_tokens),
        ];
        if let Some(context_window) = self
            .models
            .get(self.selected_model)
            .and_then(|model| model.context_window)
        {
            let remaining = context_window.saturating_sub(self.usage.total_tokens);
            lines.push(format!(
                "context window: {}",
                compact_number(context_window)
            ));
            lines.push(format!("context remaining: {}", compact_number(remaining)));
            if let Some(max_completion_tokens) = self
                .models
                .get(self.selected_model)
                .and_then(|model| model.max_completion_tokens)
            {
                lines.push(format!(
                    "context max completion: {}",
                    compact_number(max_completion_tokens)
                ));
            }
        } else {
            lines.push("context window: unknown".to_string());
        }
        if let Some((_, warning, recommendation)) = self.current_model_warning() {
            lines.push(format!("beta warning: {warning}"));
            lines.push(recommendation);
        }
        lines.join("\n")
    }

    fn build_prompt_plan(
        &self,
        prompt: &str,
        pinned: &BTreeMap<String, PathBuf>,
    ) -> Result<PromptPlanOutcome, String> {
        let mut display_prompt = prompt.to_string();
        let mut model_prompt = prompt.to_string();
        let mut attachments = Vec::new();
        let mut images = Vec::new();

        for token in prompt.split_whitespace() {
            if let Some(query) = token.strip_prefix('@') {
                let query = sanitize_attachment_query(query);
                if query.is_empty() {
                    continue;
                }

                if let Some(mention) = resolve_virtual_mention(&self.paths, &query)? {
                    display_prompt = display_prompt.replace(token, &mention.placeholder);
                    model_prompt = model_prompt.replace(token, &mention.placeholder);
                    model_prompt.push_str("\n\nMentioned context:\n");
                    model_prompt.push_str(&mention.content);
                    if !mention.content.ends_with('\n') {
                        model_prompt.push('\n');
                    }
                    continue;
                }

                if let Some(path) = pinned.get(&query) {
                    let attachment = load_attachment(&self.paths.root_dir, path.clone())?;
                    let placeholder = attachment_placeholder(&attachment);
                    display_prompt = display_prompt.replace(token, &placeholder);
                    model_prompt = model_prompt.replace(token, &placeholder);
                    attachments.push(attachment);
                    continue;
                }

                let matches = find_attachment_matches(&self.paths.root_dir, &query, 24)?;
                match matches.len() {
                    0 => {}
                    1 => {
                        let attachment = load_attachment(&self.paths.root_dir, matches[0].clone())?;
                        let placeholder = attachment_placeholder(&attachment);
                        display_prompt = display_prompt.replace(token, &placeholder);
                        model_prompt = model_prompt.replace(token, &placeholder);
                        attachments.push(attachment);
                    }
                    _ => {
                        let directory = self.paths.root_dir.clone();
                        let entries = list_mention_picker_entries(&self.paths, &directory, &query)?;
                        return Ok(PromptPlanOutcome::NeedPicker(NeedPicker {
                            state: FilePickerState {
                                original_prompt: prompt.to_string(),
                                pinned: pinned.clone(),
                            },
                            query,
                            directory,
                            entries,
                        }));
                    }
                }
            }
        }

        for pasted in &self.pasted_images {
            let marker = format!("[Pasted Image #{}]", pasted.index);
            if display_prompt.contains(&marker) || model_prompt.contains(&marker) {
                model_prompt.push_str("\n\nAttached image:\n");
                model_prompt.push_str(&marker);
                model_prompt.push_str("\nPath: ");
                model_prompt.push_str(&pasted.path.display().to_string());
                model_prompt.push('\n');
                if self.current_model_supports_vision() {
                    images.push(pasted.prompt_image.clone());
                }
            }
        }

        for pasted in &self.pasted_contents {
            let marker = format!("[Pasted Content #{}]", pasted.index);
            if display_prompt.contains(&marker) || model_prompt.contains(&marker) {
                model_prompt.push_str("\n\nPasted content:\n");
                model_prompt.push_str(&marker);
                model_prompt.push_str("\n```text\n");
                model_prompt.push_str(&pasted.content);
                model_prompt.push_str("\n```\n");
            }
        }

        if !attachments.is_empty() {
            model_prompt.push_str("\n\nAttached files:\n");
            for attachment in attachments {
                match &attachment.kind {
                    AttachmentKind::Text(content) => {
                        model_prompt.push_str("\n[File: ");
                        model_prompt.push_str(&attachment.label);
                        model_prompt.push_str("]\n```text\n");
                        model_prompt.push_str(content);
                        model_prompt.push_str("\n```\n");
                    }
                    AttachmentKind::Image(image) => {
                        model_prompt.push_str("\n[Arquivo]\nPath: ");
                        model_prompt.push_str(&attachment.path.display().to_string());
                        model_prompt.push('\n');
                        if self.current_model_supports_vision() {
                            images.push(image.clone());
                        }
                    }
                }
            }
        }

        Ok(PromptPlanOutcome::Ready(PromptPlan {
            display_prompt,
            model_prompt,
            images,
        }))
    }
}

struct NeedPicker {
    state: FilePickerState,
    query: String,
    directory: PathBuf,
    entries: Vec<FilePickerEntry>,
}

enum PromptPlanOutcome {
    Ready(PromptPlan),
    NeedPicker(NeedPicker),
}

struct TuiObserver {
    tx: mpsc::Sender<UiEvent>,
}

impl responses_agent::AgentObserver for TuiObserver {
    fn on_event(&mut self, event: responses_agent::AgentEvent<'_>) {
        match event {
            responses_agent::AgentEvent::SessionBound(session_id) => {
                let _ = self.tx.send(UiEvent::SessionBound(session_id.to_string()));
            }
            responses_agent::AgentEvent::TextDelta(delta) => {
                let _ = self.tx.send(UiEvent::Delta(delta.to_string()));
            }
            responses_agent::AgentEvent::ToolCallDelta {
                name,
                arguments_delta,
                ..
            } => {
                let _ = self.tx.send(UiEvent::ToolDelta {
                    name: name.map(|name| name.to_string()),
                    arguments_delta: arguments_delta.to_string(),
                });
            }
            responses_agent::AgentEvent::Status(status) => {
                let _ = self.tx.send(UiEvent::Status(status.to_string()));
            }
            responses_agent::AgentEvent::ToolCallStart { name, summary, .. } => {
                let _ = self.tx.send(UiEvent::ToolStart {
                    name: name.to_string(),
                    summary: summary.to_string(),
                });
            }
            responses_agent::AgentEvent::ToolCallResult { name, output, .. } => {
                let _ = self.tx.send(UiEvent::ToolResult {
                    name: name.to_string(),
                    output: output.to_string(),
                });
            }
            responses_agent::AgentEvent::Usage(usage) => {
                let _ = self.tx.send(UiEvent::Usage(usage));
            }
        }
    }
}

fn load_models(config: &AppConfig) -> Result<Vec<ModelInfo>, String> {
    model_catalog::load_models_blocking(config, Duration::from_secs(4))
}

fn spawn_model_loader(config: AppConfig) -> mpsc::Receiver<ModelLoadEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let event = match load_models(&config) {
            Ok(models) => ModelLoadEvent::Loaded(models),
            Err(err) => ModelLoadEvent::Failed(err),
        };
        let _ = tx.send(event);
    });
    rx
}

fn empty_health_rx() -> mpsc::Receiver<BackendHealth> {
    let (_tx, rx) = mpsc::channel();
    rx
}

fn spawn_mcp_discovery(registry: McpRegistry) -> mpsc::Receiver<McpLoadEvent> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let report = registry.discover_tools_report();
        let _ = tx.send(McpLoadEvent {
            tools: report.tools,
            errors: report.errors,
        });
    });
    rx
}

fn fallback_model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        name: Some(id.to_string()),
        owned_by: None,
        context_window: None,
        max_completion_tokens: None,
        prompt_price_per_million: None,
        completion_price_per_million: None,
        capabilities: Vec::new(),
    }
}

fn provider_models(provider: &ProviderProfile, current_model: &str) -> Vec<ModelInfo> {
    let mut ids = provider.models.clone();
    if ids.is_empty() {
        ids.push(provider.default_model.clone());
    }
    if !current_model.trim().is_empty() && !ids.iter().any(|id| id == current_model) {
        ids.insert(0, current_model.to_string());
    }
    ids.into_iter()
        .filter(|id| !id.trim().is_empty())
        .map(|id| fallback_model(&id))
        .collect::<Vec<_>>()
}

fn health_base_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

fn provider_action_label(provider: &ProviderProfile, config: &AppConfig) -> String {
    if provider_uses_openrouter_pkce(&provider.id) {
        if config.provider == provider.id && config.has_api_key() {
            return "connected".to_string();
        }
        return "browser login".to_string();
    }
    if provider.api_key_env.is_some() || config.provider == provider.id && config.has_api_key() {
        if provider.id == config.provider && config.has_api_key() {
            "ready".to_string()
        } else {
            "configured key".to_string()
        }
    } else {
        "needs key".to_string()
    }
}

fn provider_hint(provider: &ProviderProfile) -> String {
    if provider_uses_openrouter_pkce(&provider.id) {
        return "secure browser login, no manual key paste".to_string();
    }
    if provider.notes.iter().any(|note| note.contains("custom")) {
        return "custom provider from config.toml".to_string();
    }
    if provider.protocol == ProviderProtocol::AnthropicMessages {
        return "native Claude API key support".to_string();
    }
    "Chat Completions compatible API key provider".to_string()
}

fn small_model_score(model: &ModelInfo) -> i32 {
    let text = format!(
        "{} {}",
        model.id.to_ascii_lowercase(),
        model
            .name
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase()
    );
    let mut score = 0;
    for marker in [
        "free", "mini", "flash", "haiku", "lite", "small", "nano", "deepseek", "qwen", "glm",
        "gemini",
    ] {
        if text.contains(marker) {
            score += 10;
        }
    }
    if text.contains("sonnet") {
        score += 4;
    }
    if text.contains("opus") {
        score -= 50;
    }
    if let Some(price) = model.prompt_price_per_million {
        if price <= 1.0 {
            score += 6;
        }
    }
    if let Some(price) = model.completion_price_per_million {
        if price <= 5.0 {
            score += 6;
        }
    }
    score
}

pub fn run_session_view_tui(
    paths: AppPaths,
    config: AppConfig,
    session_id: String,
) -> Result<(), String> {
    let store = SessionStore::new(&paths)?;
    let timeline = store.timeline(&paths.project_key, &session_id)?;
    let selected = store.resolve(&paths.project_key, Some(session_id.clone()))?;
    let theme = ThemeConfig::load_or_create(&paths.theme_file)?;
    let logo = load_logo(&paths);
    let model = config.model.clone();

    let mut app = App::new(
        paths,
        config,
        vec![fallback_model(&model)],
        logo,
        theme,
        None,
    );
    app.session_id = Some(session_id);
    app.status = selected
        .summary
        .clone()
        .unwrap_or_else(|| "untitled session".to_string());
    app.messages = timeline_to_chat_messages(&timeline);
    app.running = false;
    app.follow_latest = true;
    app.scroll_feed_to_bottom();
    app.run()
}

fn timeline_to_chat_messages(events: &[crate::session::TimelineEvent]) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    let mut pending_stream_recovery: Option<ChatMessage> = None;
    for event in events {
        match event.kind.as_str() {
            "message" => {
                let role = match event.role.as_deref() {
                    Some("assistant") => MessageRole::Assistant,
                    Some("tool") => MessageRole::Tool,
                    Some("system") | Some("developer") => continue,
                    _ => MessageRole::User,
                };
                if matches!(role, MessageRole::Assistant) {
                    pending_stream_recovery = None;
                }
                messages.push(ChatMessage {
                    role,
                    title: None,
                    content: event.content.clone().unwrap_or_default(),
                });
            }
            "command" => {
                let Some(command) = &event.command else {
                    continue;
                };
                let command_parts = command.split_whitespace().collect::<Vec<_>>();
                let is_tool_call = command_parts.first() == Some(&"tool.call");
                let is_verifier = command_parts.first() == Some(&"verifier.pipeline");
                let is_hook = command_parts.first() == Some(&"hook.event");
                let is_memory_suggestion = command_parts.first() == Some(&"memory.suggestion");
                let is_skill_suggestion = command_parts.first() == Some(&"skill.suggestion");
                if is_tool_call {
                    pending_stream_recovery = None;
                }
                if !is_tool_call
                    && !is_verifier
                    && !is_hook
                    && !is_memory_suggestion
                    && !is_skill_suggestion
                {
                    continue;
                }
                let tool_name = command_parts.get(1).copied().unwrap_or("tool");
                let mut text = String::new();
                if let Some(stdout) = &event.stdout {
                    if !stdout.trim().is_empty() {
                        text.push_str(stdout);
                    }
                }
                if let Some(stderr) = &event.stderr {
                    if !stderr.trim().is_empty() {
                        if !text.is_empty() {
                            text.push_str("\n\n");
                        }
                        text.push_str(stderr);
                    }
                }
                messages.push(ChatMessage {
                    role: MessageRole::Tool,
                    title: Some(if is_verifier {
                        "Verifier".to_string()
                    } else if is_hook {
                        "Hook".to_string()
                    } else if is_memory_suggestion {
                        "Memory Suggestion".to_string()
                    } else if is_skill_suggestion {
                        "Skill Suggestion".to_string()
                    } else {
                        tool_label(tool_name)
                    }),
                    content: text,
                });
            }
            "checkpoint" => {
                if let Some(message) = checkpoint_recovery_message(event) {
                    let is_terminal_checkpoint = event
                        .command
                        .as_deref()
                        .map(|phase| {
                            phase == "stream_error_checkpoint"
                                || phase == "provider_transport_checkpoint"
                        })
                        .unwrap_or(false);
                    if is_terminal_checkpoint {
                        pending_stream_recovery = None;
                        messages.push(message);
                    } else {
                        pending_stream_recovery = Some(message);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(message) = pending_stream_recovery {
        messages.push(message);
    }
    messages
}

fn checkpoint_recovery_message(event: &crate::session::TimelineEvent) -> Option<ChatMessage> {
    let phase = event.command.as_deref()?;
    if !matches!(
        phase,
        "stream_partial"
            | "tool_call_partial"
            | "stream_completed"
            | "stream_error_checkpoint"
            | "provider_transport_checkpoint"
    ) {
        return None;
    }
    let raw = event.content.as_deref()?;
    let value: Value = serde_json::from_str(raw).ok()?;
    let text = value
        .get("text_excerpt")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if !text.is_empty() {
        return Some(ChatMessage {
            role: MessageRole::Assistant,
            title: Some("Recovered stream".to_string()),
            content: text,
        });
    }
    let tools = value.get("tools").and_then(|value| value.as_array())?;
    let tool = tools.iter().find(|tool| {
        tool.get("name")
            .and_then(|value| value.as_str())
            .map(|name| !name.trim().is_empty())
            .unwrap_or(false)
            || tool
                .get("arguments_excerpt")
                .and_then(|value| value.as_str())
                .map(|arguments| !arguments.trim().is_empty())
                .unwrap_or(false)
    })?;
    let name = tool
        .get("name")
        .and_then(|value| value.as_str())
        .filter(|name| !name.trim().is_empty());
    let arguments = tool
        .get("arguments_excerpt")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let mut content = String::new();
    append_streaming_tool_delta(&mut content, name, arguments);
    Some(ChatMessage {
        role: MessageRole::Tool,
        title: Some(streaming_tool_title(name)),
        content,
    })
}

fn load_logo(paths: &AppPaths) -> Vec<String> {
    let logo_path = paths.root_dir.join("ascii.md");
    match fs::read_to_string(logo_path) {
        Ok(content) => content.lines().map(|line| line.to_string()).collect(),
        Err(_) => vec!["WIRE".to_string(), "CLI".to_string()],
    }
}

fn prompt_height() -> u16 {
    8
}

fn welcome_prompt_rect(area: Rect) -> Rect {
    let lower_half = Rect {
        x: area.x,
        y: area.y + area.height / 2,
        width: area.width,
        height: area.height.saturating_sub(area.height / 2),
    };
    centered_rect(
        area.width.saturating_sub(12).min(76),
        prompt_height(),
        lower_half,
    )
}

fn command_suggestions(input: &str) -> Vec<&'static str> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') {
        return Vec::new();
    }
    let commands = [
        "/login",
        "/providers",
        "/models",
        "/permissions",
        "/resume",
        "/share",
        "/mcp",
        "/status",
    ];
    if trimmed == "/" {
        return commands.to_vec();
    }
    commands
        .iter()
        .copied()
        .filter(|cmd| cmd.starts_with(trimmed))
        .collect()
}

fn cursor_position(input: &str, cursor: usize) -> (usize, usize) {
    let cursor = cursor.min(input.len());
    let prefix = input.get(..cursor).unwrap_or(input);
    let mut row = 0usize;
    let mut col = 0usize;
    for ch in prefix.chars() {
        if ch == '\n' {
            row += 1;
            col = 0;
        } else {
            col += 1;
        }
    }
    (row, col)
}

fn prev_char_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    if cursor == 0 {
        return 0;
    }
    let mut idx = cursor - 1;
    while !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn next_char_boundary(text: &str, cursor: usize) -> usize {
    let cursor = cursor.min(text.len());
    if cursor >= text.len() {
        return text.len();
    }
    let mut idx = cursor + 1;
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx.min(text.len())
}

fn compact_session_id(session_id: &str) -> String {
    if session_id.len() <= 12 {
        return session_id.to_string();
    }
    let prefix = &session_id[..6];
    let suffix = &session_id[session_id.len().saturating_sub(6)..];
    format!("{prefix}…{suffix}")
}

fn render_transcript_text(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        let title = message
            .title
            .clone()
            .unwrap_or_else(|| transcript_role_label(&message.role).to_string());
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&title);
        out.push_str("\n\n");
        out.push_str(message.content.trim());
    }
    out
}

fn transcript_role_label(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::User => "you",
        MessageRole::Assistant => "wire",
        MessageRole::Tool => "tool",
        MessageRole::System => "system",
        MessageRole::Queued => "queued",
        MessageRole::Shell => "shell",
    }
}

fn write_osc52_clipboard(text: &str) -> Result<(), String> {
    let encoded = base64_encode(text.as_bytes());
    let mut stdout = io::stdout();
    stdout
        .write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes())
        .map_err(|e| e.to_string())?;
    stdout.flush().map_err(|e| e.to_string())
}

fn spinner(elapsed: Duration) -> &'static str {
    const FRAMES: &[&str] = &["-", "/", "-", "|", "\\"];
    let index = ((elapsed.as_millis() / 120) as usize) % FRAMES.len();
    FRAMES[index]
}

fn elapsed_label(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    if minutes > 0 {
        format!("{minutes}m {seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn message_title(message: &ChatMessage, thinking: bool, elapsed: Duration) -> String {
    if thinking && matches!(message.role, MessageRole::Assistant) {
        return format!("Thinking ({} · esc to interrupt)", elapsed_label(elapsed));
    }
    match message.role {
        MessageRole::User => message.title.clone().unwrap_or_else(|| "you".to_string()),
        MessageRole::Assistant => message.title.clone().unwrap_or_else(|| "wire".to_string()),
        MessageRole::Tool => message.title.clone().unwrap_or_else(|| "tool".to_string()),
        MessageRole::System => message
            .title
            .clone()
            .unwrap_or_else(|| "system".to_string()),
        MessageRole::Queued => message
            .title
            .clone()
            .unwrap_or_else(|| "queued".to_string()),
        MessageRole::Shell => message.title.clone().unwrap_or_else(|| "shell".to_string()),
    }
}

fn selected_style(theme: &ThemeConfig) -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(theme.accent)
        .add_modifier(Modifier::BOLD)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToolTone {
    Error,
    Plan,
    Explore,
    Edit,
    Run,
    Memory,
    Skill,
    Mcp,
    Other,
}

fn card_color(role: &MessageRole, title: &str, theme: &ThemeConfig) -> Color {
    match role {
        MessageRole::User => theme.accent,
        MessageRole::Assistant => theme.assistant_text,
        MessageRole::Tool => match tool_tone_from_title(title) {
            ToolTone::Error => theme.danger,
            ToolTone::Plan => theme.emphasis,
            ToolTone::Explore => theme.tool_text,
            ToolTone::Edit => theme.success,
            ToolTone::Run => theme.accent,
            ToolTone::Memory => theme.emphasis,
            ToolTone::Skill => theme.success,
            ToolTone::Mcp => theme.accent,
            ToolTone::Other => theme.emphasis,
        },
        MessageRole::System if is_error_title(title) => theme.danger,
        MessageRole::System => theme.muted,
        MessageRole::Queued => theme.muted,
        MessageRole::Shell => theme.accent,
    }
}

fn body_color(role: &MessageRole, title: &str, theme: &ThemeConfig) -> Color {
    match role {
        MessageRole::User => theme.user_text,
        MessageRole::Assistant => theme.assistant_text,
        MessageRole::Tool => match tool_tone_from_title(title) {
            ToolTone::Error => theme.danger,
            ToolTone::Plan => theme.text,
            ToolTone::Explore => theme.tool_text,
            ToolTone::Edit => theme.tool_text,
            ToolTone::Run => theme.text,
            ToolTone::Memory => theme.text,
            ToolTone::Skill => theme.text,
            ToolTone::Mcp => theme.text,
            ToolTone::Other => theme.tool_text,
        },
        MessageRole::System if is_error_title(title) => theme.danger,
        MessageRole::System => theme.muted,
        MessageRole::Queued => theme.muted,
        MessageRole::Shell => theme.text,
    }
}

fn tool_tone_from_title(title: &str) -> ToolTone {
    let lower = title.trim().to_ascii_lowercase();
    if lower.starts_with("tool error")
        || lower.starts_with("lattice blocked")
        || lower.starts_with("watchdog stopped")
        || lower.contains(" failed")
    {
        return ToolTone::Error;
    }
    if is_plan_title(&lower) {
        return ToolTone::Plan;
    }
    if lower.starts_with("read ")
        || lower.starts_with("listed ")
        || lower.starts_with("searched ")
        || lower.starts_with("matched ")
        || lower.starts_with("opened ")
        || lower.starts_with("checking ")
    {
        return ToolTone::Explore;
    }
    if lower.starts_with("edited ") || lower.starts_with("editing ") {
        return ToolTone::Edit;
    }
    if lower.starts_with("ran ") || lower.starts_with("running ") {
        return ToolTone::Run;
    }
    if lower.starts_with("memory")
        || lower.starts_with("recall")
        || lower.starts_with("lab")
        || lower.contains("context recovered")
        || lower.contains("saved context")
    {
        return ToolTone::Memory;
    }
    if lower.starts_with("skill") {
        return ToolTone::Skill;
    }
    if lower.starts_with("mcp") {
        return ToolTone::Mcp;
    }
    ToolTone::Other
}

fn is_error_title(title: &str) -> bool {
    let lower = title.trim().to_ascii_lowercase();
    lower.starts_with("error") || lower.contains("failed")
}

fn tool_label(name: &str) -> String {
    if let Some(label) = mcp_tool_label(name) {
        return label;
    }
    match name {
        "shell" => "run".to_string(),
        "git" => "git".to_string(),
        "gh" => "github".to_string(),
        "subagent" => "subagent".to_string(),
        "plan" => "plan".to_string(),
        "update_plan" => "plan".to_string(),
        "hook" => "hook".to_string(),
        "review" => "review".to_string(),
        "apply_patch" => "patched".to_string(),
        "navigate" => "nav".to_string(),
        "list_dir" => "list".to_string(),
        "read_file" => "read".to_string(),
        "write_file" => "wrote".to_string(),
        "read_lines" => "lines".to_string(),
        "grep_lines" => "grep".to_string(),
        "head_lines" => "head".to_string(),
        "tail_lines" => "tail".to_string(),
        "glob_files" => "glob".to_string(),
        "replace_in_file" => "replace".to_string(),
        "delete_file" => "delete".to_string(),
        "copy_file" => "copy".to_string(),
        "move_file" => "move".to_string(),
        "search" => "search".to_string(),
        "remember" => "memory".to_string(),
        "recall" => "recall".to_string(),
        "lab_learn" => "lab".to_string(),
        "lab_recall" => "lab recall".to_string(),
        "session_remember" => "session memory".to_string(),
        "session_recall" => "session recall".to_string(),
        "mcp_list" => "mcp".to_string(),
        "skill_list" => "skills".to_string(),
        "skill_read" => "skill".to_string(),
        "skill_create" => "skill created".to_string(),
        other => other.to_string(),
    }
}

fn tool_activity_label(name: &str) -> String {
    if is_explore_tool(name) {
        return "exploring".to_string();
    }
    if is_edit_tool(name) {
        return "editing".to_string();
    }
    if matches!(name, "shell" | "git" | "gh") {
        return "running".to_string();
    }
    tool_label(name)
}

fn tool_start_title(name: &str, summary: &str) -> String {
    if is_plan_title(&tool_label(name)) {
        return "plan".to_string();
    }
    if is_explore_tool(name) {
        return explored_detail(name, Some(summary), "");
    }
    if is_edit_tool(name) {
        return if summary.trim().is_empty() {
            "Editing".to_string()
        } else {
            format!("Editing {}", truncate_display(summary, 72))
        };
    }
    if matches!(name, "shell" | "git" | "gh") {
        return if summary.trim().is_empty() {
            "Running command".to_string()
        } else {
            format!("Running {}", truncate_display(summary, 96))
        };
    }
    capitalize_title(&tool_label(name))
}

fn tool_result_title(name: &str, summary: Option<&str>, output: &str) -> String {
    if is_tool_error_output(output) {
        return format!("Tool error in {}", tool_label(name));
    }
    if is_plan_title(&tool_label(name)) {
        return "plan".to_string();
    }
    if is_explore_tool(name) {
        return explored_detail(name, summary, output);
    }
    if is_edit_tool(name) {
        return edited_title(summary, output);
    }
    if matches!(name, "shell" | "git" | "gh") {
        let command = summary
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("command");
        return format!("Ran {}", truncate_display(command, 96));
    }
    capitalize_title(&tool_label(name))
}

fn tool_result_body(name: &str, summary: Option<&str>, output: &str) -> String {
    if is_tool_error_output(output) {
        return output.trim().to_string();
    }
    if is_explore_tool(name) {
        return explored_result_body(name, summary, output);
    }
    if is_edit_tool(name) {
        return clean_edit_output(output);
    }
    if matches!(name, "shell" | "git" | "gh") {
        let trimmed = output.trim();
        if trimmed.is_empty() {
            return "└ no output".to_string();
        }
        return trimmed.to_string();
    }
    output.to_string()
}

fn is_tool_error_output(output: &str) -> bool {
    let lower = output.trim_start().to_ascii_lowercase();
    lower.starts_with("tool error in")
        || lower.starts_with("lattice blocked")
        || lower.starts_with("watchdog stopped")
}

fn explored_result_body(name: &str, summary: Option<&str>, output: &str) -> String {
    let mut lines = vec![format!("└ {}", visible_tool_intent(name, summary))];
    match name {
        "list_dir" => {
            lines.extend(compact_tool_output_preview(output, 18));
        }
        "search" | "grep_lines" | "glob_files" | "read_lines" | "head_lines" | "tail_lines" => {
            lines.extend(compact_tool_output_preview(output, 14));
        }
        "read_file" => {
            lines.push("  loaded file contents for analysis".to_string());
        }
        "navigate" => {
            lines.extend(compact_tool_output_preview(output, 3));
        }
        _ => {}
    }
    lines.join("\n")
}

fn visible_tool_intent(name: &str, summary: Option<&str>) -> String {
    let target = summary
        .filter(|value| !value.trim().is_empty())
        .map(|value| truncate_display(value, 96))
        .unwrap_or_else(|| ".".to_string());
    match name {
        "navigate" => format!("checking directory `{target}`"),
        "list_dir" => format!("listing `{target}` to choose the next file or folder"),
        "read_file" => format!("reading `{target}` to inspect exact source"),
        "read_lines" | "head_lines" | "tail_lines" => {
            format!("reading focused lines from `{target}`")
        }
        "grep_lines" | "search" => format!("searching `{target}` for matching evidence"),
        "glob_files" => format!("matching files with `{target}`"),
        "apply_patch" | "write_file" | "replace_in_file" | "delete_file" | "copy_file"
        | "move_file" => format!("editing `{target}` and preserving unrelated changes"),
        "shell" | "git" | "gh" => format!("running `{target}` for local evidence"),
        "lab_learn" => format!("saving learned preference `{target}`"),
        "lab_recall" => format!("checking Lab preferences for `{target}`"),
        "skill_list" => "checking available local skills".to_string(),
        "skill_read" => format!("reading skill `{target}` before applying it"),
        "mcp_list" => "checking configured MCP servers".to_string(),
        _ => format!("inspecting `{target}`"),
    }
}

fn compact_tool_output_preview(output: &str, max_lines: usize) -> Vec<String> {
    let mut skipped = 0usize;
    let mut lines = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty()
            || trimmed == "Listed"
            || trimmed == "Searched"
            || trimmed == "```text"
            || trimmed == "```"
        {
            continue;
        }
        if lines.len() < max_lines {
            lines.push(format!("  {trimmed}"));
        } else {
            skipped = skipped.saturating_add(1);
        }
    }
    if skipped > 0 {
        lines.push(format!("  … {skipped} more lines"));
    }
    lines
}

fn is_explore_tool(name: &str) -> bool {
    matches!(
        name,
        "navigate"
            | "list_dir"
            | "read_file"
            | "read_lines"
            | "grep_lines"
            | "head_lines"
            | "tail_lines"
            | "glob_files"
            | "search"
    )
}

fn is_edit_tool(name: &str) -> bool {
    matches!(
        name,
        "apply_patch"
            | "write_file"
            | "replace_in_file"
            | "delete_file"
            | "copy_file"
            | "move_file"
    )
}

fn explored_detail(name: &str, summary: Option<&str>, output: &str) -> String {
    let target = summary
        .filter(|value| !value.trim().is_empty())
        .map(|value| truncate_display(value, 96))
        .unwrap_or_else(|| infer_explored_target(output));
    let verb = match name {
        "navigate" => "Opened",
        "list_dir" => "Listed",
        "read_file" | "read_lines" | "head_lines" | "tail_lines" => "Read",
        "grep_lines" | "search" => "Searched",
        "glob_files" => "Matched",
        _ => "Explored",
    };
    if target.is_empty() {
        verb.to_string()
    } else {
        format!("{verb} {target}")
    }
}

fn infer_explored_target(output: &str) -> String {
    output
        .lines()
        .find_map(|line| {
            line.strip_prefix("Read ")
                .or_else(|| line.strip_prefix("Searched"))
                .map(|value| value.trim().to_string())
        })
        .unwrap_or_default()
}

fn edited_title(summary: Option<&str>, output: &str) -> String {
    let stats = diff_stats(output);
    let target = stats
        .first_file
        .or_else(|| summary.map(|value| value.to_string()))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "files".to_string());
    let target = if stats.file_count > 1 {
        format!("{} files", stats.file_count)
    } else {
        truncate_display(&target, 72)
    };
    format!("Edited {target} (+{} -{})", stats.added, stats.removed)
}

fn clean_edit_output(output: &str) -> String {
    let mut lines = Vec::new();
    let mut started = false;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.eq_ignore_ascii_case("patch applied")
            || trimmed.eq_ignore_ascii_case("written")
            || trimmed == "(no diff produced)"
        {
            continue;
        }
        if line.starts_with("diff --")
            || line.trim_start().starts_with("```diff")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
        {
            started = true;
        }
        if started || !trimmed.is_empty() {
            lines.push(line.to_string());
        }
    }
    if lines.is_empty() {
        "└ applied with no diff".to_string()
    } else {
        lines.join("\n")
    }
}

#[derive(Default)]
struct DiffStats {
    first_file: Option<String>,
    file_count: usize,
    added: usize,
    removed: usize,
}

fn diff_stats(output: &str) -> DiffStats {
    let mut stats = DiffStats::default();
    for line in output.lines() {
        let trimmed = line.trim_start();
        if let Some(path) = trimmed.strip_prefix("diff -- ") {
            stats.file_count += 1;
            if stats.first_file.is_none() {
                stats.first_file = Some(path.trim().to_string());
            }
            continue;
        }
        if trimmed.starts_with("+++") || trimmed.starts_with("---") || trimmed.starts_with("```") {
            continue;
        }
        if trimmed.starts_with('+') {
            stats.added += 1;
        } else if trimmed.starts_with('-') {
            stats.removed += 1;
        }
    }
    stats
}

fn capitalize_title(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_uppercase(), chars.collect::<String>()),
        None => "Tool".to_string(),
    }
}

fn truncate_display(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

fn mcp_tool_label(name: &str) -> Option<String> {
    let rest = name.strip_prefix("mcp__")?;
    let mut parts = rest.split("__");
    let server = parts.next().unwrap_or("server");
    let tool = parts.next().unwrap_or("tool");
    Some(format!("mcp {server}.{tool}"))
}

fn is_plan_title(title: &str) -> bool {
    title.eq_ignore_ascii_case("plan")
}

fn is_diff_tool_title(title: Option<&str>) -> bool {
    title
        .map(|value| value.starts_with("Edited"))
        .unwrap_or(false)
}

fn latest_plan_message_index(messages: &[ChatMessage]) -> Option<usize> {
    let last_user = messages
        .iter()
        .rposition(|message| matches!(message.role, MessageRole::User))
        .unwrap_or(0);
    messages
        .iter()
        .enumerate()
        .skip(last_user)
        .rev()
        .find(|(_, message)| {
            matches!(message.role, MessageRole::Tool)
                && message.title.as_deref().map(is_plan_title).unwrap_or(false)
        })
        .map(|(index, _)| index)
}

fn latest_empty_tool_message_index(messages: &[ChatMessage], label: &str) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| {
            matches!(message.role, MessageRole::Tool)
                && message.title.as_deref() == Some(label)
                && message.content.trim().is_empty()
        })
        .map(|(index, _)| index)
}

fn latest_tool_message_index(messages: &[ChatMessage], label: &str) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| {
            matches!(message.role, MessageRole::Tool) && message.title.as_deref() == Some(label)
        })
        .map(|(index, _)| index)
}

fn latest_empty_tool_any_index(messages: &[ChatMessage]) -> Option<usize> {
    messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, message)| {
            matches!(message.role, MessageRole::Tool) && message.content.trim().is_empty()
        })
        .map(|(index, _)| index)
}

fn latest_streaming_tool_message_index(messages: &[ChatMessage]) -> Option<usize> {
    let last_user = messages
        .iter()
        .rposition(|message| matches!(message.role, MessageRole::User))
        .unwrap_or(0);
    messages
        .iter()
        .enumerate()
        .skip(last_user)
        .rev()
        .find(|(_, message)| {
            matches!(message.role, MessageRole::Tool)
                && message
                    .title
                    .as_deref()
                    .map(|title| title.starts_with("Receiving "))
                    .unwrap_or(false)
        })
        .map(|(index, _)| index)
}

fn streaming_tool_title(name: Option<&str>) -> String {
    name.filter(|value| !value.trim().is_empty())
        .map(|name| format!("Receiving {}", tool_label(name)))
        .unwrap_or_else(|| "Receiving tool call".to_string())
}

fn append_streaming_tool_delta(content: &mut String, name: Option<&str>, delta: &str) {
    const LIMIT: usize = 2400;
    if content.trim().is_empty() {
        let label = name
            .filter(|value| !value.trim().is_empty())
            .map(tool_activity_label)
            .unwrap_or_else(|| "tool call".to_string());
        content.push_str("└ receiving ");
        content.push_str(&label);
    }

    let delta = redact_secrets(delta.trim());
    if !delta.is_empty() {
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push_str(&delta);
    }

    if content.chars().count() > LIMIT {
        let tail = content
            .chars()
            .rev()
            .take(LIMIT.saturating_sub(16))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<String>();
        *content = format!("...\n{tail}");
    }
}

fn should_draw_task_separator(message: &ChatMessage) -> bool {
    !message.content.trim().is_empty()
        && matches!(message.role, MessageRole::Shell)
        && message
            .title
            .as_deref()
            .map(|title| title.starts_with("Ran "))
            .unwrap_or(false)
}

fn render_task_separator(width: usize, theme: &ThemeConfig) -> Line<'static> {
    let width = width.saturating_sub(2).clamp(12, 160);
    Line::from(vec![
        Span::styled("  ", Style::default().fg(theme.border)),
        Span::styled("─".repeat(width), Style::default().fg(theme.border)),
    ])
}

fn remove_trailing_empty_assistant(messages: &mut Vec<ChatMessage>) {
    if messages
        .last()
        .map(|message| {
            matches!(message.role, MessageRole::Assistant) && message.content.trim().is_empty()
        })
        .unwrap_or(false)
    {
        messages.pop();
    }
}

fn replace_pending_assistant_with_error(messages: &mut Vec<ChatMessage>, title: &str, body: &str) {
    if let Some(message) = messages.last_mut() {
        if matches!(message.role, MessageRole::Assistant) && message.content.trim().is_empty() {
            message.role = MessageRole::System;
            message.title = Some(title.to_string());
            message.content = body.to_string();
            return;
        }
    }
    messages.push(ChatMessage {
        role: MessageRole::System,
        title: Some(title.to_string()),
        content: body.to_string(),
    });
}

fn provider_error_card(err: &str) -> (String, String) {
    let code = provider_error_status_code(err);
    let title = code
        .map(|code| format!("error ({code})"))
        .unwrap_or_else(|| "error".to_string());
    let detail = provider_error_detail(err, code);
    let mut body = format!("details: {detail}");
    let message = err.trim();
    if !message.is_empty() {
        body.push_str("\n\nmessage: ");
        body.push_str(message);
    }
    (title, body)
}

fn provider_error_status_code(err: &str) -> Option<u16> {
    for code in [429u16, 402, 401, 403, 404, 408, 500, 502, 503, 504] {
        if err.contains(&code.to_string())
            || err.contains(&format!("\"code\":{code}"))
            || err.contains(&format!("\"status\":{code}"))
            || err.contains(&format!("returned {code}"))
        {
            return Some(code);
        }
    }
    None
}

fn provider_error_detail(err: &str, code: Option<u16>) -> &'static str {
    let lower = err.to_ascii_lowercase();
    if code == Some(429)
        || lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("rate-limited")
    {
        return "Rate Limited";
    }
    if code == Some(402) || lower.contains("credit") || lower.contains("payment required") {
        return "Insufficient Credits";
    }
    if lower.contains("login required") {
        return "Login Required";
    }
    if code == Some(401) || lower.contains("invalid api key") || lower.contains("invalid_api_key") {
        return "Invalid API Key";
    }
    if lower.contains("empty response without text")
        || lower.contains("empty response without a final text response")
        || lower.contains("completed without visible output")
    {
        return "Empty Response";
    }
    if matches!(code, Some(500 | 502 | 503 | 504)) {
        return "Provider Unavailable";
    }
    "Provider Error"
}

fn render_card(
    role: &MessageRole,
    title: &str,
    color: Color,
    body_color: Color,
    content: &str,
    markdown: bool,
    thinking: bool,
    diff_mode: bool,
    compact: bool,
    width: usize,
    elapsed: Duration,
    theme: &ThemeConfig,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let rail = rail_symbol(role, title, thinking, elapsed);
    let title_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    lines.push(Line::from(vec![
        Span::styled(rail, title_style),
        Span::raw(" "),
        Span::styled(title.to_string(), title_style),
    ]));

    if thinking && content.trim().is_empty() {
        if matches!(role, MessageRole::Assistant) {
            return lines;
        }
        lines.extend(render_working_lines(elapsed, theme, width));
        return lines;
    }

    if is_plan_title(title) && !diff_mode {
        let plan_lines = render_plan_lines(content, width.saturating_sub(2), theme);
        let plan_lines = if compact {
            compact_rendered_lines(plan_lines, 6, theme)
        } else {
            plan_lines
        };
        lines.extend(plan_lines);
        return lines;
    }

    let body = if diff_mode {
        render_diff_lines(content, theme)
    } else if markdown {
        render_markdown_lines(content, body_color, theme)
    } else {
        render_plain_lines(content, body_color)
    };

    let body = if compact && matches!(role, MessageRole::Tool) {
        compact_rendered_lines(body, 5, theme)
    } else if matches!(role, MessageRole::Assistant) {
        add_reading_spacing(body)
    } else {
        body
    };

    if body.is_empty() {
        let placeholder = if matches!(role, MessageRole::Assistant) {
            "Thinking"
        } else {
            " "
        };
        lines.push(Line::from(vec![
            Span::styled(body_prefix(role), Style::default().fg(color)),
            Span::styled(placeholder, Style::default().fg(theme.muted)),
        ]));
    } else {
        for line in wrap_rendered_lines(body, width.saturating_sub(3)) {
            let mut spans = vec![Span::styled(body_prefix(role), Style::default().fg(color))];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }
    lines
}

fn body_prefix(role: &MessageRole) -> &'static str {
    match role {
        MessageRole::Tool | MessageRole::System | MessageRole::Shell | MessageRole::Queued => "│ ",
        MessageRole::User | MessageRole::Assistant => "  ",
    }
}

fn rail_symbol(
    role: &MessageRole,
    title: &str,
    _thinking: bool,
    _elapsed: Duration,
) -> &'static str {
    match role {
        MessageRole::User => ">",
        MessageRole::Assistant => "•",
        MessageRole::Tool => match tool_tone_from_title(title) {
            ToolTone::Error => "!",
            ToolTone::Plan => "□",
            ToolTone::Explore => "◇",
            ToolTone::Edit => "◆",
            ToolTone::Run => "▶",
            ToolTone::Memory => "◈",
            ToolTone::Skill => "◇",
            ToolTone::Mcp => "⬦",
            ToolTone::Other => "◆",
        },
        MessageRole::System => "!",
        MessageRole::Queued => "»",
        MessageRole::Shell => "$",
    }
}

fn render_working_lines(
    elapsed: Duration,
    theme: &ThemeConfig,
    _width: usize,
) -> Vec<Line<'static>> {
    vec![Line::from(vec![
        Span::styled("  ", Style::default().fg(theme.border)),
        Span::styled("Thinking", Style::default().fg(theme.muted)),
        Span::raw("  "),
        Span::styled(spinner(elapsed), Style::default().fg(theme.emphasis)),
    ])]
}

fn render_plan_lines(content: &str, width: usize, theme: &ThemeConfig) -> Vec<Line<'static>> {
    let parsed = parse_plan_content(content);
    let total = parsed.steps.len();
    let completed = parsed
        .steps
        .iter()
        .filter(|step| step.status == PlanStatus::Completed)
        .count();
    let active = parsed
        .steps
        .iter()
        .any(|step| step.status == PlanStatus::InProgress);

    let mut lines = Vec::new();
    if total > 0 {
        let label = if active { "active" } else { "idle" };
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(theme.border)),
            Span::styled(
                format!("{completed}/{total} done"),
                Style::default()
                    .fg(theme.emphasis)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default().fg(theme.muted)),
            Span::styled(label, Style::default().fg(theme.muted)),
            Span::styled("  ", Style::default().fg(theme.muted)),
            Span::styled(
                progress_bar(completed, total, width.saturating_sub(18).min(28).max(8)),
                Style::default().fg(theme.accent),
            ),
        ]));
    }

    if let Some(goal) = parsed.goal {
        lines.push(Line::from(vec![
            Span::styled("  goal ", Style::default().fg(theme.muted)),
            Span::styled(goal, Style::default().fg(theme.text)),
        ]));
    }

    if !parsed.note.trim().is_empty() {
        lines.push(Line::from(vec![
            Span::styled("  note ", Style::default().fg(theme.muted)),
            Span::styled(
                parsed.note.trim().to_string(),
                Style::default().fg(theme.text),
            ),
        ]));
    }

    for step in parsed.steps {
        lines.push(Line::from(vec![
            Span::styled("  ", Style::default().fg(theme.border)),
            Span::styled(
                plan_status_label(step.status),
                plan_status_style(step.status, theme),
            ),
            Span::raw(" "),
            Span::styled(step.text, plan_step_style(step.status, theme)),
        ]));
    }

    if lines.is_empty() {
        return render_markdown_lines(content, theme.tool_text, theme);
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PlanStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
}

struct PlanStep {
    status: PlanStatus,
    text: String,
}

struct ParsedPlan {
    goal: Option<String>,
    note: String,
    steps: Vec<PlanStep>,
}

fn parse_plan_content(content: &str) -> ParsedPlan {
    let mut goal = None;
    let mut note = Vec::new();
    let mut steps = Vec::new();

    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line == "## Plan" || line == "# Plan" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("**Goal:**") {
            goal = Some(rest.trim().to_string());
            continue;
        }
        if let Some((status, text)) = parse_plan_step(line) {
            steps.push(PlanStep { status, text });
            continue;
        }
        note.push(line.trim_matches('*').to_string());
    }

    ParsedPlan {
        goal,
        note: note.join(" "),
        steps,
    }
}

fn parse_plan_step(line: &str) -> Option<(PlanStatus, String)> {
    let line = line
        .trim_start_matches(|ch: char| ch.is_ascii_digit() || ch == '.' || ch.is_whitespace())
        .trim_start_matches("- ")
        .trim();
    let rest = line.strip_prefix('[')?;
    let (status, text) = rest.split_once(']')?;
    Some((parse_plan_status(status), text.trim().to_string()))
}

fn parse_plan_status(status: &str) -> PlanStatus {
    match status.trim().to_ascii_lowercase().as_str() {
        "completed" | "done" | "complete" => PlanStatus::Completed,
        "in_progress" | "in progress" | "active" | "working" => PlanStatus::InProgress,
        "blocked" => PlanStatus::Blocked,
        _ => PlanStatus::Pending,
    }
}

fn plan_status_label(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Pending => "todo",
        PlanStatus::InProgress => "now ",
        PlanStatus::Completed => "done",
        PlanStatus::Blocked => "stop",
    }
}

fn plan_status_style(status: PlanStatus, theme: &ThemeConfig) -> Style {
    let color = match status {
        PlanStatus::Pending => theme.muted,
        PlanStatus::InProgress => theme.emphasis,
        PlanStatus::Completed => theme.success,
        PlanStatus::Blocked => theme.danger,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn plan_step_style(status: PlanStatus, theme: &ThemeConfig) -> Style {
    match status {
        PlanStatus::Pending => Style::default().fg(theme.muted),
        PlanStatus::InProgress => Style::default().fg(theme.text).add_modifier(Modifier::BOLD),
        PlanStatus::Completed => Style::default().fg(theme.success),
        PlanStatus::Blocked => Style::default().fg(theme.danger),
    }
}

fn progress_bar(completed: usize, total: usize, width: usize) -> String {
    if total == 0 {
        return String::new();
    }
    let filled = (completed * width + total.saturating_sub(1)) / total;
    let mut out = String::with_capacity(width);
    for index in 0..width {
        if index < filled {
            out.push('━');
        } else {
            out.push('─');
        }
    }
    out
}

fn render_plain_lines(content: &str, color: Color) -> Vec<Line<'static>> {
    content
        .lines()
        .map(|line| {
            Line::from(vec![Span::styled(
                line.to_string(),
                Style::default().fg(color),
            )])
        })
        .collect()
}

fn render_diff_lines(content: &str, theme: &ThemeConfig) -> Vec<Line<'static>> {
    let mut rendered = Vec::new();
    let mut old_line = 1usize;
    let mut new_line = 1usize;
    for line in content
        .lines()
        .filter(|line| !line.trim_start().starts_with("```"))
    {
        let trimmed = line.trim_start();
        let indent = line.len().saturating_sub(trimmed.len());
        let prefix = &line[..indent];
        let number = if trimmed.starts_with('+') && !trimmed.starts_with("+++") {
            let value = new_line;
            new_line += 1;
            value
        } else if trimmed.starts_with('-') && !trimmed.starts_with("---") {
            let value = old_line;
            old_line += 1;
            value
        } else if trimmed.starts_with("@@")
            || trimmed.starts_with("+++")
            || trimmed.starts_with("---")
            || trimmed.starts_with("diff --")
        {
            0
        } else {
            let value = new_line;
            old_line += 1;
            new_line += 1;
            value
        };
        let number = if number == 0 {
            "      ".to_string()
        } else {
            format!("{number:>6}")
        };

        let line = if trimmed.starts_with("@@") {
            Line::from(vec![
                Span::styled(number, Style::default().fg(theme.muted)),
                Span::styled(" ┆ ", Style::default().fg(theme.emphasis)),
                Span::styled(trimmed.to_string(), Style::default().fg(theme.emphasis)),
            ])
        } else if trimmed.starts_with("+++")
            || trimmed.starts_with("---")
            || trimmed.starts_with("diff --")
        {
            Line::from(vec![
                Span::styled(number, Style::default().fg(theme.muted)),
                Span::styled(" ╎ ", Style::default().fg(theme.border)),
                Span::styled(trimmed.to_string(), Style::default().fg(theme.muted)),
            ])
        } else if trimmed.starts_with('+') {
            Line::from(vec![
                Span::styled(number, Style::default().fg(theme.muted)),
                Span::styled(" ┃ ", Style::default().fg(theme.success)),
                Span::styled(
                    trimmed.to_string(),
                    Style::default()
                        .fg(theme.success)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
        } else if trimmed.starts_with('-') {
            Line::from(vec![
                Span::styled(number, Style::default().fg(theme.muted)),
                Span::styled(" ┃ ", Style::default().fg(theme.danger)),
                Span::styled(trimmed.to_string(), Style::default().fg(theme.danger)),
            ])
        } else {
            Line::from(vec![
                Span::styled(
                    format!("{prefix}{number}"),
                    Style::default().fg(theme.muted),
                ),
                Span::styled(" ╎ ", Style::default().fg(theme.border)),
                Span::styled(trimmed.to_string(), Style::default().fg(theme.muted)),
            ])
        };
        rendered.push(line);
    }
    rendered
}

fn render_markdown_lines(content: &str, color: Color, theme: &ThemeConfig) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let raw_lines = content.lines().collect::<Vec<_>>();
    let mut fragment = Vec::new();
    let mut index = 0usize;
    let mut in_fence = false;

    while index < raw_lines.len() {
        let line = raw_lines[index];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            fragment.push(line);
            index += 1;
            continue;
        }

        if !in_fence && markdown_table_starts(&raw_lines, index) {
            flush_markdown_fragment(&mut out, &mut fragment, color, theme);
            let mut table_lines = vec![raw_lines[index], raw_lines[index + 1]];
            index += 2;
            while index < raw_lines.len() && markdown_table_continues(raw_lines[index]) {
                table_lines.push(raw_lines[index]);
                index += 1;
            }
            out.extend(render_table_lines(&table_lines, color, theme));
            continue;
        }

        fragment.push(line);
        index += 1;
    }

    flush_markdown_fragment(&mut out, &mut fragment, color, theme);
    out
}

fn flush_markdown_fragment(
    out: &mut Vec<Line<'static>>,
    fragment: &mut Vec<&str>,
    color: Color,
    theme: &ThemeConfig,
) {
    if fragment.is_empty() {
        return;
    }
    let text = fragment.join("\n");
    out.extend(render_markdown_fragment(&text, color, theme));
    fragment.clear();
}

fn render_markdown_fragment(
    content: &str,
    color: Color,
    theme: &ThemeConfig,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let parser = Parser::new_ext(content, Options::all());
    let mut current = Vec::<Span<'static>>::new();
    let mut in_code = false;
    let mut code_lines = Vec::<String>::new();
    let mut code_lang: Option<String> = None;
    let mut list_level = 0usize;
    let mut strong = false;
    let mut emphasis = false;
    let mut heading_level: Option<HeadingLevel> = None;

    let flush_current = |lines: &mut Vec<Line<'static>>, current: &mut Vec<Span<'static>>| {
        if !current.is_empty() {
            lines.push(Line::from(current.clone()));
            current.clear();
        }
    };

    let flush_code = |lines: &mut Vec<Line<'static>>,
                      code_lines: &mut Vec<String>,
                      code_lang: &mut Option<String>| {
        if code_lines.is_empty() {
            *code_lang = None;
            return;
        }
        let lang = code_lang.clone().unwrap_or_default();
        if lang == "diff" || lang == "patch" {
            lines.push(Line::from(vec![
                Span::styled("┌", Style::default().fg(theme.border)),
                Span::styled(" diff", Style::default().fg(theme.muted)),
            ]));
            lines.extend(render_diff_lines(&code_lines.join("\n"), theme));
            code_lines.clear();
            *code_lang = None;
            lines.push(Line::from(vec![
                Span::styled("└", Style::default().fg(theme.border)),
                Span::styled(" diff", Style::default().fg(theme.muted)),
            ]));
            return;
        }
        lines.push(Line::from(vec![
            Span::styled("┌", Style::default().fg(theme.border)),
            Span::styled(
                if lang.is_empty() {
                    " code".to_string()
                } else {
                    format!(" {lang}")
                },
                Style::default().fg(theme.muted),
            ),
        ]));
        for code_line in code_lines.iter() {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(theme.border)),
                Span::styled(code_line.clone(), Style::default().fg(theme.muted)),
            ]));
        }
        code_lines.clear();
        *code_lang = None;
        lines.push(Line::from(vec![
            Span::styled("└", Style::default().fg(theme.border)),
            Span::styled(
                if lang.is_empty() {
                    " code".to_string()
                } else {
                    format!(" {lang}")
                },
                Style::default().fg(theme.muted),
            ),
        ]));
    };

    for event in parser {
        match event {
            MdEvent::Start(tag) => match tag {
                Tag::Paragraph => {
                    flush_current(&mut lines, &mut current);
                }
                Tag::Heading { level, .. } => {
                    flush_current(&mut lines, &mut current);
                    heading_level = Some(level);
                }
                Tag::CodeBlock(kind) => {
                    flush_current(&mut lines, &mut current);
                    flush_code(&mut lines, &mut code_lines, &mut code_lang);
                    code_lang = code_block_language(&kind);
                    in_code = true;
                }
                Tag::List(_) => {
                    list_level = list_level.saturating_add(1);
                }
                Tag::Item => {
                    current.push(Span::styled(
                        format!("{}• ", "  ".repeat(list_level.saturating_sub(1))),
                        Style::default().fg(color),
                    ));
                }
                Tag::Strong => {
                    strong = true;
                }
                Tag::Emphasis => {
                    emphasis = true;
                }
                _ => {}
            },
            MdEvent::End(tag) => match tag {
                TagEnd::Paragraph => {
                    flush_current(&mut lines, &mut current);
                }
                TagEnd::Heading(_) => {
                    flush_current(&mut lines, &mut current);
                    heading_level = None;
                }
                TagEnd::CodeBlock => {
                    in_code = false;
                    flush_code(&mut lines, &mut code_lines, &mut code_lang);
                }
                TagEnd::List(_) => {
                    list_level = list_level.saturating_sub(1);
                    flush_current(&mut lines, &mut current);
                }
                TagEnd::Item => {
                    flush_current(&mut lines, &mut current);
                }
                TagEnd::Strong => {
                    strong = false;
                }
                TagEnd::Emphasis => {
                    emphasis = false;
                }
                _ => {}
            },
            MdEvent::Text(text) => {
                if in_code {
                    for part in text.lines() {
                        code_lines.push(part.to_string());
                    }
                    if text.ends_with('\n') {
                        code_lines.push(String::new());
                    }
                } else {
                    let base = heading_level
                        .map(|level| heading_style(level, theme))
                        .unwrap_or_else(|| Style::default().fg(color));
                    let style = current_style(base, strong, emphasis, false, theme);
                    for part in text.lines() {
                        if !current.is_empty() && part.is_empty() {
                            flush_current(&mut lines, &mut current);
                            continue;
                        }
                        if !current.is_empty() {
                            current.push(Span::raw(" "));
                        }
                        current.extend(parse_inline_spans(part, style, theme));
                    }
                }
            }
            MdEvent::Code(code) => {
                current.push(Span::styled(
                    code.to_string(),
                    Style::default()
                        .fg(theme.text)
                        .add_modifier(Modifier::ITALIC),
                ));
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                if !current.is_empty() {
                    lines.push(Line::from(current.clone()));
                    current.clear();
                } else {
                    lines.push(Line::from(" "));
                }
            }
            MdEvent::Rule => {
                lines.push(Line::from(vec![Span::styled(
                    "────────────────",
                    Style::default().fg(theme.muted),
                )]));
            }
            _ => {}
        }
    }

    flush_current(&mut lines, &mut current);
    flush_code(&mut lines, &mut code_lines, &mut code_lang);

    lines
}

fn code_block_language(kind: &CodeBlockKind<'_>) -> Option<String> {
    match kind {
        CodeBlockKind::Fenced(info) => info
            .split_whitespace()
            .next()
            .map(|lang| lang.trim().to_ascii_lowercase())
            .filter(|lang| !lang.is_empty()),
        CodeBlockKind::Indented => None,
    }
}

fn parse_inline_spans(text: &str, base_style: Style, theme: &ThemeConfig) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buffer = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut code_ticks = 0usize;
    let mut chars = text.char_indices().peekable();

    while let Some((_, ch)) = chars.next() {
        if ch == '`' {
            let mut ticks = 1usize;
            while let Some((_, '`')) = chars.peek().copied() {
                chars.next();
                ticks += 1;
            }
            if !buffer.is_empty() {
                spans.push(Span::styled(
                    buffer.clone(),
                    current_style(base_style, bold, italic, code_ticks > 0, theme),
                ));
                buffer.clear();
            }
            if code_ticks == ticks {
                code_ticks = 0;
            } else if code_ticks == 0 {
                code_ticks = ticks;
            } else {
                buffer.push_str(&"`".repeat(ticks));
            }
            continue;
        }

        if code_ticks == 0 && ch == '*' && chars.peek().map(|(_, ch)| *ch) == Some('*') {
            chars.next();
            if !buffer.is_empty() {
                spans.push(Span::styled(
                    buffer.clone(),
                    current_style(base_style, bold, italic, false, theme),
                ));
                buffer.clear();
            }
            bold = !bold;
            continue;
        }

        if code_ticks == 0 && ch == '*' {
            if !buffer.is_empty() {
                spans.push(Span::styled(
                    buffer.clone(),
                    current_style(base_style, bold, italic, false, theme),
                ));
                buffer.clear();
            }
            italic = !italic;
            continue;
        }

        buffer.push(ch);
    }

    if !buffer.is_empty() {
        spans.push(Span::styled(
            buffer,
            current_style(base_style, bold, italic, code_ticks > 0, theme),
        ));
    }

    spans
}

fn current_style(base: Style, bold: bool, italic: bool, code: bool, theme: &ThemeConfig) -> Style {
    let mut style = base;
    if code {
        return style.fg(theme.text).add_modifier(Modifier::ITALIC);
    }
    if bold {
        style = style.fg(theme.emphasis).add_modifier(Modifier::BOLD);
    }
    if italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    style
}

fn markdown_table_starts(lines: &[&str], index: usize) -> bool {
    if index + 1 >= lines.len() {
        return false;
    }
    markdown_table_continues(lines[index]) && is_table_separator(lines[index + 1])
}

fn markdown_table_continues(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.contains('|')
}

fn is_table_separator(line: &str) -> bool {
    let cells = split_table_cells(line);
    if cells.is_empty() {
        return false;
    }
    cells.iter().all(|cell| {
        let trimmed = cell.trim();
        trimmed.chars().filter(|ch| *ch == '-').count() >= 3
            && trimmed.chars().all(|ch| matches!(ch, '-' | ':' | ' '))
    })
}

fn render_table_lines(lines: &[&str], color: Color, theme: &ThemeConfig) -> Vec<Line<'static>> {
    let rows = lines
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, line)| split_table_cells(line))
        .filter(|row| !row.is_empty())
        .collect::<Vec<_>>();
    if rows.is_empty() {
        return Vec::new();
    }

    let columns = rows.iter().map(|row| row.len()).max().unwrap_or(0).min(8);
    let widths = (0..columns)
        .map(|column| {
            rows.iter()
                .filter_map(|row| row.get(column))
                .map(|cell| cell.chars().count())
                .max()
                .unwrap_or(3)
                .clamp(3, 28)
        })
        .collect::<Vec<_>>();

    let mut out = Vec::new();
    out.push(table_rule('┌', '┬', '┐', &widths, theme));
    for (row_index, row) in rows.iter().enumerate() {
        let header = row_index == 0;
        out.push(table_row(row, &widths, header, color, theme));
        if header {
            out.push(table_rule('├', '┼', '┤', &widths, theme));
        }
    }
    out.push(table_rule('└', '┴', '┘', &widths, theme));
    out
}

fn split_table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_matches('|')
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect::<Vec<_>>()
}

fn table_rule(
    left: char,
    middle: char,
    right: char,
    widths: &[usize],
    theme: &ThemeConfig,
) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    for (index, width) in widths.iter().enumerate() {
        text.push_str(&"─".repeat(width + 2));
        text.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    Line::from(Span::styled(text, Style::default().fg(theme.border)))
}

fn table_row(
    row: &[String],
    widths: &[usize],
    header: bool,
    color: Color,
    theme: &ThemeConfig,
) -> Line<'static> {
    let mut spans = Vec::new();
    let style = if header {
        Style::default()
            .fg(theme.emphasis)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
    };
    for (index, width) in widths.iter().enumerate() {
        spans.push(Span::styled("│ ", Style::default().fg(theme.border)));
        let cell = row.get(index).cloned().unwrap_or_default();
        let cell = truncate_table_cell(&cell, *width);
        let padding = width.saturating_sub(cell.chars().count());
        spans.extend(parse_inline_spans(&cell, style, theme));
        if padding > 0 {
            spans.push(Span::raw(" ".repeat(padding)));
        }
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled("│", Style::default().fg(theme.border)));
    Line::from(spans)
}

fn truncate_table_cell(cell: &str, width: usize) -> String {
    if cell.chars().count() <= width {
        return cell.to_string();
    }
    if width <= 1 {
        return "…".to_string();
    }
    let mut text = cell.chars().take(width - 1).collect::<String>();
    text.push('…');
    text
}

fn wrap_rendered_lines(lines: Vec<Line<'static>>, max_width: usize) -> Vec<Line<'static>> {
    if max_width == 0 {
        return lines;
    }

    let mut wrapped = Vec::new();
    for line in lines {
        let mut current = Vec::<Span<'static>>::new();
        let mut current_width = 0usize;

        for span in line.spans {
            let style = span.style;
            let text = span.content.to_string();
            for ch in text.chars() {
                if ch == '\n' {
                    if !current.is_empty() {
                        wrapped.push(Line::from(current.clone()));
                        current.clear();
                        current_width = 0;
                    } else {
                        wrapped.push(Line::from(" "));
                    }
                    continue;
                }

                if current_width >= max_width {
                    wrapped.push(Line::from(current.clone()));
                    current.clear();
                    current_width = 0;
                }

                current.push(Span::styled(ch.to_string(), style));
                current_width += 1;
            }
        }

        if !current.is_empty() {
            wrapped.push(Line::from(current));
        }
    }

    wrapped
}

fn heading_style(level: HeadingLevel, theme: &ThemeConfig) -> Style {
    let color = match level {
        HeadingLevel::H1 | HeadingLevel::H2 => theme.emphasis,
        HeadingLevel::H3 | HeadingLevel::H4 => theme.text,
        HeadingLevel::H5 | HeadingLevel::H6 => theme.muted,
    };
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

fn compact_rendered_lines(
    mut lines: Vec<Line<'static>>,
    max_lines: usize,
    theme: &ThemeConfig,
) -> Vec<Line<'static>> {
    if lines.len() <= max_lines {
        return lines;
    }
    let hidden = lines.len().saturating_sub(max_lines.saturating_sub(1));
    lines.truncate(max_lines.saturating_sub(1));
    lines.push(Line::from(vec![Span::styled(
        format!("… {hidden} more lines"),
        Style::default().fg(theme.muted),
    )]));
    lines
}

fn add_reading_spacing(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let texts = lines.iter().map(line_text).collect::<Vec<_>>();
    let len = texts.len();
    for (index, line) in lines.into_iter().enumerate() {
        out.push(line);
        if index + 1 >= len {
            continue;
        }
        if should_add_reading_gap(&texts[index], &texts[index + 1]) {
            out.push(Line::from(""));
        }
    }
    out
}

fn line_text(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn should_add_reading_gap(current: &str, next: &str) -> bool {
    let current = current.trim();
    let next = next.trim();
    if current.is_empty() || next.is_empty() {
        return false;
    }
    if is_compact_markdown_line(current) || is_compact_markdown_line(next) {
        return false;
    }
    true
}

fn is_compact_markdown_line(text: &str) -> bool {
    text.starts_with('•')
        || text.starts_with('-')
        || text.starts_with('|')
        || text.starts_with("```")
        || text.starts_with("┌")
        || text.starts_with("└")
}

fn load_attachment(root: &Path, path: PathBuf) -> Result<AttachedFile, String> {
    let label = path
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    if let Some(mime_type) = image_mime_type(&path) {
        let data = fs::read(&path).map_err(|e| e.to_string())?;
        return Ok(AttachedFile {
            path,
            label: label.clone(),
            kind: AttachmentKind::Image(PromptImage {
                label,
                mime_type: mime_type.to_string(),
                data_base64: base64_encode(&data),
            }),
        });
    }
    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    Ok(AttachedFile {
        path,
        label,
        kind: AttachmentKind::Text(content),
    })
}

fn attachment_placeholder(attachment: &AttachedFile) -> String {
    match attachment.kind {
        AttachmentKind::Text(_) => format!("[{}]", attachment.label),
        AttachmentKind::Image(_) => "[Arquivo]".to_string(),
    }
}

struct VirtualMention {
    placeholder: String,
    content: String,
}

fn resolve_virtual_mention(
    paths: &AppPaths,
    query: &str,
) -> Result<Option<VirtualMention>, String> {
    let normalized = query.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Ok(None);
    }

    let skills = SkillStore::new(paths)?.list()?;
    if let Some(skill) = skills
        .into_iter()
        .find(|skill| skill.name.eq_ignore_ascii_case(&normalized))
    {
        return Ok(Some(VirtualMention {
            placeholder: format!("[skill:{}]", skill.name),
            content: format!(
                "[Skill: {}]\nPath: {}\nDescription: {}\n\n```markdown\n{}\n```",
                skill.name,
                skill.path.display(),
                skill.description,
                skill.body
            ),
        }));
    }

    let registry = McpRegistry::load(paths)?;
    if let Some(server) = registry
        .servers()
        .iter()
        .find(|server| server.name.eq_ignore_ascii_case(&normalized))
    {
        let mut content = format!(
            "[MCP server: {}]\ntransport: {}\n",
            server.name, server.transport
        );
        if let Some(url) = server.url.as_deref() {
            content.push_str("url: ");
            content.push_str(url);
            content.push('\n');
        }
        if !server.command.trim().is_empty() {
            content.push_str("command: ");
            content.push_str(&server.command);
            if !server.args.is_empty() {
                content.push(' ');
                content.push_str(&server.args.join(" "));
            }
            content.push('\n');
        }
        if let Some(timeout) = server.startup_ts {
            content.push_str(&format!("startup_ts: {timeout}\n"));
        }
        return Ok(Some(VirtualMention {
            placeholder: format!("[mcp:{}]", server.name),
            content,
        }));
    }

    Ok(None)
}

fn active_attachment_token(input: &str, cursor: usize) -> Option<(usize, usize, String)> {
    let cursor = cursor.min(input.len());
    let before = input.get(..cursor)?;
    let start = before
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    let token = input.get(start..)?;
    if !token.starts_with('@') {
        return None;
    }
    let end = input[cursor..]
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(idx, _)| cursor + idx)
        .unwrap_or(input.len());
    let query = input
        .get(start + 1..cursor)
        .unwrap_or_default()
        .trim()
        .to_string();
    Some((start, end, sanitize_attachment_query(&query)))
}

fn replace_mention_query(prompt: &str, query: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(prompt.len() + replacement.len());
    let mut cursor = 0;
    let mut replaced = false;

    for (start, token) in prompt.split_whitespace().filter_map(|token| {
        let start = prompt[cursor..].find(token).map(|offset| cursor + offset)?;
        cursor = start + token.len();
        Some((start, token))
    }) {
        if !replaced {
            if let Some(raw_query) = token.strip_prefix('@') {
                if sanitize_attachment_query(raw_query).eq_ignore_ascii_case(query) {
                    out.push_str(&prompt[out.len()..start]);
                    out.push_str(replacement);
                    out.push_str(&prompt[start + token.len()..]);
                    replaced = true;
                    break;
                }
            }
        }
    }

    if replaced {
        out
    } else {
        prompt.to_string()
    }
}

fn list_mention_picker_entries(
    paths: &AppPaths,
    directory: &Path,
    query: &str,
) -> Result<Vec<FilePickerEntry>, String> {
    let mut entries = list_virtual_mention_entries(paths, query);
    entries.extend(list_file_picker_entries(&paths.root_dir, directory, query)?);
    Ok(entries)
}

fn list_virtual_mention_entries(paths: &AppPaths, query: &str) -> Vec<FilePickerEntry> {
    let query = query.trim().to_ascii_lowercase();
    let mut entries = Vec::new();

    if let Ok(skills) = SkillStore::new(paths).and_then(|store| store.list()) {
        let mut skill_entries = skills
            .into_iter()
            .filter_map(|skill| {
                let haystack = format!("{} {}", skill.name, skill.description).to_ascii_lowercase();
                mention_score(&skill.name, &haystack, &query).map(|score| {
                    let label = if skill.description.trim().is_empty() {
                        format!("skill  @{}", skill.name)
                    } else {
                        format!(
                            "skill  @{}  {}",
                            skill.name,
                            truncate_display(&skill.description, 64)
                        )
                    };
                    (
                        score,
                        skill.name.clone(),
                        FilePickerEntry {
                            path: skill.path,
                            label,
                            kind: FilePickerEntryKind::Skill,
                            mention: Some(skill.name),
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        skill_entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        entries.extend(skill_entries.into_iter().take(8).map(|(_, _, entry)| entry));
    }

    if let Ok(registry) = McpRegistry::load(paths) {
        let mut server_entries = registry
            .servers()
            .iter()
            .filter_map(|server| {
                let endpoint = server
                    .url
                    .as_deref()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(&server.command);
                let haystack = format!(
                    "{} {} {} {}",
                    server.name,
                    server.transport,
                    endpoint,
                    server.args.join(" ")
                )
                .to_ascii_lowercase();
                mention_score(&server.name, &haystack, &query).map(|score| {
                    (
                        score,
                        server.name.clone(),
                        FilePickerEntry {
                            path: paths.root_dir.clone(),
                            label: format!(
                                "mcp    @{}  {}",
                                server.name,
                                truncate_display(endpoint, 64)
                            ),
                            kind: FilePickerEntryKind::McpServer,
                            mention: Some(server.name.clone()),
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        server_entries.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        entries.extend(
            server_entries
                .into_iter()
                .take(8)
                .map(|(_, _, entry)| entry),
        );
    }

    entries
}

fn mention_score(name: &str, haystack: &str, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(3);
    }
    let name_lower = name.to_ascii_lowercase();
    if name_lower == query {
        Some(0)
    } else if name_lower.starts_with(query) {
        Some(1)
    } else if name_lower.contains(query) {
        Some(2)
    } else if haystack.contains(query) {
        Some(4)
    } else {
        None
    }
}

fn list_file_picker_entries(
    root: &Path,
    directory: &Path,
    query: &str,
) -> Result<Vec<FilePickerEntry>, String> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let directory = fs::canonicalize(directory).unwrap_or_else(|_| directory.to_path_buf());
    let mut entries = Vec::new();
    if directory != root {
        let parent = directory.parent().unwrap_or(&root).to_path_buf();
        entries.push(FilePickerEntry {
            path: parent,
            label: "../".to_string(),
            kind: FilePickerEntryKind::Parent,
            mention: None,
        });
    }

    let query = query.to_lowercase();
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in fs::read_dir(&directory).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry.file_type().map_err(|e| e.to_string())?;
        if file_type.is_dir() {
            if should_skip_dir(&name) {
                continue;
            }
            if query.is_empty() || name.to_lowercase().contains(&query) {
                dirs.push(FilePickerEntry {
                    path,
                    label: format!("{name}/"),
                    kind: FilePickerEntryKind::Directory,
                    mention: None,
                });
            }
        } else if (query.is_empty() || name.to_lowercase().contains(&query))
            && !should_skip_file_candidate(&root, &path, &name)
        {
            files.push(FilePickerEntry {
                path,
                label: name,
                kind: FilePickerEntryKind::File,
                mention: None,
            });
        }
    }
    dirs.sort_by(|a, b| a.label.cmp(&b.label));
    files.sort_by(|a, b| a.label.cmp(&b.label));
    entries.extend(dirs);
    entries.extend(files);
    Ok(entries)
}

fn display_relative_path(root: &Path, path: &Path) -> String {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.strip_prefix(root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|| ".".to_string())
}

fn find_attachment_matches(root: &Path, query: &str, limit: usize) -> Result<Vec<PathBuf>, String> {
    let mut matches = Vec::<(u8, String, PathBuf)>::new();
    let query = query.to_lowercase();
    walk_attachment_matches(root, root, &query, limit, &mut matches)?;
    matches.sort_by(|a, b| a.cmp(b));
    Ok(matches.into_iter().map(|(_, _, path)| path).collect())
}

fn walk_attachment_matches(
    root: &Path,
    dir: &Path,
    query: &str,
    limit: usize,
    matches: &mut Vec<(u8, String, PathBuf)>,
) -> Result<(), String> {
    if matches.len() >= limit {
        return Ok(());
    }

    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let file_name = entry.file_name().to_string_lossy().to_string();
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            if should_skip_dir(&file_name) {
                continue;
            }
            walk_attachment_matches(root, &path, query, limit, matches)?;
            continue;
        }
        if should_skip_file_candidate(root, &path, &file_name) {
            continue;
        }

        let rel = path
            .strip_prefix(root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        let rel_lower = rel.to_lowercase();
        let name_lower = file_name.to_lowercase();

        let score = if name_lower == query {
            Some(0)
        } else if name_lower.contains(query) {
            Some(1)
        } else if rel_lower.contains(query) {
            Some(2)
        } else {
            None
        };

        if let Some(score) = score {
            matches.push((score, rel, path));
        }
    }

    Ok(())
}

fn should_skip_dir(name: &str) -> bool {
    matches!(
        name,
        ".git" | ".wire" | ".wirecli" | "target" | "node_modules"
    ) || name == pre_wire_state_dir_name()
}

fn pre_wire_state_dir_name() -> String {
    String::from_utf8(vec![46, 114, 105, 102, 116, 99, 111, 100, 101])
        .unwrap_or_else(|_| ".wirecli".to_string())
}

fn should_skip_file_candidate(root: &Path, path: &Path, name: &str) -> bool {
    if !is_framework_note_name(name) {
        return false;
    }
    let Some(parent) = path.parent() else {
        return false;
    };
    let parent = fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if parent != root {
        return false;
    }
    let Ok(raw) = fs::read_to_string(path) else {
        return false;
    };
    let text = raw.trim();
    text.len() <= 512
        && [
            "new next.js project",
            "project has been initialized",
            "inspect the workspace",
            "before taking any further action",
        ]
        .iter()
        .any(|marker| text.to_ascii_lowercase().contains(marker))
}

fn is_framework_note_name(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "next.js" | "react" | "vue" | "svelte" | "tailwind" | "prisma"
    )
}

fn sanitize_attachment_query(query: &str) -> String {
    query
        .trim_matches(|ch: char| {
            matches!(
                ch,
                ',' | '.' | ':' | ';' | '!' | '?' | ')' | '(' | '[' | ']' | '{' | '}'
            )
        })
        .to_string()
}

fn run_shell_command(root: &Path, config: &AppConfig, command: &str) -> String {
    if config.permission_mode == PermissionMode::FullAccess {
        return run_unrestricted_shell_command(root, command);
    }
    let argv = match split_command_line(command) {
        Ok(argv) => argv,
        Err(err) => return format!("invalid command: {err}"),
    };
    if argv.is_empty() {
        return "missing command".to_string();
    }
    if let Err(violation) = CommandPolicy::standard().validate_for_workspace(&argv, root) {
        return format!(
            "blocked by Lattice: {}: {}",
            violation.command, violation.reason
        );
    }
    if config.permission_mode == PermissionMode::Guardian {
        match crate::guardian::review_command(
            config,
            &argv,
            root,
            "manual TUI shell command",
            "user invoked !command in Wire CLI TUI",
        ) {
            Ok(decision) if decision.allow => {}
            Ok(decision) => {
                return format!(
                    "blocked by Guardian (risk={}): {}",
                    decision.risk, decision.reason
                );
            }
            Err(err) => return format!("blocked by Guardian: {err}"),
        }
    }
    let mut process = Command::new(&argv[0]);
    process.args(&argv[1..]).current_dir(root);
    match process.output() {
        Ok(output) => format_command_output(command, output),
        Err(err) => format!("failed to run shell command: {err}"),
    }
}

fn run_unrestricted_shell_command(root: &Path, command: &str) -> String {
    let output = Command::new("sh")
        .arg("-lc")
        .arg(command)
        .current_dir(root)
        .output();
    match output {
        Ok(output) => format_command_output(command, output),
        Err(err) => format!("failed to run shell command: {err}"),
    }
}

fn format_command_output(_command: &str, output: std::process::Output) -> String {
    let mut text = String::new();
    if !output.stdout.is_empty() {
        text.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    if !output.status.success() {
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("exit: {}", output.status));
    }
    text
}

fn save_pasted_image(data: &str, index: usize) -> Result<Option<PastedImage>, String> {
    if let Some((mime_type, bytes)) = decode_image_data_url(data)? {
        return write_pasted_image(index, mime_type, bytes);
    }

    let trimmed = data.trim().trim_matches('"').trim_matches('\'');
    let path_text = trimmed.strip_prefix("file://").unwrap_or(trimmed);
    let path = PathBuf::from(path_text);
    if path.exists() && path.is_file() {
        if let Some(mime_type) = image_mime_type(&path) {
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            return write_pasted_image(index, mime_type, bytes);
        }
    }

    Ok(None)
}

fn decode_image_data_url(data: &str) -> Result<Option<(&'static str, Vec<u8>)>, String> {
    let trimmed = data.trim();
    let Some((prefix, payload)) = trimmed.split_once(',') else {
        return Ok(None);
    };
    let mime_type = if prefix.starts_with("data:image/png;base64") {
        "image/png"
    } else if prefix.starts_with("data:image/jpeg;base64")
        || prefix.starts_with("data:image/jpg;base64")
    {
        "image/jpeg"
    } else if prefix.starts_with("data:image/webp;base64") {
        "image/webp"
    } else if prefix.starts_with("data:image/gif;base64") {
        "image/gif"
    } else {
        return Ok(None);
    };
    Ok(Some((mime_type, base64_decode(payload)?)))
}

fn write_pasted_image(
    index: usize,
    mime_type: &'static str,
    bytes: Vec<u8>,
) -> Result<Option<PastedImage>, String> {
    let dir = std::env::temp_dir().join("wirecli").join("pasted-images");
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let extension = match mime_type {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "img",
    };
    let path = dir.join(format!("pasted-image-{index}.{extension}"));
    fs::write(&path, &bytes).map_err(|e| e.to_string())?;
    let label = format!("Pasted Image #{index}");
    Ok(Some(PastedImage {
        index,
        label: label.clone(),
        path,
        prompt_image: PromptImage {
            label,
            mime_type: mime_type.to_string(),
            data_base64: base64_encode(&bytes),
        },
    }))
}

fn image_mime_type(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg") | Some("jpeg") => Some("image/jpeg"),
        Some("webp") => Some("image/webp"),
        Some("gif") => Some("image/gif"),
        _ => None,
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    let mut clean = input
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .collect::<Vec<_>>();
    if clean.len() % 4 != 0 {
        return Err("invalid base64 image payload".to_string());
    }
    let mut out = Vec::new();
    while !clean.is_empty() {
        let chunk = clean.drain(..4).collect::<Vec<_>>();
        let values = chunk
            .iter()
            .map(|ch| base64_value(*ch))
            .collect::<Result<Vec<_>, _>>()?;
        out.push((values[0] << 2) | (values[1] >> 4));
        if chunk[2] != '=' {
            out.push((values[1] << 4) | (values[2] >> 2));
        }
        if chunk[3] != '=' {
            out.push((values[2] << 6) | values[3]);
        }
    }
    Ok(out)
}

fn base64_value(ch: char) -> Result<u8, String> {
    match ch {
        'A'..='Z' => Ok(ch as u8 - b'A'),
        'a'..='z' => Ok(ch as u8 - b'a' + 26),
        '0'..='9' => Ok(ch as u8 - b'0' + 52),
        '+' => Ok(62),
        '/' => Ok(63),
        '=' => Ok(0),
        _ => Err("invalid base64 image payload".to_string()),
    }
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}

fn normalize_tui_command(prompt: &str) -> Option<String> {
    match prompt {
        "/mcp" | "/status" | "/permissions" | "/login" | "/providers" | "/new" => {
            Some(prompt.to_string())
        }
        "/models" => Some("/models".to_string()),
        value if "/models".starts_with(value) && value.len() >= 4 => Some("/models".to_string()),
        value if "/login".starts_with(value) && value.len() >= 4 => Some("/login".to_string()),
        value if "/providers".starts_with(value) && value.len() >= 5 => {
            Some("/providers".to_string())
        }
        value if "/new".starts_with(value) && value.len() >= 4 => Some("/new".to_string()),
        value if "/permissions".starts_with(value) && value.len() >= 5 => {
            Some("/permissions".to_string())
        }
        _ => None,
    }
}

fn permission_modes() -> [PermissionMode; 3] {
    [
        PermissionMode::Normal,
        PermissionMode::Guardian,
        PermissionMode::FullAccess,
    ]
}

fn overlay_has_modal_focus(overlay: &Overlay) -> bool {
    !matches!(overlay, Overlay::None | Overlay::LoginGate { .. })
}

#[cfg(test)]
mod tests {
    use super::{
        append_streaming_tool_delta, body_color, card_color, checkpoint_recovery_message,
        fallback_model, health_base_url, message_title, provider_error_card, render_card,
        render_diff_lines, render_markdown_lines, replace_mention_query,
        replace_pending_assistant_with_error, resolve_virtual_mention, streaming_tool_title,
        tool_result_body, tool_result_title, App, ChatMessage, FilePickerEntryKind, MessageRole,
        Overlay,
    };
    use crate::config::{AppConfig, AppPaths, ThemeConfig};
    use crate::id::next_id;
    use crate::session::TimelineEvent;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::fs;
    use std::time::Duration;

    #[test]
    fn health_base_url_strips_v1_suffix() {
        assert_eq!(
            health_base_url("http://127.0.0.1:3000/v1"),
            "http://127.0.0.1:3000/v1"
        );
    }

    #[test]
    fn health_base_url_keeps_root_url() {
        assert_eq!(
            health_base_url("http://127.0.0.1:3000"),
            "http://127.0.0.1:3000"
        );
    }

    #[test]
    fn startup_with_saved_model_does_not_probe_provider() {
        let paths = test_paths();
        let mut config = AppConfig::default();
        config.api_key = Some("wire_test_key".to_string());
        config.model = "qwen3.7-max".to_string();
        config.provider = "qwenproxy".to_string();
        config.base_url = "http://127.0.0.1:3000/v1".to_string();
        let app = super::App::new(
            paths.clone(),
            config.clone(),
            vec![fallback_model(&config.model)],
            Vec::new(),
            ThemeConfig::default(),
            None,
        );

        assert!(!app.models_loading);
        assert!(app.model_rx.is_none());
        assert!(app.startup_notice.is_none());
        assert_eq!(app.backend_health.message, "ready");

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn provider_picker_keeps_keyboard_focus_inside_login_gate() {
        let paths = test_paths();
        let config = AppConfig::default();
        let mut app = App::new(
            paths.clone(),
            config,
            Vec::new(),
            Vec::new(),
            ThemeConfig::default(),
            None,
        );
        app.login_required = true;
        app.overlay = Overlay::LoginGate {
            selected: 2,
            status: "choose provider".to_string(),
        };

        app.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            app.overlay,
            Overlay::ProviderPicker { selected: 0 }
        ));

        app.handle_key(key(KeyCode::Down));
        assert!(matches!(
            app.overlay,
            Overlay::ProviderPicker { selected: 1 }
        ));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn renders_markdown_tables_as_boxes() {
        let theme = ThemeConfig::default();
        let lines = render_markdown_lines(
            "| name | status |\n| --- | --- |\n| Wire | ok |",
            theme.text,
            &theme,
        );
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("name"));
        assert!(text.contains("Wire"));
        assert!(text.contains("┌"));
    }

    #[test]
    fn streaming_tool_delta_renders_receiving_card_body() {
        let mut content = String::new();

        append_streaming_tool_delta(&mut content, Some("shell"), r#"{"command":["npm""#);
        append_streaming_tool_delta(&mut content, Some("shell"), r#","install"]}"#);

        assert_eq!(streaming_tool_title(Some("shell")), "Receiving run");
        assert!(content.contains("receiving"));
        assert!(content.contains("npm"));
        assert!(content.contains("install"));
    }

    #[test]
    fn checkpoint_recovery_message_restores_partial_stream() {
        let event = TimelineEvent {
            kind: "checkpoint".to_string(),
            role: None,
            content: Some(
                r#"{"backend":"chat_completions","text_chars":12,"text_excerpt":"quase pronto","tools":[]}"#
                    .to_string(),
            ),
            command: Some("stream_partial".to_string()),
            stdout: None,
            stderr: None,
            exit_code: None,
            created_at: "2026-06-08T00:00:00Z".to_string(),
        };

        let message = checkpoint_recovery_message(&event).unwrap();

        assert!(matches!(message.role, MessageRole::Assistant));
        assert_eq!(message.title.as_deref(), Some("Recovered stream"));
        assert_eq!(message.content, "quase pronto");
    }

    #[test]
    fn renders_indented_fenced_diff_as_diff() {
        let theme = ThemeConfig::default();
        let lines = render_diff_lines("  ```diff\n  +added\n  -removed\n  ```", &theme);
        assert_eq!(lines.len(), 2);
        assert!(lines[0]
            .spans
            .iter()
            .any(|span| span.style.fg == Some(theme.success)));
        assert!(lines[1]
            .spans
            .iter()
            .any(|span| span.style.fg == Some(theme.danger)));
    }

    #[test]
    fn renders_markdown_diff_fence_as_diff() {
        let theme = ThemeConfig::default();
        let lines = render_markdown_lines("```diff\n+added\n-removed\n```", theme.text, &theme);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("diff"));
        assert!(text.contains("+added"));
        assert!(text.contains("-removed"));
        assert!(lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.style.fg == Some(theme.success)));
        assert!(lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .any(|span| span.style.fg == Some(theme.danger)));
    }

    #[test]
    fn formats_apply_patch_result_like_edited_card() {
        let output = "patch applied\n\ndiff -- src/tui.rs\n```diff\n--- src/tui.rs\n+++ src/tui.rs\n-old\n+new\n```";
        let title = tool_result_title("apply_patch", Some("src/tui.rs"), output);
        let body = tool_result_body("apply_patch", Some("src/tui.rs"), output);

        assert_eq!(title, "Edited src/tui.rs (+1 -1)");
        assert!(body.contains("diff -- src/tui.rs"));
        assert!(!body.contains("patch applied"));
    }

    #[test]
    fn formats_read_tools_as_explored_cards() {
        let title = tool_result_title("read_file", Some("src/tui.rs"), "ignored body");
        let body = tool_result_body("read_file", Some("src/tui.rs"), "ignored body");

        assert_eq!(title, "Read src/tui.rs");
        assert!(body.contains("reading `src/tui.rs`"));
        assert!(body.contains("loaded file contents"));
    }

    #[test]
    fn formats_list_dir_with_visible_entries() {
        let output =
            "Listed\nCurrent directory: .\nFolders\n- src/\n\nFiles\n- Cargo.toml\n- README.md";
        let title = tool_result_title("list_dir", Some("."), output);
        let body = tool_result_body("list_dir", Some("."), output);

        assert_eq!(title, "Listed .");
        assert!(body.contains("listing `.`"));
        assert!(body.contains("- src/"));
        assert!(body.contains("- Cargo.toml"));
    }

    #[test]
    fn tool_card_tones_distinguish_explore_edit_and_run() {
        let theme = ThemeConfig::default();
        let explore = card_color(&MessageRole::Tool, "Read src/tui.rs", &theme);
        let edit = card_color(&MessageRole::Tool, "Edited src/tui.rs (+1 -1)", &theme);
        let run = card_color(&MessageRole::Tool, "Ran cargo test", &theme);
        let memory = card_color(&MessageRole::Tool, "Memory", &theme);
        let mcp = card_color(&MessageRole::Tool, "Mcp", &theme);

        assert_eq!(explore, theme.tool_text);
        assert_eq!(edit, theme.success);
        assert_eq!(run, theme.accent);
        assert_eq!(memory, theme.emphasis);
        assert_eq!(mcp, theme.accent);
        assert_ne!(explore, run);
        assert_ne!(edit, run);
        assert_ne!(memory, explore);
    }

    #[test]
    fn shell_cards_are_execution_colored_not_error_colored() {
        let theme = ThemeConfig::default();

        assert_eq!(
            card_color(&MessageRole::Shell, "Ran cargo test", &theme),
            theme.accent
        );
        assert_ne!(
            card_color(&MessageRole::Shell, "Ran cargo test", &theme),
            theme.danger
        );
        assert_eq!(
            card_color(&MessageRole::System, "error (429)", &theme),
            theme.danger
        );
        assert_eq!(
            body_color(&MessageRole::Shell, "Ran cargo test", &theme),
            theme.text
        );
    }

    #[test]
    fn tool_errors_are_error_colored_not_explore_colored() {
        let theme = ThemeConfig::default();
        let output = "Tool error in `read_lines`\nmissing numeric field: end_line\nCorrect the tool arguments or choose another tool, then continue.";
        let title = tool_result_title("read_lines", Some("Next.js"), output);

        assert_eq!(title, "Tool error in lines");
        assert_eq!(card_color(&MessageRole::Tool, &title, &theme), theme.danger);
        assert_eq!(body_color(&MessageRole::Tool, &title, &theme), theme.danger);
    }

    #[test]
    fn tool_cards_render_visible_colored_body_gutter() {
        let theme = ThemeConfig::default();
        let title = "Ran cargo test";
        let lines = render_card(
            &MessageRole::Tool,
            title,
            card_color(&MessageRole::Tool, title, &theme),
            body_color(&MessageRole::Tool, title, &theme),
            "ok",
            true,
            false,
            false,
            false,
            80,
            Duration::from_secs(1),
            &theme,
        );

        assert!(lines
            .iter()
            .skip(1)
            .flat_map(|line| line.spans.iter())
            .any(|span| span.content.as_ref() == "│ " && span.style.fg == Some(theme.accent)));
    }

    #[test]
    fn empty_assistant_cards_show_thinking_not_awaiting_output() {
        let theme = ThemeConfig::default();
        let title = "wire";
        let lines = render_card(
            &MessageRole::Assistant,
            title,
            card_color(&MessageRole::Assistant, title, &theme),
            body_color(&MessageRole::Assistant, title, &theme),
            "",
            true,
            false,
            false,
            false,
            80,
            Duration::from_secs(1),
            &theme,
        );
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Thinking"));
        assert!(!text.contains("awaiting output"));
    }

    #[test]
    fn provider_error_replaces_pending_thinking_card() {
        let mut messages = vec![ChatMessage {
            role: MessageRole::Assistant,
            title: None,
            content: String::new(),
        }];
        let (title, body) = provider_error_card(
            "chat completions endpoint returned 429 Too Many Requests: Rate limit exceeded",
        );

        replace_pending_assistant_with_error(&mut messages, &title, &body);

        assert_eq!(messages.len(), 1);
        assert!(matches!(messages[0].role, MessageRole::System));
        assert_eq!(messages[0].title.as_deref(), Some("error (429)"));
        assert!(messages[0].content.contains("details: Rate Limited"));
        assert!(!messages[0].content.contains("Thinking"));
    }

    #[test]
    fn system_error_cards_render_the_error_title() {
        let message = ChatMessage {
            role: MessageRole::System,
            title: Some("error (429)".to_string()),
            content: "details: Rate Limited".to_string(),
        };

        assert_eq!(
            message_title(&message, false, Duration::from_secs(0)),
            "error (429)"
        );
    }

    #[test]
    fn mention_picker_lists_skills_and_mcp_servers() {
        let paths = test_paths();
        fs::create_dir_all(paths.wire_dir.join("skills").join("rust-review")).unwrap();
        fs::write(
            paths
                .wire_dir
                .join("skills")
                .join("rust-review")
                .join("SKILL.md"),
            "---\nname: \"rust-review\"\ndescription: \"Review Rust code safely.\"\n---\n\n# Rust Review\n",
        )
        .unwrap();
        fs::write(
            &paths.mcp_file,
            "{\n  \"servers\": [{\"name\":\"context7\",\"command\":\"npx\",\"args\":[\"-y\",\"@upstash/context7-mcp\"]}]\n}\n",
        )
        .unwrap();

        let entries = super::list_mention_picker_entries(&paths, &paths.root_dir, "rust").unwrap();
        assert!(entries.iter().any(|entry| {
            entry.kind == FilePickerEntryKind::Skill
                && entry.mention.as_deref() == Some("rust-review")
        }));

        let entries =
            super::list_mention_picker_entries(&paths, &paths.root_dir, "context").unwrap();
        assert!(entries.iter().any(|entry| {
            entry.kind == FilePickerEntryKind::McpServer
                && entry.mention.as_deref() == Some("context7")
        }));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn mention_picker_ignores_stray_framework_bootstrap_notes() {
        let paths = test_paths();
        fs::write(
            paths.root_dir.join("Next.js"),
            "I see a new Next.js project has been initialized. I will inspect the workspace to understand the current structure and configuration before taking any further action.",
        )
        .unwrap();

        let entries = super::list_mention_picker_entries(&paths, &paths.root_dir, "Next").unwrap();

        assert!(!entries.iter().any(|entry| entry.label == "Next.js"));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn virtual_mentions_resolve_to_skill_context() {
        let paths = test_paths();
        fs::create_dir_all(paths.wire_dir.join("skills").join("rust-review")).unwrap();
        fs::write(
            paths
                .wire_dir
                .join("skills")
                .join("rust-review")
                .join("SKILL.md"),
            "---\nname: \"rust-review\"\ndescription: \"Review Rust code safely.\"\n---\n\n# Rust Review\nUse cargo check.\n",
        )
        .unwrap();

        let mention = resolve_virtual_mention(&paths, "rust-review")
            .unwrap()
            .unwrap();

        assert_eq!(mention.placeholder, "[skill:rust-review]");
        assert!(mention.content.contains("Use cargo check."));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn mention_replacement_updates_only_the_matching_token() {
        let prompt = "compare @src with @rust";
        assert_eq!(
            replace_mention_query(prompt, "rust", "@rust-review"),
            "compare @src with @rust-review"
        );
    }

    fn test_paths() -> AppPaths {
        let root_dir = std::env::temp_dir().join(format!("wirecli-tui-test-{}", next_id()));
        let wire_dir = root_dir.join(".wirecli");
        let config_dir = wire_dir.join("config");
        let data_dir = wire_dir.join("data");
        fs::create_dir_all(&config_dir).unwrap();
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(
            config_dir.join("mcp_servers.json"),
            "{\n  \"servers\": []\n}\n",
        )
        .unwrap();
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }
}

struct SessionPickerApp {
    paths: AppPaths,
    config: AppConfig,
    sessions: Vec<SessionSummary>,
    theme: ThemeConfig,
    selected: usize,
    scroll: usize,
    should_quit: bool,
    chosen: Option<SessionSummary>,
}

impl SessionPickerApp {
    fn new(
        paths: AppPaths,
        config: AppConfig,
        sessions: Vec<SessionSummary>,
        theme: ThemeConfig,
    ) -> Self {
        Self {
            paths,
            config,
            sessions,
            theme,
            selected: 0,
            scroll: 0,
            should_quit: false,
            chosen: None,
        }
    }

    fn run(&mut self) -> Result<Option<SessionSummary>, String> {
        let mut terminal = init_terminal()?;
        let result = self.event_loop(&mut terminal);
        let restore_result = restore_terminal(&mut terminal);
        match (result, restore_result) {
            (Ok(()), Ok(())) => Ok(self.chosen.clone()),
            (Err(err), Ok(())) => Err(err),
            (Ok(()), Err(err)) => Err(err),
            (Err(err), Err(_restore_err)) => Err(err),
        }
    }

    fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<(), String> {
        loop {
            terminal
                .draw(|frame| self.draw(frame))
                .map_err(|e| e.to_string())?;

            if self.should_quit {
                break;
            }

            if event::poll(Duration::from_millis(50)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Key(key) => self.handle_key(key),
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollDown => self.scroll(6),
                        MouseEventKind::ScrollUp => self.scroll(-6),
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Enter => {
                self.chosen = self.sessions.get(self.selected).cloned();
                self.should_quit = true;
            }
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-8),
            KeyCode::PageDown => self.move_selection(8),
            KeyCode::Home => self.set_selection(0),
            KeyCode::End => {
                let last = self.sessions.len().saturating_sub(1);
                self.set_selection(last);
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.sessions.is_empty() {
            return;
        }
        let len = self.sessions.len() as isize;
        let next = (self.selected as isize + delta).clamp(0, len.saturating_sub(1));
        self.set_selection(next as usize);
    }

    fn set_selection(&mut self, selected: usize) {
        self.selected = selected.min(self.sessions.len().saturating_sub(1));
        let visible = 8usize;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll.saturating_add(visible) {
            self.scroll = self.selected.saturating_add(1).saturating_sub(visible);
        }
    }

    fn scroll(&mut self, delta: i16) {
        if delta.is_negative() {
            self.move_selection(-(delta as isize));
        } else {
            self.move_selection(delta as isize);
        }
    }

    fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        let width = area.width.saturating_sub(14).min(92);
        let height = area.height.saturating_sub(10).min(16).max(10);
        let modal = centered_rect(width, height, area);
        frame.render_widget(Clear, modal);

        let title = format!("sessions  ·  {}", self.config.provider_status_label());
        let visible = modal.height.saturating_sub(4).max(1) as usize;
        let start = self.scroll.min(self.sessions.len().saturating_sub(1));
        let end = (start + visible).min(self.sessions.len());
        let slice = &self.sessions[start..end];
        let items = slice
            .iter()
            .map(|session| {
                let label = session
                    .summary
                    .clone()
                    .unwrap_or_else(|| "untitled session".to_string());
                let line = format!("{label}  ·  {}", session.updated_at);
                ListItem::new(Line::from(Span::styled(
                    line,
                    Style::default().fg(self.theme.text),
                )))
            })
            .collect::<Vec<_>>();

        let mut state = ListState::default();
        state.select(Some(
            self.selected
                .saturating_sub(start)
                .min(slice.len().saturating_sub(1)),
        ));

        let list = List::new(items)
            .block(
                Block::default()
                    .title(Line::from(vec![
                        Span::styled(
                            "wirecli",
                            Style::default()
                                .fg(self.theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(title, Style::default().fg(self.theme.muted)),
                    ]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.border))
                    .border_type(BorderType::Plain),
            )
            .highlight_style(selected_style(&self.theme))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, modal, &mut state);

        let hint = if self.sessions.is_empty() {
            "No sessions found"
        } else {
            "enter choose  esc close"
        };
        frame.render_widget(
            Paragraph::new(hint)
                .alignment(Alignment::Center)
                .style(Style::default().fg(self.theme.muted)),
            Rect {
                x: modal.x + 1,
                y: modal.y + modal.height.saturating_sub(2),
                width: modal.width.saturating_sub(2),
                height: 1,
            },
        );
    }
}
