use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::PathBuf;
use ratatui::style::Color;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root_dir: PathBuf,
    pub project_key: String,
    pub rift_dir: PathBuf,
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub theme_file: PathBuf,
    pub mcp_file: PathBuf,
    pub data_dir: PathBuf,
    pub history_db: PathBuf,
    pub anchor_db: PathBuf,
    pub sandboxes_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub workspace: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            provider: "local".to_string(),
            base_url: "http://127.0.0.1:3000/v1".to_string(),
            model: "qwen3.6-plus".to_string(),
            workspace: None,
        }
    }
}

impl AppPaths {
    pub fn detect() -> Result<Self, String> {
        let root_dir = env::current_dir().map_err(|e| e.to_string())?;
        let project_key = project_key_for(&root_dir);
        let rift_dir = home_rift_dir().unwrap_or_else(|| root_dir.join(".rift"));
        fs::create_dir_all(&rift_dir).map_err(|e| e.to_string())?;
        let config_dir = rift_dir.join("config");
        fs::create_dir_all(&config_dir).map_err(|e| e.to_string())?;
        let config_file = config_dir.join("config.yaml");
        let theme_file = rift_dir.join("theme.yaml");
        let mcp_file = rift_dir.join("mcp_servers.json");
        if !mcp_file.exists() {
            fs::write(&mcp_file, "{\n  \"servers\": []\n}\n").map_err(|e| e.to_string())?;
        }
        let data_dir = rift_dir.join("data");
        fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
        let history_db = data_dir.join("history.sqlite3");
        let anchor_db = data_dir.join("anchor.sqlite3");
        let sandboxes_dir = rift_dir.join("boxes");
        fs::create_dir_all(&sandboxes_dir).map_err(|e| e.to_string())?;

        Ok(Self {
            root_dir,
            project_key,
            rift_dir,
            config_dir,
            config_file,
            theme_file,
            mcp_file,
            data_dir,
            history_db,
            anchor_db,
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

fn home_rift_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".rift"))
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
                .unwrap_or_else(|| "local".to_string()),
            base_url: entries
                .remove("base_url")
                .unwrap_or_else(|| "http://127.0.0.1:3000/v1".to_string()),
            model: entries
                .remove("model")
                .unwrap_or_else(|| "qwen3.6-plus".to_string()),
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
        output.push_str("base_url=");
        output.push_str(&self.base_url);
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

#[derive(Debug, Clone)]
pub struct ThemeConfig {
    pub background: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub user_text: Color,
    pub assistant_text: Color,
    pub tool_text: Color,
    pub emphasis: Color,
    pub success: Color,
    pub danger: Color,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            background: color_from_hex("#08111f"),
            border: color_from_hex("#2f80ff"),
            text: color_from_hex("#ffffff"),
            muted: color_from_hex("#7f8ea8"),
            accent: color_from_hex("#2f80ff"),
            user_text: color_from_hex("#ffffff"),
            assistant_text: color_from_hex("#ffffff"),
            tool_text: color_from_hex("#d5e3ff"),
            emphasis: color_from_hex("#59a6ff"),
            success: color_from_hex("#4fdb9a"),
            danger: color_from_hex("#ff6a7a"),
        }
    }
}

impl ThemeConfig {
    pub fn load_or_create(path: &PathBuf) -> Result<Self, String> {
        if !path.exists() {
            let theme = Self::default();
            fs::write(path, theme.to_yaml()).map_err(|e| e.to_string())?;
            return Ok(theme);
        }

        let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut entries = HashMap::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let (key, value) = if let Some((key, value)) = trimmed.split_once(':') {
                (key.trim(), value.trim())
            } else if let Some((key, value)) = trimmed.split_once('=') {
                (key.trim(), value.trim())
            } else {
                continue;
            };
            let value = value.trim_matches('"').trim_matches('\'');
            if !key.is_empty() && !value.is_empty() {
                entries.insert(key.to_string(), value.to_string());
            }
        }

        let mut theme = Self::default();
        if let Some(value) = entries.get("background") {
            theme.background = parse_theme_color(value).unwrap_or(theme.background);
        }
        if let Some(value) = entries.get("border") {
            theme.border = parse_theme_color(value).unwrap_or(theme.border);
        }
        if let Some(value) = entries.get("text") {
            theme.text = parse_theme_color(value).unwrap_or(theme.text);
        }
        if let Some(value) = entries.get("muted") {
            theme.muted = parse_theme_color(value).unwrap_or(theme.muted);
        }
        if let Some(value) = entries.get("accent") {
            theme.accent = parse_theme_color(value).unwrap_or(theme.accent);
        }
        if let Some(value) = entries.get("user_text") {
            theme.user_text = parse_theme_color(value).unwrap_or(theme.user_text);
        }
        if let Some(value) = entries.get("assistant_text") {
            theme.assistant_text = parse_theme_color(value).unwrap_or(theme.assistant_text);
        }
        if let Some(value) = entries.get("tool_text") {
            theme.tool_text = parse_theme_color(value).unwrap_or(theme.tool_text);
        }
        if let Some(value) = entries.get("emphasis") {
            theme.emphasis = parse_theme_color(value).unwrap_or(theme.emphasis);
        }
        if let Some(value) = entries.get("success") {
            theme.success = parse_theme_color(value).unwrap_or(theme.success);
        }
        if let Some(value) = entries.get("danger") {
            theme.danger = parse_theme_color(value).unwrap_or(theme.danger);
        }

        Ok(theme)
    }

    pub fn to_yaml(&self) -> String {
        let mut out = String::new();
        out.push_str("background: ");
        out.push_str(&color_to_hex(self.background));
        out.push('\n');
        out.push_str("border: ");
        out.push_str(&color_to_hex(self.border));
        out.push('\n');
        out.push_str("text: ");
        out.push_str(&color_to_hex(self.text));
        out.push('\n');
        out.push_str("muted: ");
        out.push_str(&color_to_hex(self.muted));
        out.push('\n');
        out.push_str("accent: ");
        out.push_str(&color_to_hex(self.accent));
        out.push('\n');
        out.push_str("user_text: ");
        out.push_str(&color_to_hex(self.user_text));
        out.push('\n');
        out.push_str("assistant_text: ");
        out.push_str(&color_to_hex(self.assistant_text));
        out.push('\n');
        out.push_str("tool_text: ");
        out.push_str(&color_to_hex(self.tool_text));
        out.push('\n');
        out.push_str("emphasis: ");
        out.push_str(&color_to_hex(self.emphasis));
        out.push('\n');
        out.push_str("success: ");
        out.push_str(&color_to_hex(self.success));
        out.push('\n');
        out.push_str("danger: ");
        out.push_str(&color_to_hex(self.danger));
        out
    }
}

fn parse_theme_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        return parse_hex_color(hex);
    }
    match value.to_ascii_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "dark_gray" => Some(Color::DarkGray),
        "white" => Some(Color::White),
        _ => None,
    }
}

fn parse_hex_color(hex: &str) -> Option<Color> {
    let hex = hex.trim();
    if hex.len() != 6 {
        return None;
    }
    let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(red, green, blue))
}

fn color_from_hex(hex: &str) -> Color {
    parse_hex_color(hex.trim_start_matches('#')).unwrap_or(Color::White)
}

fn color_to_hex(color: Color) -> String {
    match color {
        Color::Rgb(r, g, b) => format!("#{:02x}{:02x}{:02x}", r, g, b),
        Color::Black => "#000000".to_string(),
        Color::Red => "#ff5c77".to_string(),
        Color::Green => "#4fdb9a".to_string(),
        Color::Yellow => "#d7d74a".to_string(),
        Color::Blue => "#2f80ff".to_string(),
        Color::Magenta => "#8b6cff".to_string(),
        Color::Cyan => "#67d8ff".to_string(),
        Color::Gray => "#8a94a6".to_string(),
        Color::DarkGray => "#4f5b72".to_string(),
        Color::White => "#ffffff".to_string(),
        _ => "#ffffff".to_string(),
    }
}
