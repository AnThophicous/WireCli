use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root_dir: PathBuf,
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub sessions_dir: PathBuf,
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
        let config_dir = config_root.join("rift-code");
        let config_file = config_dir.join("config");
        let data_dir = data_root.join("rift-code");
        let sessions_dir = data_dir.join("sessions");

        Ok(Self {
            root_dir,
            config_dir,
            config_file,
            data_dir,
            sessions_dir,
        })
    }
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
