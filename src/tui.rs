use crate::config::{AppConfig, AppPaths, ThemeConfig};
use crate::responses_agent;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers, MouseEventKind};
use crossterm::event::{KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use pulldown_cmark::{Event as MdEvent, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap,
};
use ratatui::Terminal;
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

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
}

enum UiEvent {
    Delta(String),
    ToolStart(String),
    ToolResult { name: String, output: String },
    Done { session_id: String, output: String },
    Error(String),
}

enum Overlay {
    None,
    ModelPicker {
        selected: usize,
        scroll: usize,
    },
    FilePicker {
        query: String,
        matches: Vec<PathBuf>,
        selected: usize,
        scroll: usize,
    },
}

#[derive(Clone)]
struct AttachedFile {
    path: PathBuf,
    label: String,
    content: String,
}

struct PromptPlan {
    display_prompt: String,
    model_prompt: String,
}

struct FilePickerState {
    original_prompt: String,
    pinned: BTreeMap<String, PathBuf>,
}

pub fn run_tui(paths: AppPaths) -> Result<(), String> {
    let config = AppConfig::load(&paths)?;
    let theme = ThemeConfig::load_or_create(&paths.theme_file)?;
    let models = load_models(&config).unwrap_or_else(|_| vec![config.model.clone()]);
    let logo = load_logo(&paths);

    let mut app = App::new(paths, config, models, logo, theme);
    app.run()
}

struct App {
    paths: AppPaths,
    config: AppConfig,
    models: Vec<String>,
    selected_model: usize,
    messages: Vec<ChatMessage>,
    input: String,
    cursor: usize,
    status: String,
    running: bool,
    should_quit: bool,
    rx: Option<mpsc::Receiver<UiEvent>>,
    session_id: Option<String>,
    overlay: Overlay,
    logo: Vec<String>,
    theme: ThemeConfig,
    started_at: Instant,
    pending_prompt: Option<FilePickerState>,
    feed_scroll: u16,
    follow_latest: bool,
}

impl App {
    fn new(
        paths: AppPaths,
        config: AppConfig,
        models: Vec<String>,
        logo: Vec<String>,
        theme: ThemeConfig,
    ) -> Self {
        let selected_model = models
            .iter()
            .position(|model| model == &config.model)
            .unwrap_or(0);
        Self {
            paths,
            config,
            models,
            selected_model,
            messages: Vec::new(),
            input: String::new(),
            cursor: 0,
            status: "ready".to_string(),
            running: false,
            should_quit: false,
            rx: None,
            session_id: None,
            overlay: Overlay::None,
            logo,
            theme,
            started_at: Instant::now(),
            pending_prompt: None,
            feed_scroll: 0,
            follow_latest: true,
        }
    }

    fn run(&mut self) -> Result<(), String> {
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
                    Event::Mouse(mouse) => match mouse.kind {
                        MouseEventKind::ScrollDown => self.scroll_feed(6),
                        MouseEventKind::ScrollUp => self.scroll_feed(-6),
                        _ => {}
                    },
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if self.handle_overlay_key(key) {
            return;
        }

        if self.running {
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                self.should_quit = true;
            }
            return;
        }

        match key.code {
            KeyCode::Esc => {
                if self.input.is_empty() {
                    self.should_quit = true;
                } else {
                    self.input.clear();
                    self.cursor = 0;
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert_newline();
            }
            KeyCode::Enter => self.submit_prompt(),
            KeyCode::PageUp => self.scroll_feed(-6),
            KeyCode::PageDown => self.scroll_feed(6),
            KeyCode::Tab => self.cycle_model(1),
            KeyCode::BackTab => self.cycle_model(-1),
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
                    }
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.input.len() {
                    let next = next_char_boundary(&self.input, self.cursor);
                    if next > self.cursor {
                        self.input.drain(self.cursor..next);
                    }
                }
            }
            KeyCode::Char(ch) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL) {
                    self.input.insert(self.cursor, ch);
                    self.cursor += ch.len_utf8();
                }
            }
            _ => {}
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> bool {
        let overlay = std::mem::replace(&mut self.overlay, Overlay::None);
        match overlay {
            Overlay::None => false,
            Overlay::ModelPicker {
                mut selected,
                mut scroll,
            } => {
                let mut choose: Option<String> = None;
                match key.code {
                    KeyCode::Esc => {}
                    KeyCode::Enter => {
                        choose = self.models.get(selected).cloned();
                    }
                    KeyCode::Up => {
                        if selected > 0 {
                            selected -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if selected + 1 < self.models.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::PageUp => {
                        selected = selected.saturating_sub(8);
                    }
                    KeyCode::PageDown => {
                        selected = (selected + 8).min(self.models.len().saturating_sub(1));
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => {
                        selected = self.models.len().saturating_sub(1);
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
                    self.selected_model = selected;
                    self.config.model = model;
                } else {
                    self.overlay = Overlay::ModelPicker { selected, scroll };
                }
                true
            }
            Overlay::FilePicker {
                query,
                matches,
                mut selected,
                mut scroll,
            } => {
                let mut close = false;
                let mut reopen: Option<(String, Vec<PathBuf>)> = None;
                let mut chosen_path: Option<PathBuf> = None;
                match key.code {
                    KeyCode::Esc => {
                        close = true;
                        self.pending_prompt = None;
                    }
                    KeyCode::Enter => {
                        chosen_path = matches.get(selected).cloned();
                        close = true;
                        self.pending_prompt = None;
                    }
                    KeyCode::Up => {
                        if selected > 0 {
                            selected -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if selected + 1 < matches.len() {
                            selected += 1;
                        }
                    }
                    KeyCode::PageUp => {
                        selected = selected.saturating_sub(8);
                    }
                    KeyCode::PageDown => {
                        selected = (selected + 8).min(matches.len().saturating_sub(1));
                    }
                    KeyCode::Home => selected = 0,
                    KeyCode::End => {
                        selected = matches.len().saturating_sub(1);
                    }
                    _ => {}
                }

                let visible = 8usize;
                if selected < scroll {
                    scroll = selected;
                } else if selected >= scroll.saturating_add(visible) {
                    scroll = selected.saturating_add(1).saturating_sub(visible);
                }

                let had_choice = chosen_path.is_some();
                if let Some(path) = chosen_path.clone() {
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
                                reopen = Some((next.query, next.matches));
                            }
                            Err(err) => {
                                self.messages.push(ChatMessage {
                                    role: MessageRole::System,
                                    title: None,
                                    content: format!("error: {err}"),
                                });
                            }
                        }
                    }
                }

                if let Some((query, matches)) = reopen {
                    self.overlay = Overlay::FilePicker {
                        query,
                        matches,
                        selected: 0,
                        scroll: 0,
                    };
                } else if !close {
                    self.overlay = Overlay::FilePicker {
                        query,
                        matches,
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
        }
    }

    fn submit_prompt(&mut self) {
        let prompt = self.input.trim().to_string();
        if prompt.is_empty() || self.running {
            return;
        }

        if prompt == "/models" {
            self.overlay = Overlay::ModelPicker {
                selected: self.selected_model.min(self.models.len().saturating_sub(1)),
                scroll: 0,
            };
            self.input.clear();
            self.cursor = 0;
            return;
        }

        let pinned = BTreeMap::new();
        match self.build_prompt_plan(&prompt, &pinned) {
            Ok(PromptPlanOutcome::Ready(plan)) => {
                self.start_prompt_submission(plan);
            }
            Ok(PromptPlanOutcome::NeedPicker(next)) => {
                self.pending_prompt = Some(next.state);
                self.overlay = Overlay::FilePicker {
                    query: next.query,
                    matches: next.matches,
                    selected: 0,
                    scroll: 0,
                };
            }
            Err(err) => {
                self.messages.push(ChatMessage {
                    role: MessageRole::System,
                    title: None,
                    content: format!("error: {err}"),
                });
                self.status = "failed".to_string();
            }
        }
    }

    fn start_prompt_submission(&mut self, plan: PromptPlan) {
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
        self.follow_latest = true;
        self.scroll_feed_to_bottom();
        self.input.clear();
        self.cursor = 0;

        let model = self.current_model();
        self.config.model = model;

        let (tx, rx) = mpsc::channel();
        self.rx = Some(rx);
        let paths = self.paths.clone();
        let config = self.config.clone();
        let prompt = plan.model_prompt;

        thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
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
            let result = runtime.block_on(responses_agent::run_prompt_with_observer(
                &paths,
                &config,
                prompt,
                &mut observer,
            ));

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
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }

        for event in events {
            match event {
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
                UiEvent::ToolStart(name) => {
                    self.messages.push(ChatMessage {
                        role: MessageRole::Tool,
                        title: Some(tool_label(&name)),
                        content: String::new(),
                    });
                    if self.follow_latest {
                        self.scroll_feed_to_bottom();
                    }
                }
                UiEvent::ToolResult { name, output } => {
                    if let Some(last) = self.messages.last_mut() {
                        if matches!(last.role, MessageRole::Tool) {
                            last.content = output;
                            if last.title.is_none() {
                                last.title = Some(tool_label(&name));
                            }
                        }
                    }
                    if self.follow_latest {
                        self.scroll_feed_to_bottom();
                    }
                }
                UiEvent::Done { session_id, output } => {
                    self.session_id = Some(session_id.clone());
                    if let Some(last) = self.messages.last_mut() {
                        if matches!(last.role, MessageRole::Assistant) && last.content.is_empty() {
                            last.content = output;
                        }
                    }
                    self.status = format!("session {}", compact_session_id(&session_id));
                    self.running = false;
                    self.rx = None;
                    self.follow_latest = true;
                    self.scroll_feed_to_bottom();
                }
                UiEvent::Error(err) => {
                    self.messages.push(ChatMessage {
                        role: MessageRole::System,
                        title: None,
                        content: format!("error: {err}"),
                    });
                    self.status = "failed".to_string();
                    self.running = false;
                    self.rx = None;
                    self.follow_latest = true;
                    self.scroll_feed_to_bottom();
                }
            }
        }
    }

    fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        let area = frame.area();
        if self.messages.is_empty() {
            self.draw_welcome(frame, area);
            self.draw_prompt(frame, welcome_prompt_rect(area));
        } else {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(prompt_height())])
                .split(area);
            self.draw_feed(frame, layout[0]);
            self.draw_prompt(frame, layout[1]);
        }

        match &self.overlay {
            Overlay::None => {}
            Overlay::ModelPicker { selected, scroll } => {
                self.draw_model_picker(frame, *selected, *scroll);
            }
            Overlay::FilePicker {
                query,
                matches,
                selected,
                scroll,
            } => {
                self.draw_file_picker(frame, query, matches, *selected, *scroll);
            }
        }
    }

    fn draw_welcome(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let logo_height = self.logo.len() as u16;
        let logo_area = centered_rect(area.width.saturating_sub(12).min(92), logo_height + 2, area);
        let logo_lines = self
            .logo
            .iter()
            .map(|line| Line::from(Span::styled(line.clone(), Style::default().fg(Color::Blue))))
            .collect::<Vec<_>>();
        let logo = Paragraph::new(logo_lines)
            .alignment(Alignment::Center)
            .block(Block::default());
        frame.render_widget(logo, logo_area);

        let hint = Paragraph::new(Line::from(vec![
            Span::styled(
                "Enter",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" to send  "),
            Span::styled(
                "Shift+Enter",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" to break line  "),
            Span::styled(
                "/models",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" to switch  "),
            Span::styled(
                "@file",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" to attach"),
        ]))
        .alignment(Alignment::Center)
        .style(Style::default().fg(Color::DarkGray));
        let hint_area = Rect {
            x: area.x + 2,
            y: logo_area.y + logo_area.height + 1,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(hint, hint_area);
    }

    fn draw_feed(&self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        let mut lines = Vec::new();
        for (idx, message) in self.messages.iter().enumerate() {
            let is_last_assistant = self.running
                && idx + 1 == self.messages.len()
                && matches!(message.role, MessageRole::Assistant)
                && message.content.is_empty();
            let card_title = message_title(message, is_last_assistant, self.started_at.elapsed());
            lines.extend(render_card(
                &card_title,
                self.theme.accent,
                body_color(&message.role, &self.theme),
                &message.content,
                matches!(message.role, MessageRole::Assistant | MessageRole::Tool),
                matches!(message.role, MessageRole::Tool)
                    && message.title.as_deref() == Some("Patched"),
                area.width as usize,
                &self.theme,
            ));
            lines.push(Line::from(""));
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(self.theme.border))
            .border_type(BorderType::Rounded);
        let viewport_rows = area.height.saturating_sub(2);
        let max_scroll = lines.len().saturating_sub(viewport_rows as usize) as u16;
        let scroll_rows = self.feed_scroll.min(max_scroll);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((scroll_rows, 0)),
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
        let title = format!(
            "prompt  · {}  · {}",
            self.config.provider,
            self.current_model()
        );
        let text = if self.input.is_empty() {
            vec![Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "type a prompt",
                    Style::default().fg(self.theme.muted),
                ),
            ])]
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
                            "riftcli ",
                            Style::default()
                                .fg(self.theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(title, Style::default().fg(self.theme.muted)),
                    ]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.border))
                    .border_type(BorderType::Rounded),
            )
            .wrap(Wrap { trim: false })
            .alignment(Alignment::Left);

        frame.render_widget(input, prompt_area);

        if !self.running {
            let (cursor_row, cursor_col) = cursor_position(&self.input, self.cursor);
            let cursor_x =
                input_area.x + cursor_col.min(input_area.width.saturating_sub(1) as usize) as u16;
            let cursor_y =
                input_area.y + cursor_row.min(input_area.height.saturating_sub(1) as usize) as u16;
            frame.set_cursor_position((cursor_x, cursor_y));
        }
    }

    fn draw_model_picker(&self, frame: &mut ratatui::Frame<'_>, selected: usize, scroll: usize) {
        let width = self
            .models
            .iter()
            .map(|m| m.len())
            .max()
            .unwrap_or(12)
            .saturating_add(10)
            .min(72) as u16;
        let height = (self.models.len().min(10) as u16).saturating_add(4);
        let area = centered_rect(width, height, frame.area());
        frame.render_widget(Clear, area);

        let visible = area.height.saturating_sub(4).max(1) as usize;
        let start = scroll.min(self.models.len().saturating_sub(1));
        let end = (start + visible).min(self.models.len());
        let slice = &self.models[start..end];

        let items = slice
            .iter()
            .map(|model| {
                ListItem::new(Line::from(Span::styled(
                    model.clone(),
                    Style::default().fg(Color::White),
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
                            "models",
                            Style::default()
                                .fg(self.theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  use arrows and enter"),
                    ]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.border))
                    .border_type(BorderType::Rounded),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▸ ");
        frame.render_stateful_widget(list, area, &mut state);

        let hint = Paragraph::new("Esc close")
            .alignment(Alignment::Right)
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

    fn draw_file_picker(
        &self,
        frame: &mut ratatui::Frame<'_>,
        query: &str,
        matches: &[PathBuf],
        selected: usize,
        scroll: usize,
    ) {
        let width = frame.area().width.saturating_sub(10).min(100).max(50);
        let height = (matches.len().min(10) as u16).saturating_add(5);
        let area = centered_rect(width, height, frame.area());
        frame.render_widget(Clear, area);

        let visible = area.height.saturating_sub(4).max(1) as usize;
        let start = scroll.min(matches.len().saturating_sub(1));
        let end = (start + visible).min(matches.len());
        let slice = &matches[start..end];

        let items = slice
            .iter()
            .map(|path| {
                let rel = path
                    .strip_prefix(&self.paths.root_dir)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| path.display().to_string());
                ListItem::new(Line::from(Span::styled(
                    rel,
                    Style::default().fg(Color::White),
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
                            "attach",
                            Style::default()
                                .fg(self.theme.accent)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw("  "),
                        Span::styled(format!("@{query}"), Style::default().fg(self.theme.muted)),
                    ]))
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(self.theme.border))
                    .border_type(BorderType::Rounded),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(self.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▸ ");
        frame.render_stateful_widget(list, area, &mut state);

        let hint = Paragraph::new("Enter choose  ·  Esc cancel")
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

    fn insert_newline(&mut self) {
        if self.running {
            return;
        }
        self.input.insert(self.cursor, '\n');
        self.cursor += '\n'.len_utf8();
    }

    fn scroll_feed(&mut self, delta: i16) {
        if self.messages.is_empty() {
            return;
        }
        if delta.is_negative() {
            self.follow_latest = false;
        }
        let next = if delta.is_negative() {
            self.feed_scroll.saturating_sub(delta.wrapping_abs() as u16)
        } else {
            self.feed_scroll.saturating_add(delta as u16)
        };
        self.feed_scroll = next;
    }

    fn scroll_feed_to_bottom(&mut self) {
        self.feed_scroll = u16::MAX;
        self.follow_latest = true;
    }

    fn current_model(&self) -> String {
        self.models
            .get(self.selected_model)
            .cloned()
            .unwrap_or_else(|| self.config.model.clone())
    }

    fn cycle_model(&mut self, step: isize) {
        if self.models.is_empty() {
            return;
        }
        let len = self.models.len() as isize;
        let next = (self.selected_model as isize + step).rem_euclid(len);
        self.selected_model = next as usize;
        self.config.model = self.current_model();
    }

    fn build_prompt_plan(
        &self,
        prompt: &str,
        pinned: &BTreeMap<String, PathBuf>,
    ) -> Result<PromptPlanOutcome, String> {
        let mut display_prompt = prompt.to_string();
        let mut model_prompt = prompt.to_string();
        let mut attachments = Vec::new();

        for token in prompt.split_whitespace() {
            if let Some(query) = token.strip_prefix('@') {
                let query = sanitize_attachment_query(query);
                if query.is_empty() {
                    continue;
                }

                if let Some(path) = pinned.get(&query) {
                    let attachment = load_attachment(&self.paths.root_dir, path.clone())?;
                    let placeholder = format!("[{}]", attachment.label);
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
                        let placeholder = format!("[{}]", attachment.label);
                        display_prompt = display_prompt.replace(token, &placeholder);
                        model_prompt = model_prompt.replace(token, &placeholder);
                        attachments.push(attachment);
                    }
                    _ => {
                        return Ok(PromptPlanOutcome::NeedPicker(NeedPicker {
                            state: FilePickerState {
                                original_prompt: prompt.to_string(),
                                pinned: pinned.clone(),
                            },
                            query,
                            matches,
                        }));
                    }
                }
            }
        }

        if !attachments.is_empty() {
            model_prompt.push_str("\n\nAttached files:\n");
            for attachment in attachments {
                model_prompt.push_str("\n[File: ");
                model_prompt.push_str(&attachment.label);
                model_prompt.push_str("]\n```text\n");
                model_prompt.push_str(&attachment.content);
                model_prompt.push_str("\n```\n");
            }
        }

        Ok(PromptPlanOutcome::Ready(PromptPlan {
            display_prompt,
            model_prompt,
        }))
    }
}

struct NeedPicker {
    state: FilePickerState,
    query: String,
    matches: Vec<PathBuf>,
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
            responses_agent::AgentEvent::TextDelta(delta) => {
                let _ = self.tx.send(UiEvent::Delta(delta.to_string()));
            }
            responses_agent::AgentEvent::ToolCallStart { name } => {
                let _ = self.tx.send(UiEvent::ToolStart(name.to_string()));
            }
            responses_agent::AgentEvent::ToolCallResult { name, output } => {
                let _ = self.tx.send(UiEvent::ToolResult {
                    name: name.to_string(),
                    output: output.to_string(),
                });
            }
        }
    }
}

fn load_models(config: &AppConfig) -> Result<Vec<String>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let url = format!("{}/models", config.base_url.trim_end_matches('/'));
        let response = reqwest::Client::new()
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let value: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("models endpoint returned {}: {}", status, value));
        }
        let mut models = Vec::new();
        if let Some(data) = value.get("data").and_then(|v| v.as_array()) {
            for model in data {
                if let Some(id) = model.get("id").and_then(|v| v.as_str()) {
                    models.push(id.to_string());
                }
            }
        }
        if models.is_empty() {
            models.push(config.model.clone());
        }
        Ok(models)
    })
}

fn load_logo(paths: &AppPaths) -> Vec<String> {
    let logo_path = paths.root_dir.join("ascii.md");
    match fs::read_to_string(logo_path) {
        Ok(content) => content.lines().map(|line| line.to_string()).collect(),
        Err(_) => vec!["RIFT".to_string(), "CLI".to_string()],
    }
}

fn prompt_height() -> u16 {
    6
}

fn welcome_prompt_rect(area: Rect) -> Rect {
    let lower_half = Rect {
        x: area.x,
        y: area.y + area.height / 2,
        width: area.width,
        height: area.height.saturating_sub(area.height / 2),
    };
    centered_rect(
        area.width.saturating_sub(8).min(88),
        prompt_height(),
        lower_half,
    )
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

fn spinner(elapsed: Duration) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
    let index = ((elapsed.as_millis() / 80) as usize) % FRAMES.len();
    FRAMES[index]
}

fn message_title(message: &ChatMessage, thinking: bool, elapsed: Duration) -> String {
    match message.role {
        MessageRole::User => message.title.clone().unwrap_or_else(|| "you".to_string()),
        MessageRole::Assistant => {
            if thinking {
                format!("rift {}", spinner(elapsed))
            } else {
                message.title.clone().unwrap_or_else(|| "rift".to_string())
            }
        }
        MessageRole::Tool => message.title.clone().unwrap_or_else(|| "tool".to_string()),
        MessageRole::System => "system".to_string(),
    }
}

fn role_color(role: &MessageRole) -> Color {
    match role {
        MessageRole::User => Color::Cyan,
        MessageRole::Assistant => Color::Blue,
        MessageRole::Tool => Color::LightBlue,
        MessageRole::System => Color::DarkGray,
    }
}

fn body_color(role: &MessageRole, theme: &ThemeConfig) -> Color {
    match role {
        MessageRole::User => theme.user_text,
        MessageRole::Assistant => theme.assistant_text,
        MessageRole::Tool => theme.tool_text,
        MessageRole::System => theme.muted,
    }
}

fn tool_label(name: &str) -> String {
    match name {
        "shell" => "Executed".to_string(),
        "apply_patch" => "Patched".to_string(),
        "list_dir" => "Listed".to_string(),
        "read_file" => "Read".to_string(),
        "write_file" => "Wrote".to_string(),
        "search" => "Searched".to_string(),
        "remember" => "Remembered".to_string(),
        "recall" => "Recalled".to_string(),
        other => other.to_string(),
    }
}

fn render_card(
    title: &str,
    color: Color,
    body_color: Color,
    content: &str,
    markdown: bool,
    diff_mode: bool,
    width: usize,
    theme: &ThemeConfig,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            "╭ ",
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            title.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ]));

    let body = if diff_mode {
        render_diff_lines(content, body_color)
    } else if markdown {
        render_markdown_lines(content, body_color, theme)
    } else {
        render_plain_lines(content, body_color)
    };

    if body.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("│ ", Style::default().fg(color)),
            Span::raw(" "),
        ]));
    } else {
        for line in wrap_rendered_lines(body, width.saturating_sub(4)) {
            let mut spans = vec![Span::styled("│ ", Style::default().fg(color))];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }

    lines.push(Line::from(vec![
        Span::styled("╰", Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(
            "─".repeat(width.saturating_sub(2).max(8)),
            Style::default().fg(theme.muted),
        ),
    ]));
    lines
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

fn render_diff_lines(content: &str, _color: Color) -> Vec<Line<'static>> {
    content
        .lines()
        .map(|line| {
            if line.starts_with('+') {
                Line::from(vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Green),
                )])
            } else if line.starts_with('-') {
                Line::from(vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Red),
                )])
            } else if line.starts_with("@@") {
                Line::from(vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Yellow),
                )])
            } else {
                Line::from(vec![Span::styled(
                    line.to_string(),
                    Style::default().fg(Color::Gray),
                )])
            }
        })
        .collect()
}

fn render_markdown_lines(content: &str, color: Color, theme: &ThemeConfig) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let parser = Parser::new_ext(content, Options::all());
    let mut current = Vec::<Span<'static>>::new();
    let mut in_code = false;
    let mut list_level = 0usize;

    for event in parser {
        match event {
            MdEvent::Start(tag) => match tag {
                Tag::Paragraph => {
                    if !current.is_empty() {
                        lines.push(Line::from(current.clone()));
                        current.clear();
                    }
                }
                Tag::Heading { level, .. } => {
                    if !current.is_empty() {
                        lines.push(Line::from(current.clone()));
                        current.clear();
                    }
                    current.push(Span::styled(
                        heading_prefix(level),
                        Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ));
                }
                Tag::CodeBlock(_) => {
                    if !current.is_empty() {
                        lines.push(Line::from(current.clone()));
                        current.clear();
                    }
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
                _ => {}
            },
            MdEvent::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) => {
                    if !current.is_empty() {
                        lines.push(Line::from(current.clone()));
                        current.clear();
                    }
                }
                TagEnd::CodeBlock => {
                    in_code = false;
                    if !current.is_empty() {
                        lines.push(Line::from(current.clone()));
                        current.clear();
                    }
                }
                TagEnd::List(_) => {
                    list_level = list_level.saturating_sub(1);
                    if !current.is_empty() {
                        lines.push(Line::from(current.clone()));
                        current.clear();
                    }
                }
                TagEnd::Item => {
                    if !current.is_empty() {
                        lines.push(Line::from(current.clone()));
                        current.clear();
                    }
                }
                _ => {}
            },
            MdEvent::Text(text) => {
                let style = if in_code {
                    Style::default().fg(theme.text)
                } else {
                    Style::default().fg(color)
                };
                for part in text.lines() {
                    if !current.is_empty() && part.is_empty() {
                        lines.push(Line::from(current.clone()));
                        current.clear();
                        continue;
                    }
                    if !current.is_empty() {
                        current.push(Span::raw(" "));
                    }
                    current.extend(parse_inline_spans(part, style, theme));
                    if in_code {
                        lines.push(Line::from(current.clone()));
                        current.clear();
                    }
                }
            }
            MdEvent::Code(code) => {
                current.push(Span::styled(
                    format!("`{code}`"),
                    Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
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

    if !current.is_empty() {
        lines.push(Line::from(current));
    }

    lines
}

fn parse_inline_spans(text: &str, base_style: Style, theme: &ThemeConfig) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buffer = String::new();
    let mut chars = text.chars().peekable();
    let mut bold = false;
    let mut code = false;

    while let Some(ch) = chars.next() {
        if ch == '`' {
            if !buffer.is_empty() {
                spans.push(Span::styled(buffer.clone(), current_style(base_style, bold, code, theme)));
                buffer.clear();
            }
            code = !code;
            continue;
        }

        if !code && ch == '*' && chars.peek() == Some(&'*') {
            chars.next();
            if !buffer.is_empty() {
                spans.push(Span::styled(buffer.clone(), current_style(base_style, bold, code, theme)));
                buffer.clear();
            }
            bold = !bold;
            continue;
        }

        buffer.push(ch);
    }

    if !buffer.is_empty() {
        spans.push(Span::styled(buffer, current_style(base_style, bold, code, theme)));
    }

    spans
}

fn current_style(base: Style, bold: bool, code: bool, theme: &ThemeConfig) -> Style {
    let mut style = base;
    if code {
        style = style.fg(theme.muted);
    } else if bold {
        style = style.fg(theme.emphasis).add_modifier(Modifier::BOLD);
    }
    style
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

fn heading_prefix(level: HeadingLevel) -> &'static str {
    match level {
        HeadingLevel::H1 => "# ",
        HeadingLevel::H2 => "## ",
        HeadingLevel::H3 => "### ",
        HeadingLevel::H4 => "#### ",
        HeadingLevel::H5 => "##### ",
        HeadingLevel::H6 => "###### ",
    }
}

fn load_attachment(root: &Path, path: PathBuf) -> Result<AttachedFile, String> {
    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let label = path
        .strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    Ok(AttachedFile {
        path,
        label,
        content,
    })
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
    matches!(name, ".git" | ".rift" | ".riftcode" | "target" | "node_modules")
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

fn init_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>, String> {
    enable_raw_mode().map_err(|e| e.to_string())?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| e.to_string())?;
    execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .map_err(|e| e.to_string())?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).map_err(|e| e.to_string())
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<(), String> {
    disable_raw_mode().map_err(|e| e.to_string())?;
    execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags).map_err(|e| e.to_string())?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(|e| e.to_string())?;
    terminal.show_cursor().map_err(|e| e.to_string())
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, width, height)
}
