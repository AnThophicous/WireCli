use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root_dir: PathBuf,
    pub project_key: String,
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub history_db: PathBuf,
    pub sandboxes_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub provider: String,
    pub model: String,
    pub workspace: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            provider: "echo".to_string(),
            model: "default".to_string(),
            workspace: None,
        }
    }
}

impl AppPaths {
    pub fn detect() -> Result<Self, String> {
        let home = env::var_os("HOME").ok_or_else(|| "HOME is not set".to_string())?;
        let home = PathBuf::from(home);

        let config_root = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let data_root = env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/share"));

        let root_dir = env::current_dir().map_err(|e| e.to_string())?;
        let project_key = project_key_for(&root_dir);
        let config_dir = resolve_writable_dir(
            &[
                config_root.join("rift-code"),
                root_dir.join(".riftcode/config"),
                PathBuf::from("/tmp/rift-code/config"),
            ],
            "config",
        )?;
        let config_file = config_dir.join("config");
        let data_dir = resolve_writable_dir(
            &[
                data_root.join("rift-code"),
                root_dir.join(".riftcode/data"),
                PathBuf::from("/tmp/rift-code/data"),
            ],
            "data",
        )?;
        let history_db = data_dir.join("history.sqlite3");
        let sandboxes_dir = data_dir.join("lattice");

        Ok(Self {
            root_dir,
            project_key,
            config_dir,
            config_file,
            data_dir,
            history_db,
            sandboxes_dir,
        })
    }
}

fn project_key_for(path: &PathBuf) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.clone())
        .to_string_lossy()
        .to_string()
}

fn resolve_writable_dir(candidates: &[PathBuf], label: &str) -> Result<PathBuf, String> {
    let mut last_error = None;
    for candidate in candidates {
        match fs::create_dir_all(candidate) {
            Ok(()) => return Ok(candidate.clone()),
            Err(err) => last_error = Some(err.to_string()),
        }
    }

    Err(match last_error {
        Some(err) => format!("unable to create writable {label} directory: {err}"),
        None => format!("unable to create writable {label} directory"),
    })
}

impl AppConfig {
    pub fn load(paths: &AppPaths) -> Result<Self, String> {
        if !paths.config_file.exists() {
            return Ok(Self::default());
        }

        let raw = fs::read_to_string(&paths.config_file).map_err(|e| e.to_string())?;
        let mut entries = HashMap::new();

        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let mut parts = trimmed.splitn(2, '=');
            let key = parts.next().unwrap_or("").trim();
            let value = parts.next().unwrap_or("").trim();
            if !key.is_empty() {
                entries.insert(key.to_string(), value.to_string());
            }
        }

        Ok(Self {
            provider: entries
                .remove("provider")
                .unwrap_or_else(|| "echo".to_string()),
            model: entries
                .remove("model")
                .unwrap_or_else(|| "default".to_string()),
            workspace: entries.remove("workspace").and_then(|value| {
                if value.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            }),
        })
    }

    pub fn to_file_contents(&self) -> String {
        let mut output = String::new();
        output.push_str("provider=");
        output.push_str(&self.provider);
        output.push('\n');
        output.push_str("model=");
        output.push_str(&self.model);
        output.push('\n');
        if let Some(workspace) = &self.workspace {
            output.push_str("workspace=");
            output.push_str(&workspace.display().to_string());
            output.push('\n');
        }
        output
    }
}
