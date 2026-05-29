use crate::backend::select_backend;
use crate::config::{AppConfig, AppPaths};
use crate::session::{SessionEvent, SessionStore};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub enum Command {
    Init,
    Status,
    Doctor,
    Sessions,
    Run { prompt: String },
    Resume { session_id: Option<String> },
    Config { action: ConfigAction },
    Help,
    Version,
}

#[derive(Debug, Clone)]
pub enum ConfigAction {
    Show,
    Get { key: String },
    Set { key: String, value: String },
}

#[derive(Debug, Clone)]
pub struct Cli {
    pub command: Command,
}

pub fn parse_args(args: Vec<String>) -> Cli {
    let command = match args.first().map(|s| s.as_str()) {
        None => Command::Help,
        Some("-h") | Some("--help") => Command::Help,
        Some("-V") | Some("--version") => Command::Version,
        Some("init") => Command::Init,
        Some("status") => Command::Status,
        Some("doctor") => Command::Doctor,
        Some("sessions") | Some("list") => Command::Sessions,
        Some("run") => {
            let prompt = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
            Command::Run { prompt }
        }
        Some("resume") => {
            let session_id = args.get(1).cloned();
            Command::Resume { session_id }
        }
        Some("config") => parse_config_command(&args[1..]),
        _ => Command::Help,
    };

    Cli { command }
}

fn parse_config_command(args: &[String]) -> Command {
    match args.first().map(|s| s.as_str()) {
        Some("show") | None => Command::Config {
            action: ConfigAction::Show,
        },
        Some("get") => {
            let key = args.get(1).cloned().unwrap_or_default();
            Command::Config {
                action: ConfigAction::Get { key },
            }
        }
        Some("set") => {
            let key = args.get(1).cloned().unwrap_or_default();
            let value = args.iter().skip(2).cloned().collect::<Vec<_>>().join(" ");
            Command::Config {
                action: ConfigAction::Set { key, value },
            }
        }
        _ => Command::Help,
    }
}

pub fn run(cli: Cli) -> Result<(), String> {
    let paths = AppPaths::detect()?;

    match cli.command {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Version => {
            println!("riftcode-cli 0.1.0");
            Ok(())
        }
        Command::Init => init(&paths),
        Command::Status => status(&paths),
        Command::Doctor => doctor(&paths),
        Command::Sessions => list_sessions(&paths),
        Command::Run { prompt } => run_prompt(&paths, prompt),
        Command::Resume { session_id } => resume_session(&paths, session_id),
        Command::Config { action } => config_command(&paths, action),
    }
}

fn init(paths: &AppPaths) -> Result<(), String> {
    fs::create_dir_all(&paths.config_dir).map_err(|e| e.to_string())?;
    fs::create_dir_all(&paths.data_dir).map_err(|e| e.to_string())?;
    if !paths.config_file.exists() {
        fs::write(&paths.config_file, AppConfig::default().to_file_contents())
            .map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&paths.sessions_dir).map_err(|e| e.to_string())?;
    println!("initialized {}", paths.root_dir.display());
    Ok(())
}

fn status(paths: &AppPaths) -> Result<(), String> {
    let config = AppConfig::load(paths)?;
    println!("root: {}", paths.root_dir.display());
    println!("config: {}", paths.config_file.display());
    println!("data: {}", paths.data_dir.display());
    println!("sessions: {}", paths.sessions_dir.display());
    println!("provider: {}", config.provider);
    println!("model: {}", config.model);
    if let Some(workspace) = config.workspace {
        println!("workspace: {}", workspace.display());
    }
    Ok(())
}

fn doctor(paths: &AppPaths) -> Result<(), String> {
    let config = AppConfig::load(paths)?;
    let backend = select_backend(&config);
    println!("binary: riftcode-cli");
    println!("config dir: {}", paths.config_dir.display());
    println!("data dir: {}", paths.data_dir.display());
    println!("backend: {}", backend.name());
    println!("configured provider: {}", config.provider);
    println!("configured model: {}", config.model);
    println!(
        "config file: {}",
        if paths.config_file.exists() {
            "present"
        } else {
            "missing"
        }
    );
    Ok(())
}

fn list_sessions(paths: &AppPaths) -> Result<(), String> {
    let store = SessionStore::new(paths)?;
    let sessions = store.list()?;
    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    for session in sessions {
        println!(
            "{}  {}  {}",
            session.id,
            session.updated_at,
            session.summary.unwrap_or_else(|| "session".to_string())
        );
    }

    Ok(())
}

fn run_prompt(paths: &AppPaths, prompt: String) -> Result<(), String> {
    if prompt.trim().is_empty() {
        return Err("missing prompt".to_string());
    }

    let config = AppConfig::load(paths)?;
    let backend = select_backend(&config);
    let mut store = SessionStore::new(paths)?;
    let session = store.create(&prompt)?;
    store.append_event(&session.id, SessionEvent::user(prompt.clone()))?;

    let output = backend.respond(&prompt);
    store.append_event(&session.id, SessionEvent::assistant(output.text.clone()))?;

    println!("{}", output.text);
    println!("session: {}", session.id);
    Ok(())
}

fn resume_session(paths: &AppPaths, session_id: Option<String>) -> Result<(), String> {
    let store = SessionStore::new(paths)?;
    let session = store.resolve(session_id)?;
    let events = store.read_events(&session.id)?;

    println!("session: {}", session.id);
    println!("created: {}", session.created_at);
    for event in events {
        println!("[{}] {}", event.role, event.content);
    }
    Ok(())
}

fn config_command(paths: &AppPaths, action: ConfigAction) -> Result<(), String> {
    let mut config = AppConfig::load(paths)?;
    match action {
        ConfigAction::Show => {
            print!("{}", config.to_file_contents());
        }
        ConfigAction::Get { key } => match key.as_str() {
            "provider" => println!("{}", config.provider),
            "model" => println!("{}", config.model),
            "workspace" => {
                if let Some(workspace) = config.workspace {
                    println!("{}", workspace.display());
                }
            }
            _ => return Err(format!("unknown key: {key}")),
        },
        ConfigAction::Set { key, value } => {
            match key.as_str() {
                "provider" => config.provider = value,
                "model" => config.model = value,
                "workspace" => {
                    config.workspace = if value.trim().is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(value))
                    }
                }
                _ => return Err(format!("unknown key: {key}")),
            }
            fs::create_dir_all(&paths.config_dir).map_err(|e| e.to_string())?;
            fs::write(&paths.config_file, config.to_file_contents()).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn print_help() {
    println!("riftcode-cli 0.1.0");
    println!();
    println!("usage:");
    println!("  riftcode-cli init");
    println!("  riftcode-cli status");
    println!("  riftcode-cli doctor");
    println!("  riftcode-cli sessions");
    println!("  riftcode-cli run <prompt...>");
    println!("  riftcode-cli resume [session-id]");
    println!("  riftcode-cli config [show|get|set]");
    println!("  riftcode-cli --help");
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
}
