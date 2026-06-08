use crate::agent_tools::BoxTools;
use crate::approvals::{ApprovalState, ApprovalStore};
use crate::commands::parser::split_command_line;
use crate::config::{AppConfig, AppPaths};
use crate::hooks::{HookMatchMode, HookStore};
use crate::mcp::McpRegistry;
use crate::mcp::McpServerConfig;
use crate::memory::AnchorStore;
use crate::model_catalog;
use crate::models::{compact_number, estimated_tokens};
use crate::policy::CommandPolicy;
use crate::providers::{apply_provider_preset, available_providers};
use crate::responses_agent;
use crate::sandbox::SandboxManager;
use crate::session::SessionStore;
use crate::skills::SkillStore;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Command {
    Tui,
    Sessions,
    Resume {
        session_id: Option<String>,
    },
    Share {
        session_id: Option<String>,
        path: Option<String>,
    },
    Login,
    Status,
    Models,
    Providers {
        provider_id: Option<String>,
    },
    Mcp,
    Mcpc,
    Approvals {
        action: ApprovalsAction,
    },
    Hooks {
        action: HooksAction,
    },
    Skills {
        action: SkillsAction,
    },
    Harness {
        action: crate::harness::HarnessAction,
    },
    Run {
        prompt: String,
    },
    #[cfg(debug_assertions)]
    DevPath {
        path: String,
    },
    Lattice {
        action: LatticeAction,
    },
    Help,
    Version,
}

#[derive(Debug, Clone)]
pub enum LatticeAction {
    New {
        name: String,
    },
    List,
    Run {
        cell_id: String,
        command: Vec<String>,
    },
    Tools,
}

#[derive(Debug, Clone)]
pub enum SkillsAction {
    List,
    Read {
        name: String,
    },
    Create {
        name: String,
        description: String,
        body: String,
    },
}

#[derive(Debug, Clone)]
pub enum HooksAction {
    List,
    Add {
        event: String,
        command: Vec<String>,
        match_command: Option<String>,
        match_mode: HookMatchMode,
        match_tool: Option<String>,
        match_status: Option<String>,
        match_path: Option<String>,
    },
    Remove {
        id: String,
    },
}

#[derive(Debug, Clone)]
pub enum ApprovalsAction {
    List,
    AllowOnce { id: String },
    AllowRepo { id: String },
    Deny { id: String },
    DenyAlways { id: String },
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub command: Command,
}

pub fn parse_args(args: Vec<String>) -> Cli {
    if args.first().map(|s| s.as_str()) == Some("wirecli") {
        return parse_args(args.into_iter().skip(1).collect());
    }

    let command = match args.first().map(|s| s.as_str()) {
        None => Command::Tui,
        Some("-h") | Some("--help") => Command::Help,
        Some("-V") | Some("--version") => Command::Version,
        Some("tui") => Command::Tui,
        Some("sessions") => Command::Sessions,
        Some("resume") => Command::Resume {
            session_id: args.get(1).cloned(),
        },
        Some("share") => Command::Share {
            session_id: args.get(1).cloned(),
            path: args.get(2).cloned(),
        },
        Some("login") => Command::Login,
        Some("status") => Command::Status,
        Some("models") => Command::Models,
        Some("providers") | Some("provider") => Command::Providers {
            provider_id: args.get(1).cloned(),
        },
        Some("mcp") => Command::Mcp,
        Some("mcpc") => Command::Mcpc,
        Some("approvals") | Some("approval") => parse_approvals_command(&args[1..]),
        Some("hooks") | Some("hook") => parse_hooks_command(&args[1..]),
        Some("skills") | Some("skill") => parse_skills_command(&args[1..]),
        Some("harness") => Command::Harness {
            action: crate::harness::parse_action(&args[1..]),
        },
        Some("box") => parse_lattice_command(&args[1..]),
        #[cfg(debug_assertions)]
        Some("path") => Command::DevPath {
            path: args.get(1).cloned().unwrap_or_default(),
        },
        Some("run") => {
            let prompt = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            Command::Run { prompt }
        }
        _ => Command::Help,
    };

    Cli { command }
}

fn parse_lattice_command(args: &[String]) -> Command {
    match args.first().map(|s| s.as_str()) {
        Some("new") | Some("init") => {
            let name = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            Command::Lattice {
                action: LatticeAction::New { name },
            }
        }
        Some("list") | None => Command::Lattice {
            action: LatticeAction::List,
        },
        Some("run") | Some("exec") => {
            let cell_id = args.get(1).cloned().unwrap_or_default();
            let command = parse_command_tail(&args[2..]);
            Command::Lattice {
                action: LatticeAction::Run { cell_id, command },
            }
        }
        Some("tools") | Some("toolbox") => Command::Lattice {
            action: LatticeAction::Tools,
        },
        _ => Command::Help,
    }
}

fn parse_skills_command(args: &[String]) -> Command {
    match args.first().map(|s| s.as_str()) {
        None | Some("list") => Command::Skills {
            action: SkillsAction::List,
        },
        Some("read") => Command::Skills {
            action: SkillsAction::Read {
                name: args.get(1).cloned().unwrap_or_default(),
            },
        },
        Some("create") => {
            let name = args.get(1).cloned().unwrap_or_default();
            let description = args.get(2).cloned().unwrap_or_default();
            let body = args.iter().skip(3).cloned().collect::<Vec<_>>().join(" ");
            Command::Skills {
                action: SkillsAction::Create {
                    name,
                    description,
                    body,
                },
            }
        }
        _ => Command::Help,
    }
}

fn parse_hooks_command(args: &[String]) -> Command {
    match args.first().map(|s| s.as_str()) {
        None | Some("list") => Command::Hooks {
            action: HooksAction::List,
        },
        Some("add") => {
            let event = args.get(1).cloned().unwrap_or_default();
            let mut command = Vec::new();
            let mut match_command = None;
            let mut match_mode = HookMatchMode::Exact;
            let mut match_tool = None;
            let mut match_status = None;
            let mut match_path = None;
            let mut index = 2;
            while index < args.len() {
                match args[index].as_str() {
                    "--" if command.is_empty() => {
                        command.extend(args.iter().skip(index + 1).cloned());
                        break;
                    }
                    "--match-command" => {
                        if let Some(value) = args.get(index + 1) {
                            match_command = Some(value.clone());
                        }
                        index += 2;
                    }
                    "--match-mode" => {
                        match_mode =
                            HookMatchMode::from_value(args.get(index + 1).map(String::as_str));
                        index += 2;
                    }
                    "--match-tool" => {
                        if let Some(value) = args.get(index + 1) {
                            match_tool = Some(value.clone());
                        }
                        index += 2;
                    }
                    "--match-status" => {
                        if let Some(value) = args.get(index + 1) {
                            match_status = Some(value.clone());
                        }
                        index += 2;
                    }
                    "--match-path" => {
                        if let Some(value) = args.get(index + 1) {
                            match_path = Some(value.clone());
                        }
                        index += 2;
                    }
                    value => {
                        command.push(value.to_string());
                        index += 1;
                    }
                }
            }
            Command::Hooks {
                action: HooksAction::Add {
                    event,
                    command,
                    match_command,
                    match_mode,
                    match_tool,
                    match_status,
                    match_path,
                },
            }
        }
        Some("remove") => Command::Hooks {
            action: HooksAction::Remove {
                id: args.get(1).cloned().unwrap_or_default(),
            },
        },
        _ => Command::Help,
    }
}

fn parse_approvals_command(args: &[String]) -> Command {
    match args.first().map(|s| s.as_str()) {
        None | Some("list") | Some("ls") => Command::Approvals {
            action: ApprovalsAction::List,
        },
        Some("allow-once") | Some("once") => Command::Approvals {
            action: ApprovalsAction::AllowOnce {
                id: args.get(1).cloned().unwrap_or_default(),
            },
        },
        Some("allow-repo") | Some("repo") => Command::Approvals {
            action: ApprovalsAction::AllowRepo {
                id: args.get(1).cloned().unwrap_or_default(),
            },
        },
        Some("deny") => Command::Approvals {
            action: ApprovalsAction::Deny {
                id: args.get(1).cloned().unwrap_or_default(),
            },
        },
        Some("deny-always") | Some("never") => Command::Approvals {
            action: ApprovalsAction::DenyAlways {
                id: args.get(1).cloned().unwrap_or_default(),
            },
        },
        _ => Command::Help,
    }
}

fn parse_command_tail(args: &[String]) -> Vec<String> {
    if let Some(pos) = args.iter().position(|arg| arg == "--") {
        return args.iter().skip(pos + 1).cloned().collect();
    }
    args.to_vec()
}

pub fn run(cli: Cli) -> Result<(), String> {
    #[cfg(debug_assertions)]
    if let Command::DevPath { path } = &cli.command {
        return run_dev_path(&path);
    }

    if let Command::Harness {
        action: crate::harness::HarnessAction::Help,
    } = &cli.command
    {
        crate::harness::print_help();
        return Ok(());
    }

    let paths = AppPaths::detect()?;

    match cli.command {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Version => {
            println!("wirecli 0.1.1");
            Ok(())
        }
        Command::Tui => crate::tui::run_tui(paths),
        Command::Sessions => crate::tui::run_sessions_tui(paths),
        Command::Resume { session_id } => resume_command(&paths, session_id),
        Command::Share { session_id, path } => share_command(&paths, session_id, path),
        Command::Login => login_command(&paths),
        Command::Status => status_command(&paths),
        Command::Models => list_models(&paths),
        Command::Providers { provider_id } => providers_command(&paths, provider_id),
        Command::Mcp => list_mcp(&paths),
        Command::Mcpc => configure_mcp(&paths),
        Command::Approvals { action } => approvals_command(&paths, action),
        Command::Hooks { action } => hooks_command(&paths, action),
        Command::Skills { action } => skills_command(&paths, action),
        Command::Harness { action } => crate::harness::run(&paths, action),
        Command::Run { prompt } => run_prompt(&paths, prompt),
        Command::Lattice { action } => lattice_command(&paths, action),
        #[cfg(debug_assertions)]
        Command::DevPath { .. } => {
            unreachable!("dev path command is handled before path detection")
        }
    }
}

#[cfg(debug_assertions)]
fn run_dev_path(path: &str) -> Result<(), String> {
    let path = path.trim();
    if path.is_empty() {
        return Err("missing path; use `cargo run -- wirecli path <directory>`".to_string());
    }
    let path = expand_user_path(path)?;
    if !path.exists() {
        return Err(format!("path does not exist: {}", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("path is not a directory: {}", path.display()));
    }
    std::env::set_current_dir(&path)
        .map_err(|e| format!("failed to enter {}: {e}", path.display()))?;
    crate::tui::run_tui(AppPaths::detect()?)
}

#[cfg(debug_assertions)]
fn expand_user_path(path: &str) -> Result<PathBuf, String> {
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        return Ok(home_dir()?.join(rest));
    }
    Ok(PathBuf::from(path))
}

#[cfg(debug_assertions)]
fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| "could not expand `~`; HOME/USERPROFILE is not set".to_string())
}

fn hooks_command(paths: &AppPaths, action: HooksAction) -> Result<(), String> {
    let store = HookStore::new(paths)?;
    match action {
        HooksAction::List => {
            let hooks = store.list()?;
            if hooks.is_empty() {
                println!("No hooks yet.");
            } else {
                for hook in hooks {
                    let mut filters = Vec::new();
                    if let Some(match_command) = hook.match_command.as_deref() {
                        filters.push(format!(
                            "command:{}:{}",
                            hook.match_mode.as_deref().unwrap_or("exact"),
                            match_command
                        ));
                    }
                    if let Some(match_tool) = hook.match_tool.as_deref() {
                        filters.push(format!("tool:{match_tool}"));
                    }
                    if let Some(match_status) = hook.match_status.as_deref() {
                        filters.push(format!("status:{match_status}"));
                    }
                    if let Some(match_path) = hook.match_path.as_deref() {
                        filters.push(format!(
                            "path:{}:{}",
                            hook.match_mode.as_deref().unwrap_or("exact"),
                            match_path
                        ));
                    }
                    if filters.is_empty() {
                        println!("{}  {}  {}", hook.id, hook.event, hook.command.join(" "));
                    } else {
                        println!(
                            "{}  {}  [{}]  {}",
                            hook.id,
                            hook.event,
                            filters.join(","),
                            hook.command.join(" ")
                        );
                    }
                }
            }
        }
        HooksAction::Add {
            event,
            command,
            match_command,
            match_mode,
            match_tool,
            match_status,
            match_path,
        } => {
            let hook = store.add_scoped(
                &event,
                command,
                match_command,
                match_mode,
                match_tool,
                match_status,
                match_path,
            )?;
            println!("added hook: {}", hook.id);
            println!("event: {}", hook.event);
            if let Some(match_command) = hook.match_command.as_deref() {
                println!(
                    "match: {} ({})",
                    match_command,
                    hook.match_mode.as_deref().unwrap_or("exact")
                );
            }
            if let Some(match_tool) = hook.match_tool.as_deref() {
                println!("match_tool: {match_tool}");
            }
            if let Some(match_status) = hook.match_status.as_deref() {
                println!("match_status: {match_status}");
            }
            if let Some(match_path) = hook.match_path.as_deref() {
                println!("match_path: {match_path}");
            }
            println!("command: {}", hook.command.join(" "));
        }
        HooksAction::Remove { id } => {
            if store.remove(&id)? {
                println!("removed hook: {id}");
            } else {
                return Err(format!("hook not found: {id}"));
            }
        }
    }
    Ok(())
}

fn approvals_command(paths: &AppPaths, action: ApprovalsAction) -> Result<(), String> {
    let store = ApprovalStore::new(paths)?;
    match action {
        ApprovalsAction::List => {
            let records = store.list()?;
            if records.is_empty() {
                println!("No approval requests yet.");
            } else {
                for record in records {
                    println!(
                        "{}  {}  {}  {}",
                        record.id,
                        record.state.as_str(),
                        record.risk,
                        record.command.join(" ")
                    );
                    println!("    {}", record.explanation);
                    if !record.reason.trim().is_empty() {
                        println!("    reason: {}", record.reason);
                    }
                }
            }
        }
        ApprovalsAction::AllowOnce { id } => {
            let record = store.decide(&id, ApprovalState::AllowOnce)?;
            println!("allowed once: {}", record.id);
        }
        ApprovalsAction::AllowRepo { id } => {
            let record = store.decide(&id, ApprovalState::AllowRepo)?;
            println!("allowed in this repo: {}", record.id);
        }
        ApprovalsAction::Deny { id } => {
            let record = store.decide(&id, ApprovalState::Denied)?;
            println!("denied: {}", record.id);
        }
        ApprovalsAction::DenyAlways { id } => {
            let record = store.decide(&id, ApprovalState::DenyAlways)?;
            println!("denied always in this repo: {}", record.id);
        }
    }
    Ok(())
}

fn skills_command(paths: &AppPaths, action: SkillsAction) -> Result<(), String> {
    let store = SkillStore::new(paths)?;
    match action {
        SkillsAction::List => {
            let records = store.list()?;
            if records.is_empty() {
                println!("No local skills yet.");
            } else {
                for record in records {
                    println!("{}", record.name);
                    if !record.description.trim().is_empty() {
                        println!("  {}", record.description);
                    }
                    println!("  {}", record.path.display());
                }
            }
        }
        SkillsAction::Read { name } => {
            let record = store.read(&name)?;
            println!("{}", record.name);
            println!("{}", record.path.display());
            println!("{}", record.description);
            println!("{}", record.body);
        }
        SkillsAction::Create {
            name,
            description,
            body,
        } => {
            let record = store.create(&name, &description, &body)?;
            println!("created: {}", record.name);
            println!("{}", record.path.display());
        }
    }
    Ok(())
}

fn providers_command(paths: &AppPaths, provider_id: Option<String>) -> Result<(), String> {
    let mut config = AppConfig::load(paths)?;
    if let Some(provider_id) = provider_id {
        let profile = apply_provider_preset(&mut config, &provider_id)?;
        config.save(paths)?;
        println!("provider: {}", profile.id);
        println!("base_url: {}", profile.base_url);
        println!("model: {}", profile.default_model);
        println!("protocol: {}", profile.protocol.as_str());
        if let Some(env_name) = profile.api_key_env {
            println!("api_key_env: {env_name}");
        }
        return Ok(());
    }

    for profile in available_providers(&config) {
        let marker = if profile.id == config.provider {
            "*"
        } else {
            " "
        };
        println!(
            "{marker} {}  {}  {}",
            profile.id,
            profile.default_model,
            profile.protocol.as_str()
        );
        println!("    {}", profile.base_url);
        if let Some(env_name) = profile.api_key_env.as_deref() {
            println!("    key env: {env_name}");
        }
        for note in profile.notes {
            println!("    note: {note}");
        }
    }
    Ok(())
}

fn login_command(paths: &AppPaths) -> Result<(), String> {
    crate::tui::run_login_tui(paths.clone())
}

fn status_command(paths: &AppPaths) -> Result<(), String> {
    let config = AppConfig::load(paths)?;
    println!("provider: {}", config.provider_status_label());
    println!("protocol: {}", config.protocol.as_str());
    println!("account: {}", config.account_summary());
    println!(
        "base_url: {}",
        if config.base_url.trim().is_empty() {
            "not configured"
        } else {
            config.base_url.as_str()
        }
    );
    println!(
        "model: {}",
        if config.model.trim().is_empty() {
            "not configured"
        } else {
            config.model.as_str()
        }
    );
    println!(
        "provider key: {}",
        if config.has_api_key() {
            "configured"
        } else {
            "not configured"
        }
    );
    println!(
        "features: memories={} auto_context_compact={} terminal_resize_reflow={} image_generation={}",
        config.features.memories,
        config.features.auto_context_compact,
        config.features.terminal_resize_reflow,
        config.features.image_generation
    );
    println!("permissions: {}", config.permission_mode.title());
    print_context_status(paths, &config)?;
    Ok(())
}

fn resume_command(paths: &AppPaths, session_id: Option<String>) -> Result<(), String> {
    let config = AppConfig::load(paths)?;
    let store = SessionStore::new(paths)?;
    let selected = if matches!(session_id.as_deref(), None | Some("latest")) {
        store.resolve(&paths.project_key, None)?
    } else {
        store.resolve(&paths.project_key, session_id)?
    };
    crate::tui::run_session_view_tui(paths.clone(), config, selected.id)
}

fn share_command(
    paths: &AppPaths,
    session_id: Option<String>,
    path: Option<String>,
) -> Result<(), String> {
    let store = SessionStore::new(paths)?;
    let session = if matches!(session_id.as_deref(), None | Some("latest")) {
        store.resolve(&paths.project_key, None)?
    } else {
        store.resolve(&paths.project_key, session_id)?
    };
    let timeline = store.timeline(&paths.project_key, &session.id)?;
    let transcript = format_session_transcript(&session, &timeline);
    if let Some(path) = path {
        std::fs::write(&path, transcript).map_err(|e| e.to_string())?;
        println!("saved transcript: {path}");
    } else {
        println!("{transcript}");
    }
    Ok(())
}

fn format_session_transcript(
    session: &crate::session::SessionSummary,
    timeline: &[crate::session::TimelineEvent],
) -> String {
    let mut out = String::new();
    out.push_str("# Session ");
    out.push_str(&session.id);
    out.push('\n');
    if let Some(summary) = &session.summary {
        out.push_str("\nSummary: ");
        out.push_str(summary);
        out.push('\n');
    }
    out.push_str("\n## Timeline\n");
    for event in timeline {
        out.push_str("\n### ");
        out.push_str(&event.kind);
        if let Some(role) = &event.role {
            out.push_str(" / ");
            out.push_str(role);
        }
        out.push('\n');
        if let Some(content) = &event.content {
            if !content.trim().is_empty() {
                out.push_str(content.trim());
                out.push('\n');
            }
        }
        if let Some(command) = &event.command {
            out.push_str("command: ");
            out.push_str(command);
            out.push('\n');
        }
        if let Some(stdout) = &event.stdout {
            if !stdout.trim().is_empty() {
                out.push_str("stdout:\n");
                out.push_str(stdout);
                if !stdout.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
        if let Some(stderr) = &event.stderr {
            if !stderr.trim().is_empty() {
                out.push_str("stderr:\n");
                out.push_str(stderr);
                if !stderr.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
}

fn list_models(paths: &AppPaths) -> Result<(), String> {
    let mut config = AppConfig::load(paths)?;
    if config.requires_login() {
        return Err(
            "login required; run `wirecli login` or edit `~/.wirecli/config/config.toml`"
                .to_string(),
        );
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let parsed = runtime
        .block_on(model_catalog::load_models(&config))
        .map_err(|err| {
            if err.starts_with("login required") && config.api_key.is_some() {
                config.clear_saved_login();
                let _ = config.save(paths);
                "saved provider credential expired; run `wirecli login` or update the provider API key"
                    .to_string()
            } else {
                err
            }
        })?;
    runtime.shutdown_background();

    for model in parsed {
        println!("{}  [{}]", model.title(), model.id);
        let mut details = Vec::new();
        if let Some(owned_by) = model.owned_by.as_deref() {
            if !owned_by.is_empty() {
                details.push(format!("owner {owned_by}"));
            }
        }
        if let Some(context_window) = model.context_window {
            details.push(format!("ctx {}", context_window));
        }
        if let Some(max_completion_tokens) = model.max_completion_tokens {
            details.push(format!("max completion {}", max_completion_tokens));
        }
        if !model.capabilities.is_empty() {
            details.push(model.capabilities.join(" "));
        }
        if !details.is_empty() {
            println!("  {}", details.join("  ·  "));
        }
    }
    Ok(())
}

fn print_context_status(paths: &AppPaths, config: &AppConfig) -> Result<(), String> {
    if config.requires_login() {
        println!("context window: unknown");
        println!("context remaining: unknown");
        return Ok(());
    }

    let models = model_catalog::load_models_blocking(config, std::time::Duration::from_secs(4))
        .unwrap_or_default();
    let model = model_catalog::current_model_info(&models, &config.model);
    let Some(context_window) = model.as_ref().and_then(|model| model.context_window) else {
        println!("context window: unknown");
        println!("context remaining: unknown");
        return Ok(());
    };
    let max_completion_tokens = model
        .as_ref()
        .and_then(|model| model.max_completion_tokens)
        .unwrap_or_else(|| (context_window / 8).clamp(2_048, 8_192));
    let estimated_history_tokens = latest_session_estimated_tokens(paths).unwrap_or(0);
    let used_with_reserve = estimated_history_tokens.saturating_add(max_completion_tokens);
    let remaining = context_window.saturating_sub(used_with_reserve);
    println!("context window: {}", compact_number(context_window));
    println!(
        "context max completion: {}",
        compact_number(max_completion_tokens)
    );
    println!(
        "context estimated current session: {}",
        compact_number(estimated_history_tokens)
    );
    println!("context remaining: {}", compact_number(remaining));
    Ok(())
}

fn latest_session_estimated_tokens(paths: &AppPaths) -> Result<u64, String> {
    let store = SessionStore::new(paths)?;
    let session = match store.resolve(&paths.project_key, None) {
        Ok(session) => session,
        Err(_) => return Ok(0),
    };
    let timeline = store.timeline(&paths.project_key, &session.id)?;
    let mut total = 0u64;
    for event in timeline {
        if let Some(content) = event.content.as_deref() {
            total = total.saturating_add(estimated_tokens(content));
        }
        if let Some(command) = event.command.as_deref() {
            total = total.saturating_add(estimated_tokens(command));
        }
        if let Some(stdout) = event.stdout.as_deref() {
            total = total.saturating_add(estimated_tokens(stdout));
        }
        if let Some(stderr) = event.stderr.as_deref() {
            total = total.saturating_add(estimated_tokens(stderr));
        }
    }
    Ok(total)
}

fn list_mcp(paths: &AppPaths) -> Result<(), String> {
    let registry = McpRegistry::load(paths)?;
    print_mcp_panel(&registry)?;
    Ok(())
}

fn configure_mcp(paths: &AppPaths) -> Result<(), String> {
    let mut line = String::new();
    println!("MCP setup");
    print!("name: ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let name = line.trim().to_string();
    if name.is_empty() {
        return Err("missing MCP server name".to_string());
    }

    line.clear();
    print!("transport (stdio/http) [stdio]: ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let transport = match line.trim() {
        "http" | "https" => "http".to_string(),
        _ => "stdio".to_string(),
    };

    line.clear();
    let mut command = String::new();
    let mut args = Vec::new();
    let mut url = None;
    let mut http_headers = BTreeMap::new();
    let mut startup_ts = Some(120u64);

    if transport == "http" {
        print!("url: ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let value = line.trim().to_string();
        if value.is_empty() {
            return Err("missing MCP url".to_string());
        }
        url = Some(value);
        line.clear();
        print!("http header key (optional, blank to skip): ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let header_key = line.trim().to_string();
        if !header_key.is_empty() {
            line.clear();
            print!("http header value: ");
            io::stdout().flush().map_err(|e| e.to_string())?;
            io::stdin()
                .read_line(&mut line)
                .map_err(|e| e.to_string())?;
            let header_value = line.trim().to_string();
            if header_value.is_empty() {
                return Err("missing MCP header value".to_string());
            }
            http_headers.insert(header_key, header_value);
        }
    } else {
        print!("command: ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        command = line.trim().to_string();
        if command.is_empty() {
            return Err("missing MCP command".to_string());
        }

        line.clear();
        print!("args (quoted shell-style, optional): ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        args = split_command_line(line.trim())?;

        line.clear();
        print!("cwd (optional): ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        let cwd = match line.trim() {
            "" => None,
            value => Some(PathBuf::from(value)),
        };

        line.clear();
        print!("startup timeout seconds [120]: ");
        io::stdout().flush().map_err(|e| e.to_string())?;
        io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        startup_ts = match line.trim() {
            "" => Some(120),
            value => value.parse::<u64>().ok().filter(|value| *value > 0),
        };

        let server = McpServerConfig {
            name: name.clone(),
            transport,
            command,
            args,
            cwd,
            env: BTreeMap::new(),
            url,
            http_headers,
            startup_ts,
        };
        McpRegistry::add_server(paths, server)?;
        println!("saved MCP server: {name}");
        return Ok(());
    }

    let server = McpServerConfig {
        name: name.clone(),
        transport,
        command,
        args,
        cwd: None,
        env: BTreeMap::new(),
        url,
        http_headers,
        startup_ts,
    };
    McpRegistry::add_server(paths, server)?;
    println!("saved MCP server: {name}");
    Ok(())
}

fn print_mcp_panel(registry: &McpRegistry) -> Result<(), String> {
    println!("MCP");
    if registry.servers().is_empty() {
        println!("Nothing Yet!");
        return Ok(());
    }

    println!("configured servers:");
    for server in registry.servers() {
        if server.transport == "http" {
            println!(
                "- {}  http {}",
                server.name,
                server.url.clone().unwrap_or_default()
            );
        } else {
            println!("- {}  {}", server.name, server.command);
        }
    }

    let report = registry.discover_tools_report();
    if report.tools.is_empty() {
        println!("No MCP tools discovered");
    } else {
        println!();
        println!("tools:");
        for tool in report.tools {
            println!(
                "- {}  ({}::{})",
                tool.function_name, tool.server_name, tool.tool_name
            );
        }
    }
    if !report.errors.is_empty() {
        println!();
        println!("discovery warnings:");
        for error in report.errors {
            println!("- {error}");
        }
    }
    Ok(())
}

fn run_prompt(paths: &AppPaths, prompt: String) -> Result<(), String> {
    if prompt.trim().is_empty() {
        return Err("missing prompt".to_string());
    }

    let config = AppConfig::load(paths)?;
    if config.requires_login() {
        return Err(
            "login required; run `wirecli login` or edit `~/.wirecli/config/config.toml`"
                .to_string(),
        );
    }
    if config.requires_model_selection() {
        return Err(
            "model not selected; run `wirecli` and choose a model with /models".to_string(),
        );
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let (session_id, _output) =
        runtime.block_on(responses_agent::run_prompt(paths, &config, prompt.clone()))?;
    runtime.shutdown_background();
    println!("\nsession: {}", session_id);
    Ok(())
}

fn lattice_command(paths: &AppPaths, action: LatticeAction) -> Result<(), String> {
    let manager = SandboxManager::new(paths)?;
    let anchors = AnchorStore::new(paths)?;
    let hooks = HookStore::new(paths)?;
    match action {
        LatticeAction::New { name } => {
            let summary = manager.create(&name)?;
            println!("box created");
            println!("id: {}", summary.id);
            println!("name: {}", summary.name);
            println!(
                "workspace: {}/{}",
                paths.sandboxes_dir.display(),
                summary.id
            );
        }
        LatticeAction::List => {
            let cells = manager.list()?;
            if cells.is_empty() {
                println!("no boxes");
            } else {
                for cell in cells {
                    println!(
                        "{}  {}  {}  {}",
                        cell.id, cell.state, cell.created_at, cell.name
                    );
                }
            }
        }
        LatticeAction::Run { cell_id, command } => {
            if cell_id.trim().is_empty() {
                return Err("missing cell id".to_string());
            }
            let status = manager.run(&cell_id, &command)?;
            if !status.success() {
                return Err(match status.code() {
                    Some(code) => format!("cell command exited with code {code}"),
                    None => "cell command terminated by signal".to_string(),
                });
            }
        }
        LatticeAction::Tools => {
            let config = AppConfig::load(paths)?;
            let tools = BoxTools::new(
                &manager,
                &anchors,
                &hooks,
                &config,
                config.permission_mode,
                CommandPolicy::standard(),
            );
            for tool in tools.list() {
                println!("{tool}");
            }
        }
    }
    Ok(())
}

fn print_help() {
    println!("wirecli 0.1.1");
    println!();
    println!("usage:");
    println!("  wirecli");
    println!("  wirecli tui");
    println!("  wirecli sessions");
    println!("  wirecli resume [latest|session-id]");
    println!("  wirecli share [latest|session-id] [file.md]");
    println!("  wirecli run <prompt...>");
    println!("  wirecli login");
    println!("  wirecli status");
    println!("  wirecli models");
    println!("  wirecli providers [provider-id]");
    println!("  wirecli approvals [list|allow-once|allow-repo|deny|deny-always]");
    println!("  wirecli hooks [list|add|remove]");
    println!("  wirecli skills [list|read|create]");
    println!("  wirecli harness [run|replay|inspect|doctor|evals]");
    println!("  wirecli mcp");
    println!("  wirecli mcpc");
    println!("  wirecli box [new|list|run|tools]");
    println!("  wirecli --help");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_runs_prompt_command() {
        let cli = parse_args(vec![
            "run".to_string(),
            "hello".to_string(),
            "world".to_string(),
        ]);
        match cli.command {
            Command::Run { prompt } => assert_eq!(prompt, "hello world"),
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn parse_status_command() {
        let cli = parse_args(vec!["status".to_string()]);
        match cli.command {
            Command::Status => {}
            _ => panic!("expected status command"),
        }
    }

    #[test]
    fn parse_box_alias() {
        let cli = parse_args(vec!["box".to_string(), "list".to_string()]);
        match cli.command {
            Command::Lattice { action } => match action {
                LatticeAction::List => {}
                _ => panic!("expected list action"),
            },
            _ => panic!("expected lattice command"),
        }
    }

    #[test]
    fn parse_box_tools() {
        let cli = parse_args(vec!["box".to_string(), "tools".to_string()]);
        match cli.command {
            Command::Lattice { action } => match action {
                LatticeAction::Tools => {}
                _ => panic!("expected tools action"),
            },
            _ => panic!("expected lattice command"),
        }
    }

    #[test]
    fn parse_binary_name_aliases_to_tui() {
        let cli = parse_args(vec!["wirecli".to_string()]);
        match cli.command {
            Command::Tui => {}
            _ => panic!("expected tui command"),
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn parse_hidden_dev_path_command_after_binary_alias() {
        let cli = parse_args(vec![
            "wirecli".to_string(),
            "path".to_string(),
            "~/workspace".to_string(),
        ]);
        match cli.command {
            Command::DevPath { path } => assert_eq!(path, "~/workspace"),
            _ => panic!("expected dev path command"),
        }
    }

    #[test]
    fn parse_mcp_command() {
        let cli = parse_args(vec!["mcp".to_string()]);
        match cli.command {
            Command::Mcp => {}
            _ => panic!("expected mcp command"),
        }
    }

    #[test]
    fn parse_mcpc_command() {
        let cli = parse_args(vec!["mcpc".to_string()]);
        match cli.command {
            Command::Mcpc => {}
            _ => panic!("expected mcpc command"),
        }
    }

    #[test]
    fn parse_approvals_allow_once_command() {
        let cli = parse_args(vec![
            "approvals".to_string(),
            "allow-once".to_string(),
            "req_1".to_string(),
        ]);
        match cli.command {
            Command::Approvals { action } => match action {
                ApprovalsAction::AllowOnce { id } => assert_eq!(id, "req_1"),
                _ => panic!("expected allow once"),
            },
            _ => panic!("expected approvals command"),
        }
    }

    #[test]
    fn parse_scoped_hook_add_command() {
        let cli = parse_args(vec![
            "hooks".to_string(),
            "add".to_string(),
            "post_tool_use".to_string(),
            "--match-tool".to_string(),
            "shell".to_string(),
            "--match-status".to_string(),
            "blocked".to_string(),
            "--match-path".to_string(),
            "src/".to_string(),
            "--match-mode".to_string(),
            "starts_with".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "check".to_string(),
        ]);
        match cli.command {
            Command::Hooks { action } => match action {
                HooksAction::Add {
                    event,
                    command,
                    match_mode,
                    match_tool,
                    match_status,
                    match_path,
                    ..
                } => {
                    assert_eq!(event, "post_tool_use");
                    assert_eq!(command, ["cargo", "check"]);
                    assert_eq!(match_mode.as_str(), "starts_with");
                    assert_eq!(match_tool.as_deref(), Some("shell"));
                    assert_eq!(match_status.as_deref(), Some("blocked"));
                    assert_eq!(match_path.as_deref(), Some("src/"));
                }
                _ => panic!("expected hook add"),
            },
            _ => panic!("expected hooks command"),
        }
    }

    #[test]
    fn parse_hook_add_preserves_command_separator_after_command_started() {
        let cli = parse_args(vec![
            "hooks".to_string(),
            "add".to_string(),
            "after_edit".to_string(),
            "cargo".to_string(),
            "test".to_string(),
            "--".to_string(),
            "--ignored".to_string(),
        ]);
        match cli.command {
            Command::Hooks { action } => match action {
                HooksAction::Add { command, .. } => {
                    assert_eq!(command, ["cargo", "test", "--", "--ignored"]);
                }
                _ => panic!("expected hook add"),
            },
            _ => panic!("expected hooks command"),
        }
    }

    #[test]
    fn parse_harness_command() {
        let cli = parse_args(vec![
            "harness".to_string(),
            "run".to_string(),
            "--prompt".to_string(),
            "hello".to_string(),
        ]);
        match cli.command {
            Command::Harness { .. } => {}
            _ => panic!("expected harness command"),
        }
    }

    #[test]
    fn parse_sessions_command() {
        let cli = parse_args(vec!["sessions".to_string()]);
        match cli.command {
            Command::Sessions => {}
            _ => panic!("expected sessions command"),
        }
    }
}
