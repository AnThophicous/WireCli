use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::providers::{provider_uses_openrouter_pkce, ProviderProtocol};
use crate::safekey::{is_protected_secret, protect_secret, reveal_secret, write_private_file};

const WIRE_HOME_ENV: &str = "WIRE_HOME";
const WIRE_STATE_DIR: &str = ".wirecli";
const CONFIG_FILE_NAME: &str = "config.toml";
const CONFIG_DOC_FILE_NAME: &str = "config.md";

const SENSITIVE_CONFIG_KEYS: &[&str] = &[
    "api_key",
    "wire_session_token",
    "account_id",
    "account_name",
    "account_email",
];

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub root_dir: PathBuf,
    pub project_key: String,
    pub wire_dir: PathBuf,
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub secret_key_file: PathBuf,
    pub theme_file: PathBuf,
    pub mcp_file: PathBuf,
    pub hooks_file: PathBuf,
    pub data_dir: PathBuf,
    pub history_db: PathBuf,
    pub anchor_db: PathBuf,
    pub memory_context_file: PathBuf,
    pub sandboxes_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub approvals_reviewer: String,
    pub model_reasoning_effort: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub wire_session_token: Option<String>,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub account_email: Option<String>,
    pub workspace: Option<PathBuf>,
    pub permission_mode: PermissionMode,
    pub protocol: ProviderProtocol,
    pub features: AppFeatures,
    pub feature_context: FeatureContextConfig,
    pub model_providers: Vec<CustomModelProvider>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomModelProvider {
    pub id: String,
    pub name: Option<String>,
    pub base_url: String,
    pub models: Vec<String>,
    pub api_key_env: Option<String>,
    pub protocol: ProviderProtocol,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GlobalModelStatus {
    pub provider: String,
    pub model: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    pub protocol: ProviderProtocol,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppFeatures {
    pub memories: bool,
    pub auto_context_compact: bool,
    pub terminal_resize_reflow: bool,
    pub image_generation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureContextConfig {
    pub enabled: bool,
    pub afup: bool,
    pub flash_cache_memory: bool,
    pub automatic_context_compaction: bool,
    pub acc_model: String,
    pub fcm_max_entries: usize,
}

impl Default for FeatureContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            afup: true,
            flash_cache_memory: true,
            automatic_context_compaction: true,
            acc_model: "openrouter/free".to_string(),
            fcm_max_entries: 192,
        }
    }
}

impl Default for AppFeatures {
    fn default() -> Self {
        Self {
            memories: true,
            auto_context_compact: true,
            terminal_resize_reflow: true,
            image_generation: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Normal,
    Guardian,
    FullAccess,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Guardian => "guardian",
            Self::FullAccess => "full_access",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Normal => "Normal",
            Self::Guardian => "Guardian",
            Self::FullAccess => "Full Access",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Normal => {
                "Agent can edit files and run allowed commands only inside the project Box; risky commands require approval."
            }
            Self::Guardian => {
                "Agent stays inside the Box and commands pass local approval plus configured provider review."
            }
            Self::FullAccess => {
                "Not recommended. Agent can read, write, run commands, and access the network without Guardian review."
            }
        }
    }

    pub fn from_config(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "guardian" | "medium" => Self::Guardian,
            "full" | "full_permission" | "full-permission" | "full_access" | "full-access" => {
                Self::FullAccess
            }
            _ => Self::Normal,
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            provider: "openrouter".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            model: String::new(),
            approvals_reviewer: "user".to_string(),
            model_reasoning_effort: None,
            api_key_env: None,
            api_key: None,
            wire_session_token: None,
            account_id: None,
            account_name: None,
            account_email: None,
            workspace: None,
            permission_mode: PermissionMode::Normal,
            protocol: ProviderProtocol::ChatCompletions,
            features: AppFeatures::default(),
            feature_context: FeatureContextConfig::default(),
            model_providers: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn memories_enabled(&self) -> bool {
        self.features.memories && self.feature_context.enabled
    }

    pub fn afup_enabled(&self) -> bool {
        self.memories_enabled() && self.feature_context.afup
    }

    pub fn flash_cache_memory_enabled(&self) -> bool {
        self.memories_enabled() && self.feature_context.flash_cache_memory
    }

    pub fn auto_context_compaction_enabled(&self) -> bool {
        self.features.auto_context_compact
            && self.feature_context.enabled
            && self.feature_context.automatic_context_compaction
    }

    pub fn acc_model(&self) -> &str {
        let model = self.feature_context.acc_model.trim();
        if model.is_empty() {
            "openrouter/free"
        } else {
            model
        }
    }
}

impl AppPaths {
    pub fn detect() -> Result<Self, String> {
        let root_dir = env::current_dir().map_err(|e| e.to_string())?;
        let project_key = project_key_for(&root_dir);
        let wire_dir = wire_home_dir()?.join(WIRE_STATE_DIR);
        migrate_project_local_state(&root_dir, &wire_dir)?;
        ensure_project_gitignore_for_wire_state(&root_dir)?;
        ensure_private_dir(&wire_dir)?;
        let config_dir = wire_dir.join("config");
        ensure_private_dir(&config_dir)?;
        let config_file = config_dir.join(CONFIG_FILE_NAME);
        let secret_key_file = config_dir.join("secret.key");
        ensure_config_docs(&config_dir)?;
        ensure_default_config_file(&config_dir, &config_file)?;
        let theme_file = wire_dir.join("theme.yaml");
        let mcp_file = config_dir.join("mcp_servers.json");
        let hooks_file = wire_dir.join("hooks.json");
        if !hooks_file.exists() {
            write_private_file(&hooks_file, b"{\n  \"v\": 1,\n  \"hooks\": []\n}\n")?;
        }
        let data_dir = wire_dir.join("data");
        ensure_private_dir(&data_dir)?;
        let history_db = data_dir.join("history.sqlite3");
        let anchor_db = data_dir.join("anchor.sqlite3");
        let memory_context_file = data_dir.join("memory_context.json");
        if !memory_context_file.exists() {
            write_private_file(&memory_context_file, b"{\"v\":1,\"p\":{}}\n")?;
        }
        let sandboxes_dir = wire_dir.join("boxes");
        ensure_private_dir(&sandboxes_dir)?;

        Ok(Self {
            root_dir,
            project_key,
            wire_dir,
            config_dir,
            config_file,
            secret_key_file,
            theme_file,
            mcp_file,
            hooks_file,
            data_dir,
            history_db,
            anchor_db,
            memory_context_file,
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

fn wire_home_dir() -> Result<PathBuf, String> {
    if let Ok(value) = env::var(WIRE_HOME_ENV) {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    if let Ok(value) = env::var("HOME") {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    if let Ok(value) = env::var("USERPROFILE") {
        let value = value.trim();
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    Err("could not detect the user home directory for Wire CLI state".to_string())
}

fn migrate_project_local_state(root_dir: &Path, wire_dir: &Path) -> Result<(), String> {
    let local_wire = root_dir.join(WIRE_STATE_DIR);
    copy_state_dir_if_needed(&local_wire, wire_dir)?;
    let legacy_dir = root_dir.join(pre_wire_state_dir_name());
    copy_state_dir_if_needed(&legacy_dir, wire_dir)
}

fn migrate_pre_wire_state(root_dir: &Path, wire_dir: &Path) -> Result<(), String> {
    if wire_dir.exists() {
        return Ok(());
    }
    let legacy_dir = root_dir.join(pre_wire_state_dir_name());
    if !legacy_dir.exists() {
        return Ok(());
    }
    copy_dir_all(&legacy_dir, wire_dir)
}

fn copy_state_dir_if_needed(source: &Path, destination: &Path) -> Result<(), String> {
    if !source.exists() {
        return Ok(());
    }
    if same_path(source, destination) {
        return Ok(());
    }
    copy_dir_all(source, destination)
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn pre_wire_state_dir_name() -> String {
    String::from_utf8(vec![46, 114, 105, 102, 116, 99, 111, 100, 101])
        .unwrap_or_else(|_| ".wirecli".to_string())
}

fn copy_dir_all(source: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_dir_all(&source_path, &destination_path)?;
        } else if !destination_path.exists() {
            fs::copy(&source_path, &destination_path).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

fn ensure_private_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn ensure_project_gitignore_for_wire_state(root_dir: &Path) -> Result<(), String> {
    if !root_dir.join(".git").exists() {
        return Ok(());
    }
    let mut required = vec![format!("{WIRE_STATE_DIR}/"), ".wci/".to_string()];
    let legacy_name = pre_wire_state_dir_name();
    if root_dir.join(&legacy_name).exists() {
        required.push(format!("{legacy_name}/"));
    }
    let path = root_dir.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut missing = Vec::new();
    for entry in required {
        if !gitignore_contains(&existing, &entry) {
            missing.push(entry);
        }
    }
    if missing.is_empty() {
        return Ok(());
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.contains("# Wire CLI local state") {
        next.push_str("\n# Wire CLI local state\n");
    }
    for entry in missing {
        next.push_str(&entry);
        next.push('\n');
    }
    fs::write(path, next).map_err(|e| e.to_string())
}

fn gitignore_contains(raw: &str, needle: &str) -> bool {
    raw.lines().any(|line| {
        let line = line.trim();
        line == needle || line == needle.trim_end_matches('/')
    })
}

fn ensure_config_docs(config_dir: &Path) -> Result<(), String> {
    let path = config_dir.join(CONFIG_DOC_FILE_NAME);
    if path.exists() {
        return Ok(());
    }
    write_private_file(&path, CONFIG_DOCS.as_bytes())
}

fn ensure_default_config_file(config_dir: &Path, config_file: &Path) -> Result<(), String> {
    if config_file.exists()
        || config_dir.join("config.json").exists()
        || config_dir.join("config.yaml").exists()
        || config_dir.join("config").exists()
    {
        return Ok(());
    }
    write_private_file(config_file, DEFAULT_CONFIG_TOML.as_bytes())
}

fn scrub_legacy_config_file_if_present(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(path).unwrap_or_default();
    if raw.contains("intentionally inert") && !config_needs_sensitive_migration(&raw) {
        return Ok(());
    }
    write_private_file(
        path,
        b"# Wire CLI migrated this legacy config to config.toml.\n# This file is intentionally inert and contains no credentials.\n",
    )
}

const DEFAULT_CONFIG_TOML: &str = r#"# Wire CLI config. Keep this file private.
model_provider = "openrouter"
base_url = "https://openrouter.ai/api/v1"
model = ""
approvals_reviewer = "user"
permission_mode = "normal"
method = "chat_completions"

[features]
memories = true
auto-context-compact = true
terminal_resize_reflow = true
image_generation = false

[feature_context]
enabled = true
afup = true
flash_cache_memory = true
automatic_context_compaction = true
acc_model = "openrouter/free"
fcm_max_entries = 192
"#;

const CONFIG_DOCS: &str = r#"# Wire CLI Configuration

This directory is private Wire CLI state. It is stored under the current user's home directory:

- Linux: `~/.wirecli/config/`
- Windows: `%USERPROFILE%\.wirecli\config\`

Wire CLI no longer writes provider configuration into the project checkout by default. If an older project-local `.wirecli` or legacy state directory exists, Wire copies its contents into the user-home state directory and leaves the original directory in place for manual review. When the current project is a Git repository, Wire adds `.wirecli/` to `.gitignore` so accidental local state does not get committed.

## config.toml

`config.toml` is the canonical config file. `config.json` is legacy only. When Wire reads an old JSON config, it writes a fresh TOML config and scrubs the legacy JSON file so provider credentials are not left behind in plaintext.

Minimal example:

```toml
model_provider = "openrouter"
model = ""
approvals_reviewer = "user"
method = "chat_completions"

[features]
memories = true
auto-context-compact = true
terminal_resize_reflow = true
image_generation = false

[feature_context]
enabled = true
afup = true # Adaptive Framework for User Patterns
flash_cache_memory = true
automatic_context_compaction = true # ACC
acc_model = "openrouter/free" # prefer fast, inexpensive models
fcm_max_entries = 192
```

## Secrets

Never paste raw provider keys into documentation, session transcripts, skills, hooks, or MCP config. Wire supports provider keys through environment variables:

```toml
api_key_env = "OPENAI_API_KEY"
```

OpenRouter uses the built-in PKCE login flow. If that flow stores an API key or account metadata locally, Wire encrypts those values before writing them to `config.toml`.

Encrypted values look like:

```toml
api_key = "wireenc:v1:..."
account_id = "wireenc:v1:..."
```

The local key lives in `secret.key` with private file permissions. This is encryption at rest for normal disk exposure and accidental sharing. It is not a defense against malware already running as the same operating-system user; a local stealer with user-level filesystem access can still read both the encrypted config and the local key. For maximum safety, prefer `api_key_env` for providers that support static keys and keep real keys in the operating-system secret store or shell environment.

## Custom Providers

Use OpenAI-compatible provider tables:

```toml
model_provider = "local-router"
model = "router-smart"
model_reasoning_effort = "medium" # optional; only for compatible models

[model-provider.local-router]
name = "Local Router"
base-url = "http://localhost:3000/v1"
method = "chat_completions" # chat_completions or responses
env_key = "LOCAL_ROUTER_API_KEY"
models = ["router-fast", "router-smart", "router-coder"]
```

TOML does not allow repeated `model = ""` keys in the same table. Use `models = ["a", "b"]` for multiple models.

## MCP Servers

MCP servers can be declared directly in `config.toml`:

```toml
[mcp_servers.context7]
url = "https://example.com/mcp"
startup_ts = 120

[mcp_servers.local_docs]
command = "npx"
args = ["-y", "@example/local-docs-mcp"]
startup_ts = 120
```

Do not place tokens in MCP URLs or args. Use environment variables consumed by the server process instead.

## Supported Systems

Wire CLI targets Linux and Windows for end users. macOS support is not a release target yet.
"#;

impl AppConfig {
    pub fn load(paths: &AppPaths) -> Result<Self, String> {
        let mut loaded_legacy_path = None;
        let toml_path = paths.config_file.clone();
        let json_path = paths.config_dir.join("config.json");
        let yaml_path = paths.config_dir.join("config.yaml");
        let bare_legacy_path = paths.config_dir.join("config");
        let has_local_config = toml_path.exists()
            || json_path.exists()
            || yaml_path.exists()
            || bare_legacy_path.exists();
        let raw = if toml_path.exists() {
            fs::read_to_string(&toml_path).map_err(|e| e.to_string())?
        } else if json_path.exists() {
            loaded_legacy_path = Some(json_path.clone());
            fs::read_to_string(&json_path).map_err(|e| e.to_string())?
        } else if bare_legacy_path.exists() {
            loaded_legacy_path = Some(bare_legacy_path.clone());
            fs::read_to_string(&bare_legacy_path).map_err(|e| e.to_string())?
        } else {
            let legacy = if toml_path.exists() {
                toml_path
            } else {
                yaml_path
            };
            if legacy.exists() {
                loaded_legacy_path = Some(legacy.clone());
                fs::read_to_string(&legacy).map_err(|e| e.to_string())?
            } else {
                let env_constrains_provider = env_declares_provider_or_base_url();
                let mut config = Self::default()
                    .with_env_overrides()
                    .with_provider_compatibility();
                if config.model.trim().is_empty() {
                    config = if env_constrains_provider {
                        config.with_compatible_global_model_status()
                    } else {
                        config.with_global_model_status()
                    }
                    .with_provider_compatibility();
                }
                return Ok(config);
            }
        };

        let needs_sensitive_migration = config_needs_sensitive_migration(&raw);
        let local_provider_or_base_configured = raw_declares_provider_or_base_url(&raw);
        let config = if raw.trim_start().starts_with('{') {
            Self::from_json(&raw, &paths.secret_key_file)?
        } else {
            Self::from_legacy_kv(&raw, &paths.secret_key_file)?
        };
        let config = config.with_provider_compatibility();

        if needs_sensitive_migration || loaded_legacy_path.is_some() {
            config.save(paths)?;
            if let Some(legacy_path) = loaded_legacy_path {
                write_private_file(
                    &legacy_path,
                    b"# Wire CLI migrated this legacy config to config.toml.\n# This file is intentionally inert and contains no credentials.\n",
                )?;
            }
        }
        scrub_legacy_config_file_if_present(&json_path)?;
        scrub_legacy_config_file_if_present(&bare_legacy_path)?;

        let mut config = config.with_env_overrides().with_provider_compatibility();
        if !has_local_config || config.model.trim().is_empty() {
            let provider_or_base_constrained =
                local_provider_or_base_configured || env_declares_provider_or_base_url();
            config = if provider_or_base_constrained {
                config.with_compatible_global_model_status()
            } else {
                config.with_global_model_status()
            }
            .with_provider_compatibility();
        }
        Ok(config)
    }

    pub fn save(&self, paths: &AppPaths) -> Result<(), String> {
        let contents = self.to_file_contents(paths)?;
        write_private_file(&paths.config_file, contents.as_bytes())
    }

    pub fn to_file_contents(&self, paths: &AppPaths) -> Result<String, String> {
        let api_key = protect_optional(&paths.secret_key_file, &self.api_key)?;
        let wire_session_token =
            protect_optional(&paths.secret_key_file, &self.wire_session_token)?;
        let account_id = protect_optional(&paths.secret_key_file, &self.account_id)?;
        let account_name = protect_optional(&paths.secret_key_file, &self.account_name)?;
        let account_email = protect_optional(&paths.secret_key_file, &self.account_email)?;
        let mut out = String::new();
        out.push_str("# Wire CLI config. Keep this file private.\n");
        push_toml_string(&mut out, "model_provider", &self.provider);
        push_toml_string(&mut out, "base_url", &self.base_url);
        push_toml_string(&mut out, "model", &self.model);
        push_toml_string(&mut out, "approvals_reviewer", &self.approvals_reviewer);
        if let Some(value) = self.model_reasoning_effort.as_deref() {
            push_toml_string(&mut out, "model_reasoning_effort", value);
        }
        if let Some(value) = self.api_key_env.as_deref() {
            push_toml_string(&mut out, "api_key_env", value);
        }
        if let Some(value) = api_key.as_deref() {
            push_toml_string(&mut out, "api_key", value);
        }
        if let Some(value) = wire_session_token.as_deref() {
            push_toml_string(&mut out, "wire_session_token", value);
        }
        if let Some(value) = account_id.as_deref() {
            push_toml_string(&mut out, "account_id", value);
        }
        if let Some(value) = account_name.as_deref() {
            push_toml_string(&mut out, "account_name", value);
        }
        if let Some(value) = account_email.as_deref() {
            push_toml_string(&mut out, "account_email", value);
        }
        if let Some(workspace) = self.workspace.as_ref() {
            push_toml_string(&mut out, "workspace", &workspace.display().to_string());
        }
        push_toml_string(&mut out, "permission_mode", self.permission_mode.as_str());
        push_toml_string(&mut out, "method", self.protocol.as_str());

        out.push_str("\n[features]\n");
        push_toml_bool(&mut out, "memories", self.features.memories);
        push_toml_bool(
            &mut out,
            "auto-context-compact",
            self.features.auto_context_compact,
        );
        push_toml_bool(
            &mut out,
            "terminal_resize_reflow",
            self.features.terminal_resize_reflow,
        );
        push_toml_bool(&mut out, "image_generation", self.features.image_generation);

        out.push_str("\n[feature_context]\n");
        push_toml_bool(&mut out, "enabled", self.feature_context.enabled);
        push_toml_bool(&mut out, "afup", self.feature_context.afup);
        push_toml_bool(
            &mut out,
            "flash_cache_memory",
            self.feature_context.flash_cache_memory,
        );
        push_toml_bool(
            &mut out,
            "automatic_context_compaction",
            self.feature_context.automatic_context_compaction,
        );
        push_toml_string(&mut out, "acc_model", &self.feature_context.acc_model);
        push_toml_usize(
            &mut out,
            "fcm_max_entries",
            self.feature_context.fcm_max_entries,
        );

        for provider in &self.model_providers {
            if provider.id.trim().is_empty() {
                continue;
            }
            out.push_str("\n[model-provider.");
            out.push_str(&provider.id);
            out.push_str("]\n");
            if let Some(name) = provider.name.as_deref() {
                push_toml_string(&mut out, "name", name);
            }
            push_toml_string(&mut out, "base-url", &provider.base_url);
            push_toml_string(&mut out, "method", provider.protocol.as_str());
            if let Some(api_key_env) = provider.api_key_env.as_deref() {
                push_toml_string(&mut out, "env_key", api_key_env);
            }
            push_toml_array(&mut out, "models", &provider.models);
        }

        out.push_str(&preserved_toml_sections(
            &paths.config_file,
            &["mcp_servers.", "mcp_server."],
        )?);

        Ok(out)
    }

    pub fn requires_login(&self) -> bool {
        self.base_url.trim().is_empty() || !self.has_api_key()
    }

    pub fn requires_model_selection(&self) -> bool {
        self.model.trim().is_empty()
    }

    pub fn has_api_key(&self) -> bool {
        if !self.api_key.as_deref().unwrap_or("").trim().is_empty() {
            return true;
        }

        self.api_key_env
            .as_deref()
            .and_then(|name| env::var(name).ok())
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
    }

    pub fn clear_saved_login(&mut self) {
        self.api_key = None;
        self.wire_session_token = None;
        self.account_id = None;
        self.account_name = None;
        self.account_email = None;
    }

    pub fn account_summary(&self) -> String {
        match (
            self.account_name
                .as_deref()
                .filter(|value| !value.trim().is_empty()),
            self.account_email
                .as_deref()
                .filter(|value| !value.trim().is_empty()),
            self.account_id
                .as_deref()
                .filter(|value| !value.trim().is_empty()),
        ) {
            (Some(name), Some(email), Some(id)) => format!("{name} <{email}> ({id})"),
            (Some(name), Some(email), None) => format!("{name} <{email}>"),
            (Some(name), None, Some(id)) => format!("{name} ({id})"),
            (None, Some(email), Some(id)) => format!("{email} ({id})"),
            (Some(name), None, None) => name.to_string(),
            (None, Some(email), None) => email.to_string(),
            (None, None, Some(id)) => format!("Wire account {id}"),
            (None, None, None) if provider_uses_openrouter_pkce(&self.provider) => {
                "OpenRouter not connected".to_string()
            }
            (None, None, None) => "not connected".to_string(),
        }
    }

    pub fn provider_status_label(&self) -> String {
        if self.provider.trim().is_empty() {
            "not configured".to_string()
        } else {
            self.provider.clone()
        }
    }

    fn from_json(raw: &str, key_path: &PathBuf) -> Result<Self, String> {
        let value: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        Ok(Self {
            provider: value
                .get("provider")
                .or_else(|| value.get("model_provider"))
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            base_url: value
                .get("base_url")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            model: value
                .get("model")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            approvals_reviewer: value
                .get("approvals_reviewer")
                .and_then(|value| value.as_str())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or("user")
                .to_string(),
            model_reasoning_effort: string_or_none(&value, "model_reasoning_effort"),
            api_key_env: string_or_none(&value, "api_key_env"),
            api_key: protected_string_or_none(&value, "api_key", key_path)?,
            wire_session_token: protected_string_or_none(&value, "wire_session_token", key_path)?,
            account_id: protected_string_or_none(&value, "account_id", key_path)?,
            account_name: protected_string_or_none(&value, "account_name", key_path)?,
            account_email: protected_string_or_none(&value, "account_email", key_path)?,
            workspace: value
                .get("workspace")
                .and_then(|value| value.as_str())
                .and_then(|value| {
                    if value.trim().is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(value))
                    }
                }),
            permission_mode: value
                .get("permission_mode")
                .and_then(|value| value.as_str())
                .map(PermissionMode::from_config)
                .unwrap_or(PermissionMode::Normal),
            protocol: value
                .get("protocol")
                .or_else(|| value.get("method"))
                .or_else(|| value.get("wire_api"))
                .and_then(|value| value.as_str())
                .map(ProviderProtocol::from_config)
                .unwrap_or(ProviderProtocol::ChatCompletions),
            features: features_from_value(value.get("features")),
            feature_context: feature_context_from_value(
                value
                    .get("feature_context")
                    .or_else(|| value.get("feature-context"))
                    .or_else(|| value.get("context_features"))
                    .or_else(|| value.get("context-features")),
                value.get("features"),
            ),
            model_providers: custom_providers_from_json(value.get("model_providers"))
                .or_else(|| custom_providers_from_json(value.get("model-provider")))
                .unwrap_or_default(),
        })
    }

    fn from_legacy_kv(raw: &str, key_path: &PathBuf) -> Result<Self, String> {
        let parsed = parse_toml_like_config(raw);
        let mut entries = parsed.entries;

        Ok(Self {
            provider: entries
                .remove("provider")
                .or_else(|| entries.remove("model_provider"))
                .unwrap_or_default(),
            base_url: entries
                .remove("base_url")
                .or_else(|| entries.remove("baseurl"))
                .unwrap_or_default(),
            model: entries.remove("model").unwrap_or_default(),
            approvals_reviewer: entries
                .remove("approvals_reviewer")
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "user".to_string()),
            model_reasoning_effort: entries
                .remove("model_reasoning_effort")
                .filter(|value| !value.trim().is_empty()),
            api_key_env: entries
                .remove("api_key_env")
                .or_else(|| entries.remove("env_key"))
                .and_then(|value| if value.is_empty() { None } else { Some(value) }),
            api_key: decrypt_legacy_optional(entries.remove("api_key"), key_path)?,
            wire_session_token: decrypt_legacy_optional(
                entries.remove("wire_session_token"),
                key_path,
            )?,
            account_id: decrypt_legacy_optional(entries.remove("account_id"), key_path)?,
            account_name: decrypt_legacy_optional(entries.remove("account_name"), key_path)?,
            account_email: decrypt_legacy_optional(entries.remove("account_email"), key_path)?,
            workspace: entries.remove("workspace").and_then(|value| {
                if value.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(value))
                }
            }),
            permission_mode: entries
                .remove("permission_mode")
                .map(|value| PermissionMode::from_config(&value))
                .unwrap_or(PermissionMode::Normal),
            protocol: entries
                .remove("protocol")
                .or_else(|| entries.remove("method"))
                .or_else(|| entries.remove("wire_api"))
                .map(|value| ProviderProtocol::from_config(&value))
                .unwrap_or(ProviderProtocol::ChatCompletions),
            features: features_from_entries(&entries),
            feature_context: feature_context_from_entries(&entries),
            model_providers: parsed.model_providers,
        })
    }

    fn with_env_overrides(mut self) -> Self {
        let mut provider_overridden = false;
        let mut base_url_overridden = false;
        let mut model_overridden = false;
        if let Ok(provider) = env::var("WIRE_PROVIDER") {
            if !provider.trim().is_empty() {
                self.provider = provider;
                provider_overridden = true;
            }
        }
        if let Ok(base_url) = env::var("WIRE_BASE_URL").or_else(|_| env::var("OPENAI_BASE_URL")) {
            if !base_url.trim().is_empty() {
                self.base_url = base_url;
                base_url_overridden = true;
            }
        }
        if let Ok(model) = env::var("WIRE_MODEL").or_else(|_| env::var("OPENAI_MODEL")) {
            if !model.trim().is_empty() {
                self.model = model;
                model_overridden = true;
            }
        }
        if let Ok(api_key_env) = env::var("WIRE_API_KEY_ENV") {
            if !api_key_env.trim().is_empty() {
                self.api_key_env = Some(api_key_env);
            }
        }
        if provider_overridden || base_url_overridden || model_overridden {
            self.provider = self.provider.trim().to_string();
            self.base_url = self.base_url.trim().to_string();
            self.model = self.model.trim().to_string();
        }
        self
    }

    fn with_provider_compatibility(mut self) -> Self {
        if self.provider.trim().is_empty() {
            self.provider = "openrouter".to_string();
        }
        if self.base_url.trim().is_empty() && self.provider == "openrouter" {
            self.base_url = "https://openrouter.ai/api/v1".to_string();
        }
        if self.approvals_reviewer.trim().is_empty() {
            self.approvals_reviewer = "user".to_string();
        }
        if self.protocol == ProviderProtocol::Responses
            && official_wireai_relay_base(&self.base_url)
        {
            self.protocol = ProviderProtocol::ChatCompletions;
        }
        if let Some(custom) = self
            .model_providers
            .iter()
            .find(|provider| provider.id == self.provider)
        {
            if self.base_url.trim().is_empty() {
                self.base_url = custom.base_url.clone();
            }
            let model_is_not_listed = !self.model.trim().is_empty()
                && !custom.models.is_empty()
                && !custom.models.iter().any(|model| model == self.model.trim());
            if model_is_not_listed {
                self.model.clear();
            }
            if self.model.trim().is_empty() {
                if let Some(model) = custom.models.first() {
                    self.model = model.clone();
                }
            }
            if self.api_key_env.is_none() {
                self.api_key_env = custom.api_key_env.clone();
            }
            self.protocol = custom.protocol;
        }
        self
    }

    fn with_global_model_status(mut self) -> Self {
        let Some(status) = load_global_model_status() else {
            return self;
        };
        self = self.with_global_model_status_value(status, true);
        self
    }

    fn with_compatible_global_model_status(mut self) -> Self {
        let Some(status) = load_global_model_status() else {
            return self;
        };
        self = self.with_global_model_status_value(status, false);
        self
    }

    fn with_global_model_status_value(
        mut self,
        status: GlobalModelStatus,
        allow_provider_switch: bool,
    ) -> Self {
        if !allow_provider_switch && !global_model_status_matches_config(&self, &status) {
            return self;
        }
        if !status.provider.trim().is_empty() {
            if allow_provider_switch || self.provider.trim().is_empty() {
                self.provider = status.provider;
            }
        }
        if !status.base_url.trim().is_empty() {
            if allow_provider_switch || self.base_url.trim().is_empty() {
                self.base_url = status.base_url;
            }
        }
        if !status.model.trim().is_empty() {
            self.model = status.model;
        }
        if allow_provider_switch || self.api_key_env.is_none() {
            self.api_key_env = status.api_key_env;
        }
        self.protocol = status.protocol;
        self
    }
}

pub fn save_global_model_status(config: &AppConfig) -> Result<(), String> {
    if config.provider.trim().is_empty() || config.model.trim().is_empty() {
        return Ok(());
    }
    let Some(path) = global_model_status_file() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let status = GlobalModelStatus {
        provider: config.provider.clone(),
        model: config.model.clone(),
        base_url: config.base_url.clone(),
        api_key_env: config.api_key_env.clone(),
        protocol: config.protocol,
    };
    let mut raw = String::new();
    push_toml_string(&mut raw, "provider", &status.provider);
    push_toml_string(&mut raw, "model", &status.model);
    push_toml_string(&mut raw, "base_url", &status.base_url);
    if let Some(api_key_env) = status.api_key_env.as_deref() {
        push_toml_string(&mut raw, "api_key_env", api_key_env);
    }
    push_toml_string(&mut raw, "method", status.protocol.as_str());
    write_private_file(&path, raw.as_bytes())
}

fn load_global_model_status() -> Option<GlobalModelStatus> {
    let path = global_model_status_file()?;
    let raw = fs::read_to_string(path).ok()?;
    if raw.trim_start().starts_with('{') {
        return serde_json::from_str(&raw).ok();
    }
    let parsed = parse_toml_like_config(&raw);
    Some(GlobalModelStatus {
        provider: parsed.entries.get("provider").cloned().unwrap_or_default(),
        model: parsed.entries.get("model").cloned().unwrap_or_default(),
        base_url: parsed.entries.get("base_url").cloned().unwrap_or_default(),
        api_key_env: parsed.entries.get("api_key_env").cloned(),
        protocol: parsed
            .entries
            .get("method")
            .or_else(|| parsed.entries.get("protocol"))
            .map(|value| ProviderProtocol::from_config(value))
            .unwrap_or(ProviderProtocol::ChatCompletions),
    })
}

fn global_model_status_file() -> Option<PathBuf> {
    wire_home_dir()
        .ok()
        .map(|path| path.join(WIRE_STATE_DIR).join("config/model-status.toml"))
}

fn global_model_status_matches_config(config: &AppConfig, status: &GlobalModelStatus) -> bool {
    if status.provider.trim().is_empty() {
        return false;
    }
    if !config.provider.trim().is_empty()
        && normalize_provider_for_compare(&config.provider)
            != normalize_provider_for_compare(&status.provider)
    {
        return false;
    }
    if !config.base_url.trim().is_empty()
        && !status.base_url.trim().is_empty()
        && normalize_base_url_for_compare(&config.base_url)
            != normalize_base_url_for_compare(&status.base_url)
    {
        return false;
    }
    true
}

fn raw_declares_provider_or_base_url(raw: &str) -> bool {
    if raw.trim_start().starts_with('{') {
        return serde_json::from_str::<Value>(raw)
            .ok()
            .map(|value| {
                ["provider", "model_provider", "base_url", "base-url"]
                    .iter()
                    .any(|key| {
                        value
                            .get(key)
                            .and_then(|value| value.as_str())
                            .map(|value| !value.trim().is_empty())
                            .unwrap_or(false)
                    })
            })
            .unwrap_or(false);
    }
    let parsed = parse_toml_like_config(raw);
    ["provider", "model_provider", "base_url", "baseurl"]
        .iter()
        .any(|key| {
            parsed
                .entries
                .get(*key)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        })
}

fn env_declares_provider_or_base_url() -> bool {
    ["WIRE_PROVIDER", "WIRE_BASE_URL", "OPENAI_BASE_URL"]
        .iter()
        .any(|key| {
            env::var(key)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
        })
}

fn normalize_provider_for_compare(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalize_base_url_for_compare(value: &str) -> String {
    value.trim().trim_end_matches('/').to_ascii_lowercase()
}

#[derive(Default)]
struct ParsedTomlConfig {
    entries: HashMap<String, String>,
    model_providers: Vec<CustomModelProvider>,
}

#[derive(Default)]
struct CustomProviderDraft {
    id: String,
    name: Option<String>,
    base_url: Option<String>,
    models: Vec<String>,
    api_key_env: Option<String>,
    protocol: Option<ProviderProtocol>,
}

fn push_toml_string(out: &mut String, key: &str, value: &str) {
    out.push_str(key);
    out.push_str(" = \"");
    out.push_str(&toml_escape(value));
    out.push_str("\"\n");
}

fn push_toml_bool(out: &mut String, key: &str, value: bool) {
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(if value { "true" } else { "false" });
    out.push('\n');
}

fn push_toml_usize(out: &mut String, key: &str, value: usize) {
    out.push_str(key);
    out.push_str(" = ");
    out.push_str(&value.to_string());
    out.push('\n');
}

fn push_toml_array(out: &mut String, key: &str, values: &[String]) {
    out.push_str(key);
    out.push_str(" = [");
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push('"');
        out.push_str(&toml_escape(value));
        out.push('"');
    }
    out.push_str("]\n");
}

fn toml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(ch),
        }
    }
    out
}

fn preserved_toml_sections(path: &Path, prefixes: &[&str]) -> Result<String, String> {
    if !path.exists() {
        return Ok(String::new());
    }
    let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut out = String::new();
    let mut keep = false;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            let section = trimmed.trim_start_matches('[').trim_end_matches(']').trim();
            keep = prefixes.iter().any(|prefix| section.starts_with(prefix));
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.is_empty() {
        Ok(out)
    } else {
        Ok(format!("\n{out}"))
    }
}

fn merge_config_toml_overlay(paths: &AppPaths, config: &mut AppConfig) -> Result<(), String> {
    let path = paths.config_file.clone();
    if !path.exists() {
        return Ok(());
    }
    let raw = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let parsed = parse_toml_like_config(&raw);
    if let Some(provider) = parsed
        .entries
        .get("model_provider")
        .or_else(|| parsed.entries.get("provider"))
        .filter(|value| !value.trim().is_empty())
    {
        config.provider = provider.clone();
    }
    if let Some(model) = parsed
        .entries
        .get("model")
        .filter(|value| !value.trim().is_empty())
    {
        config.model = model.clone();
    }
    if let Some(base_url) = parsed
        .entries
        .get("base_url")
        .or_else(|| parsed.entries.get("baseurl"))
        .filter(|value| !value.trim().is_empty())
    {
        config.base_url = base_url.clone();
    }
    if let Some(method) = parsed
        .entries
        .get("method")
        .or_else(|| parsed.entries.get("protocol"))
        .or_else(|| parsed.entries.get("wire_api"))
        .filter(|value| !value.trim().is_empty())
    {
        config.protocol = ProviderProtocol::from_config(method);
    }
    if let Some(api_key_env) = parsed
        .entries
        .get("api_key_env")
        .or_else(|| parsed.entries.get("env_key"))
        .filter(|value| !value.trim().is_empty())
    {
        config.api_key_env = Some(api_key_env.clone());
    }
    if !parsed.model_providers.is_empty() {
        config.model_providers = parsed.model_providers;
    }
    Ok(())
}

fn parse_toml_like_config(raw: &str) -> ParsedTomlConfig {
    let mut entries = HashMap::new();
    let mut section = String::new();
    let mut providers: BTreeMap<String, CustomProviderDraft> = BTreeMap::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_string();
            continue;
        }

        let Some((raw_key, raw_value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = normalize_config_key(raw_key);
        if key.is_empty() {
            continue;
        }
        let value = strip_toml_comment(raw_value).trim().to_string();

        if let Some((provider_id, model_section)) = model_provider_section(&section) {
            update_custom_provider(&mut providers, &provider_id, model_section, &key, &value);
            continue;
        }

        if !section.is_empty() && !is_known_config_section(&section) && custom_provider_key(&key) {
            update_custom_provider(&mut providers, &section, false, &key, &value);
            continue;
        }

        let parsed_value = parse_toml_string(&value);
        let full_key = if section.is_empty() {
            key
        } else {
            format!("{}.{}", normalize_section_name(&section), key)
        };
        entries.insert(full_key, parsed_value);
    }

    ParsedTomlConfig {
        entries,
        model_providers: providers
            .into_values()
            .filter_map(custom_provider_from_draft)
            .collect(),
    }
}

fn model_provider_section(section: &str) -> Option<(String, bool)> {
    let mut parts = section.split('.').map(str::trim);
    let root = parts.next()?;
    let root = normalize_section_name(root);
    if !matches!(
        root.as_str(),
        "model_provider" | "model_providers" | "modelprovider" | "modelproviders"
    ) {
        return None;
    }
    let id = parts.next()?.trim();
    if id.is_empty() {
        return None;
    }
    let model_section = parts
        .next()
        .map(normalize_section_name)
        .map(|value| value == "models")
        .unwrap_or(false);
    Some((id.to_string(), model_section))
}

fn update_custom_provider(
    providers: &mut BTreeMap<String, CustomProviderDraft>,
    provider_id: &str,
    model_section: bool,
    key: &str,
    raw_value: &str,
) {
    let id = provider_id.trim().to_string();
    if id.is_empty() {
        return;
    }
    let draft = providers
        .entry(id.clone())
        .or_insert_with(|| CustomProviderDraft {
            id,
            ..CustomProviderDraft::default()
        });

    if model_section {
        if matches!(key, "model" | "id" | "name") {
            push_unique_model(&mut draft.models, parse_toml_string(raw_value));
        }
        return;
    }

    match key {
        "name" | "label" => draft.name = Some(parse_toml_string(raw_value)),
        "base_url" | "baseurl" => draft.base_url = Some(parse_toml_string(raw_value)),
        "model" | "default_model" => {
            push_unique_model(&mut draft.models, parse_toml_string(raw_value))
        }
        "models" => {
            for model in parse_toml_string_list(raw_value) {
                push_unique_model(&mut draft.models, model);
            }
        }
        "api_key_env" | "env_key" => draft.api_key_env = Some(parse_toml_string(raw_value)),
        "method" | "protocol" | "wire_api" => {
            draft.protocol = Some(ProviderProtocol::from_config(&parse_toml_string(raw_value)))
        }
        _ => {}
    }
}

fn push_unique_model(models: &mut Vec<String>, model: String) {
    let model = model.trim();
    if model.is_empty() || models.iter().any(|value| value == model) {
        return;
    }
    models.push(model.to_string());
}

fn custom_provider_from_draft(draft: CustomProviderDraft) -> Option<CustomModelProvider> {
    let base_url = draft.base_url.unwrap_or_default();
    if base_url.trim().is_empty() && draft.models.is_empty() {
        return None;
    }
    Some(CustomModelProvider {
        id: draft.id,
        name: draft.name.filter(|value| !value.trim().is_empty()),
        base_url,
        models: draft.models,
        api_key_env: draft.api_key_env.filter(|value| !value.trim().is_empty()),
        protocol: draft.protocol.unwrap_or(ProviderProtocol::ChatCompletions),
    })
}

fn custom_providers_from_json(value: Option<&Value>) -> Option<Vec<CustomModelProvider>> {
    let value = value?;
    match value {
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(custom_provider_from_json_value)
                .collect(),
        ),
        Value::Object(map) => Some(
            map.iter()
                .filter_map(|(id, value)| {
                    let mut provider = custom_provider_from_json_value(value)?;
                    if provider.id.trim().is_empty() {
                        provider.id = id.clone();
                    }
                    Some(provider)
                })
                .collect(),
        ),
        _ => None,
    }
}

fn custom_provider_from_json_value(value: &Value) -> Option<CustomModelProvider> {
    let object = value.as_object()?;
    let id = object
        .get("id")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let name = object
        .get("name")
        .or_else(|| object.get("label"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.to_string());
    let base_url = object
        .get("base_url")
        .or_else(|| object.get("base-url"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let mut models = Vec::new();
    if let Some(model) = object
        .get("model")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    {
        models.push(model.to_string());
    }
    if let Some(values) = object.get("models").and_then(|value| value.as_array()) {
        for value in values {
            if let Some(model) = value.as_str().filter(|value| !value.trim().is_empty()) {
                push_unique_model(&mut models, model.to_string());
            }
        }
    }
    Some(CustomModelProvider {
        id,
        name,
        base_url,
        models,
        api_key_env: object
            .get("api_key_env")
            .or_else(|| object.get("env_key"))
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_string()),
        protocol: object
            .get("protocol")
            .or_else(|| object.get("method"))
            .or_else(|| object.get("wire_api"))
            .and_then(|value| value.as_str())
            .map(ProviderProtocol::from_config)
            .unwrap_or(ProviderProtocol::ChatCompletions),
    })
}

fn parse_toml_string_list(raw: &str) -> Vec<String> {
    let value = strip_toml_comment(raw).trim().to_string();
    if !value.starts_with('[') || !value.ends_with(']') {
        return vec![parse_toml_string(&value)];
    }
    let inner = value.trim_start_matches('[').trim_end_matches(']');
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in inner.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                out.push(current.trim().to_string());
                current.clear();
            }
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == ',' => {
                let value = current.trim();
                if !value.is_empty() {
                    out.push(parse_toml_string(value));
                }
                current.clear();
            }
            None => current.push(ch),
        }
    }
    let value = current.trim();
    if !value.is_empty() {
        out.push(parse_toml_string(value));
    }
    out.into_iter()
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn parse_toml_string(raw: &str) -> String {
    let value = strip_toml_comment(raw)
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string();
    unescape_toml_basic(&value)
}

fn unescape_toml_basic(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn strip_toml_comment(raw: &str) -> String {
    let mut out = String::new();
    let mut quote: Option<char> = None;
    for ch in raw.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                out.push(ch);
            }
            Some(_) => out.push(ch),
            None if ch == '"' || ch == '\'' => {
                quote = Some(ch);
                out.push(ch);
            }
            None if ch == '#' => break,
            None => out.push(ch),
        }
    }
    out
}

fn normalize_config_key(key: &str) -> String {
    key.trim().replace('-', "_")
}

fn normalize_section_name(section: &str) -> String {
    section.trim().replace('-', "_")
}

fn custom_provider_key(key: &str) -> bool {
    matches!(
        key,
        "name"
            | "label"
            | "base_url"
            | "baseurl"
            | "model"
            | "models"
            | "default_model"
            | "api_key_env"
            | "env_key"
            | "method"
            | "protocol"
            | "wire_api"
    )
}

fn is_known_config_section(section: &str) -> bool {
    let section = normalize_section_name(section);
    section == "features"
        || section.starts_with("mcp_servers.")
        || section.starts_with("mcp_server.")
}

fn official_wireai_relay_base(base_url: &str) -> bool {
    let value = base_url.trim().to_ascii_lowercase();
    value.contains("localhost")
        || value.contains("127.0.0.1")
        || value.contains("wireai")
        || value.contains("wire-api")
}

fn string_or_none(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(|value| value.as_str())
        .and_then(|value| {
            if value.trim().is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        })
}

fn features_from_value(value: Option<&Value>) -> AppFeatures {
    let mut features = AppFeatures::default();
    let Some(value) = value else {
        return features;
    };
    if let Some(enabled) = value.get("memories").and_then(|value| value.as_bool()) {
        features.memories = enabled;
    }
    if let Some(enabled) = value
        .get("auto_context_compact")
        .or_else(|| value.get("auto-context-compact"))
        .and_then(|value| value.as_bool())
    {
        features.auto_context_compact = enabled;
    }
    if let Some(enabled) = value
        .get("terminal_resize_reflow")
        .or_else(|| value.get("terminal-resize-reflow"))
        .and_then(|value| value.as_bool())
    {
        features.terminal_resize_reflow = enabled;
    }
    if let Some(enabled) = value
        .get("image_generation")
        .or_else(|| value.get("image-generation"))
        .and_then(|value| value.as_bool())
    {
        features.image_generation = enabled;
    }
    features
}

fn features_from_entries(entries: &HashMap<String, String>) -> AppFeatures {
    let mut features = AppFeatures::default();
    if let Some(enabled) = parse_bool_entry(entries, "features.memories") {
        features.memories = enabled;
    }
    if let Some(enabled) = parse_bool_entry(entries, "features.auto-context-compact")
        .or_else(|| parse_bool_entry(entries, "features.auto_context_compact"))
    {
        features.auto_context_compact = enabled;
    }
    if let Some(enabled) = parse_bool_entry(entries, "features.terminal_resize_reflow")
        .or_else(|| parse_bool_entry(entries, "features.terminal-resize-reflow"))
    {
        features.terminal_resize_reflow = enabled;
    }
    if let Some(enabled) = parse_bool_entry(entries, "features.image_generation")
        .or_else(|| parse_bool_entry(entries, "features.image-generation"))
    {
        features.image_generation = enabled;
    }
    features
}

fn feature_context_from_value(
    value: Option<&Value>,
    legacy_features: Option<&Value>,
) -> FeatureContextConfig {
    let mut config = FeatureContextConfig::default();
    if let Some(features) = legacy_features {
        if let Some(enabled) = features
            .get("auto_context_compact")
            .or_else(|| features.get("auto-context-compact"))
            .and_then(|value| value.as_bool())
        {
            config.automatic_context_compaction = enabled;
        }
        if let Some(enabled) = features.get("memories").and_then(|value| value.as_bool()) {
            config.enabled = enabled;
        }
    }
    let Some(value) = value else {
        return config;
    };
    if let Some(enabled) = value.get("enabled").and_then(|value| value.as_bool()) {
        config.enabled = enabled;
    }
    if let Some(enabled) = value.get("afup").and_then(|value| value.as_bool()) {
        config.afup = enabled;
    }
    if let Some(enabled) = value
        .get("flash_cache_memory")
        .or_else(|| value.get("flash-cache-memory"))
        .or_else(|| value.get("fcm"))
        .and_then(|value| value.as_bool())
    {
        config.flash_cache_memory = enabled;
    }
    if let Some(enabled) = value
        .get("automatic_context_compaction")
        .or_else(|| value.get("automatic-context-compaction"))
        .or_else(|| value.get("auto_context_compaction"))
        .or_else(|| value.get("auto-context-compaction"))
        .or_else(|| value.get("autocontextcompacting"))
        .and_then(|value| value.as_bool())
    {
        config.automatic_context_compaction = enabled;
    }
    if let Some(model) = value
        .get("acc_model")
        .or_else(|| value.get("acc-model"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
    {
        config.acc_model = model.trim().to_string();
    }
    if let Some(max_entries) = value
        .get("fcm_max_entries")
        .or_else(|| value.get("fcm-max-entries"))
        .and_then(|value| value.as_u64())
    {
        config.fcm_max_entries = max_entries.clamp(16, 4096) as usize;
    }
    config
}

fn feature_context_from_entries(entries: &HashMap<String, String>) -> FeatureContextConfig {
    let mut config = FeatureContextConfig::default();
    if let Some(enabled) = parse_bool_entry(entries, "features.memories") {
        config.enabled = enabled;
    }
    if let Some(enabled) = parse_bool_entry(entries, "features.auto-context-compact")
        .or_else(|| parse_bool_entry(entries, "features.auto_context_compact"))
    {
        config.automatic_context_compaction = enabled;
    }
    if let Some(enabled) = parse_bool_entry_any(
        entries,
        &[
            "feature_context.enabled",
            "feature-context.enabled",
            "context_features.enabled",
            "context-features.enabled",
        ],
    ) {
        config.enabled = enabled;
    }
    if let Some(enabled) = parse_bool_entry_any(
        entries,
        &[
            "feature_context.afup",
            "feature-context.afup",
            "context_features.afup",
            "context-features.afup",
        ],
    ) {
        config.afup = enabled;
    }
    if let Some(enabled) = parse_bool_entry_any(
        entries,
        &[
            "feature_context.flash_cache_memory",
            "feature-context.flash_cache_memory",
            "feature_context.flash-cache-memory",
            "feature_context.fcm",
            "context_features.flash_cache_memory",
        ],
    ) {
        config.flash_cache_memory = enabled;
    }
    if let Some(enabled) = parse_bool_entry_any(
        entries,
        &[
            "feature_context.automatic_context_compaction",
            "feature-context.automatic_context_compaction",
            "feature_context.automatic-context-compaction",
            "feature_context.auto_context_compaction",
            "feature_context.auto-context-compaction",
            "feature_context.autocontextcompacting",
            "context_features.automatic_context_compaction",
        ],
    ) {
        config.automatic_context_compaction = enabled;
    }
    if let Some(model) = string_entry_any(
        entries,
        &[
            "feature_context.acc_model",
            "feature-context.acc_model",
            "feature_context.acc-model",
            "context_features.acc_model",
        ],
    )
    .filter(|value| !value.trim().is_empty())
    {
        config.acc_model = model.trim().to_string();
    }
    if let Some(max_entries) = usize_entry_any(
        entries,
        &[
            "feature_context.fcm_max_entries",
            "feature-context.fcm_max_entries",
            "feature_context.fcm-max-entries",
            "context_features.fcm_max_entries",
        ],
    ) {
        config.fcm_max_entries = max_entries.clamp(16, 4096);
    }
    config
}

fn parse_bool_entry_any(entries: &HashMap<String, String>, keys: &[&str]) -> Option<bool> {
    keys.iter().find_map(|key| parse_bool_entry(entries, key))
}

fn string_entry_any(entries: &HashMap<String, String>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| entries.get(*key).cloned())
        .map(|value| parse_toml_string(&value))
}

fn usize_entry_any(entries: &HashMap<String, String>, keys: &[&str]) -> Option<usize> {
    keys.iter().find_map(|key| {
        entries
            .get(*key)
            .map(|value| parse_toml_string(value))
            .and_then(|value| value.parse::<usize>().ok())
    })
}

fn parse_bool_entry(entries: &HashMap<String, String>, key: &str) -> Option<bool> {
    entries
        .get(key)
        .and_then(|value| match strip_quotes(value).as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        })
}

fn strip_quotes(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_ascii_lowercase()
}

fn config_needs_sensitive_migration(raw: &str) -> bool {
    if raw.trim_start().starts_with('{') {
        let Ok(value) = serde_json::from_str::<Value>(raw) else {
            return false;
        };
        return SENSITIVE_CONFIG_KEYS.iter().any(|key| {
            value
                .get(*key)
                .and_then(|value| value.as_str())
                .map(sensitive_value_needs_migration)
                .unwrap_or(false)
        });
    }

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if SENSITIVE_CONFIG_KEYS.contains(&key) && sensitive_value_needs_migration(value) {
            return true;
        }
    }

    false
}

fn sensitive_value_needs_migration(value: &str) -> bool {
    let value = value.trim().trim_matches('"').trim_matches('\'').trim();
    !value.is_empty() && !is_protected_secret(value)
}

fn protect_optional(key_path: &PathBuf, value: &Option<String>) -> Result<Option<String>, String> {
    value
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(|value| protect_secret(key_path, value))
        .transpose()
}

fn protected_string_or_none(
    value: &Value,
    key: &str,
    key_path: &PathBuf,
) -> Result<Option<String>, String> {
    string_or_none(value, key)
        .map(|value| reveal_secret(key_path, &value))
        .transpose()
}

fn decrypt_legacy_optional(
    value: Option<String>,
    key_path: &PathBuf,
) -> Result<Option<String>, String> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| reveal_secret(key_path, &value))
        .transpose()
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
            background: color_from_hex("#050505"),
            border: color_from_hex("#5a3b50"),
            text: color_from_hex("#f7edf4"),
            muted: color_from_hex("#a98fa0"),
            accent: color_from_hex("#ff5fb7"),
            user_text: color_from_hex("#fff4fb"),
            assistant_text: color_from_hex("#e9dce5"),
            tool_text: color_from_hex("#ffd1e8"),
            emphasis: color_from_hex("#ffb3dc"),
            success: color_from_hex("#ff8bd2"),
            danger: color_from_hex("#ff4f7a"),
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

#[cfg(test)]
mod tests {
    use super::{AppConfig, AppPaths};
    use crate::id::next_id;
    use crate::providers::ProviderProtocol;
    use std::fs;

    #[test]
    fn requires_login_accepts_api_key_env() {
        let key_name = "WIRECLI_TEST_API_KEY";
        std::env::set_var(key_name, "token-from-env");
        let config = AppConfig {
            provider: "openrouter".to_string(),
            base_url: "http://localhost:3000/v1".to_string(),
            model: "wireai/auto".to_string(),
            approvals_reviewer: "user".to_string(),
            model_reasoning_effort: None,
            api_key_env: Some(key_name.to_string()),
            api_key: None,
            wire_session_token: None,
            account_id: None,
            account_name: None,
            account_email: None,
            workspace: None,
            permission_mode: super::PermissionMode::Normal,
            protocol: crate::providers::ProviderProtocol::Responses,
            features: super::AppFeatures::default(),
            feature_context: super::FeatureContextConfig::default(),
            model_providers: Vec::new(),
        };

        assert!(!config.requires_login());

        std::env::remove_var(key_name);
    }

    #[test]
    fn login_state_is_separate_from_model_selection() {
        let key_name = "WIRECLI_TEST_MODEL_PICK_KEY";
        std::env::set_var(key_name, "token-from-env");
        let mut config = AppConfig::default();
        config.api_key_env = Some(key_name.to_string());
        config.model.clear();

        assert!(!config.requires_login());
        assert!(config.requires_model_selection());

        std::env::remove_var(key_name);
    }

    #[test]
    fn default_openrouter_config_does_not_preselect_expensive_model() {
        let config = AppConfig::default().with_provider_compatibility();

        assert_eq!(config.provider, "openrouter");
        assert_eq!(config.base_url, "https://openrouter.ai/api/v1");
        assert!(config.model.is_empty());
    }

    #[test]
    fn migrates_pre_wire_state_without_deleting_source() {
        let root_dir =
            std::env::temp_dir().join(format!("wirecli-config-migrate-test-{}", next_id()));
        let source = root_dir.join(super::pre_wire_state_dir_name());
        let destination = root_dir.join(".wirecli");
        fs::create_dir_all(source.join("config")).unwrap();
        fs::write(source.join("config").join("config.json"), "{}").unwrap();

        super::migrate_pre_wire_state(&root_dir, &destination).unwrap();

        assert!(source.join("config").join("config.json").exists());
        assert!(destination.join("config").join("config.json").exists());

        let _ = fs::remove_dir_all(root_dir);
    }

    #[test]
    fn git_repositories_ignore_project_local_wire_state() {
        let root_dir = std::env::temp_dir().join(format!("wirecli-gitignore-test-{}", next_id()));
        fs::create_dir_all(root_dir.join(".git")).unwrap();

        super::ensure_project_gitignore_for_wire_state(&root_dir).unwrap();

        let raw = fs::read_to_string(root_dir.join(".gitignore")).unwrap();
        assert!(raw.contains(".wirecli/"));
        assert!(raw.contains(".wci/"));

        let _ = fs::remove_dir_all(root_dir);
    }

    #[test]
    fn default_config_file_is_toml_not_json() {
        let paths = test_paths();
        fs::create_dir_all(&paths.config_dir).unwrap();

        super::ensure_default_config_file(&paths.config_dir, &paths.config_file).unwrap();

        let raw = fs::read_to_string(&paths.config_file).unwrap();
        assert!(raw.contains("model_provider = \"openrouter\""));
        assert!(!paths.config_dir.join("config.json").exists());

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn official_wireai_base_uses_chat_completions_protocol() {
        let config = AppConfig {
            provider: "openrouter".to_string(),
            base_url: "http://localhost:3000/v1".to_string(),
            model: "qwen/qwen3.7-plus".to_string(),
            approvals_reviewer: "user".to_string(),
            model_reasoning_effort: None,
            api_key_env: None,
            api_key: Some("wai_test".to_string()),
            wire_session_token: None,
            account_id: Some("acct".to_string()),
            account_name: None,
            account_email: None,
            workspace: None,
            permission_mode: super::PermissionMode::Normal,
            protocol: ProviderProtocol::Responses,
            features: super::AppFeatures::default(),
            feature_context: super::FeatureContextConfig::default(),
            model_providers: Vec::new(),
        }
        .with_provider_compatibility();

        assert_eq!(config.protocol, ProviderProtocol::ChatCompletions);
    }

    #[test]
    fn config_toml_loads_custom_model_provider_with_models() {
        let paths = test_paths();
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            paths.config_dir.join("config.toml"),
            r#"
model_provider = "local-router"

[model_provider.local-router]
name = "Local Router"
base-url = "http://localhost:3000/v1"
method = "completations"
env_key = "LOCAL_ROUTER_API_KEY"
models = ["router-fast", "router-smart"]

[model_provider.local-router.models.coder]
model = "router-coder"
"#,
        )
        .unwrap();

        let loaded = AppConfig::load(&paths).unwrap();
        assert_eq!(loaded.provider, "local-router");
        assert_eq!(loaded.base_url, "http://localhost:3000/v1");
        assert_eq!(loaded.model, "router-fast");
        assert_eq!(loaded.protocol, ProviderProtocol::ChatCompletions);
        assert_eq!(loaded.api_key_env.as_deref(), Some("LOCAL_ROUTER_API_KEY"));
        let provider = loaded
            .model_providers
            .iter()
            .find(|provider| provider.id == "local-router")
            .unwrap();
        assert_eq!(
            provider.models,
            vec![
                "router-fast".to_string(),
                "router-smart".to_string(),
                "router-coder".to_string()
            ]
        );

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn config_toml_loads_feature_context() {
        let paths = test_paths();
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            paths.config_dir.join("config.toml"),
            r#"
model_provider = "openrouter"
base_url = "https://openrouter.ai/api/v1"
model = ""
method = "chat_completions"

[features]
memories = true
auto-context-compact = true

[feature_context]
enabled = true
afup = false
flash_cache_memory = true
automatic_context_compaction = true
acc_model = "wire/compact-fast"
fcm_max_entries = 96
"#,
        )
        .unwrap();

        let loaded = AppConfig::load(&paths).unwrap();

        assert!(loaded.memories_enabled());
        assert!(!loaded.afup_enabled());
        assert!(loaded.flash_cache_memory_enabled());
        assert!(loaded.auto_context_compaction_enabled());
        assert_eq!(loaded.acc_model(), "wire/compact-fast");
        assert_eq!(loaded.feature_context.fcm_max_entries, 96);

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn compatible_global_model_status_does_not_cross_provider_boundary() {
        let config = AppConfig {
            provider: "qwenproxy".to_string(),
            base_url: "http://127.0.0.1:3000/v1".to_string(),
            model: String::new(),
            approvals_reviewer: "user".to_string(),
            model_reasoning_effort: None,
            api_key_env: None,
            api_key: Some("local-test-key".to_string()),
            wire_session_token: None,
            account_id: None,
            account_name: None,
            account_email: None,
            workspace: None,
            permission_mode: super::PermissionMode::Normal,
            protocol: ProviderProtocol::ChatCompletions,
            features: super::AppFeatures::default(),
            feature_context: super::FeatureContextConfig::default(),
            model_providers: Vec::new(),
        };
        let status = super::GlobalModelStatus {
            provider: "openrouter".to_string(),
            model: "openrouter/free".to_string(),
            base_url: "https://openrouter.ai/api/v1".to_string(),
            api_key_env: None,
            protocol: ProviderProtocol::ChatCompletions,
        };

        let loaded = config.with_global_model_status_value(status, false);

        assert_eq!(loaded.provider, "qwenproxy");
        assert_eq!(loaded.base_url, "http://127.0.0.1:3000/v1");
        assert!(loaded.model.is_empty());
    }

    #[test]
    fn custom_provider_replaces_incompatible_model_with_configured_model() {
        let config = AppConfig {
            provider: "qwenproxy".to_string(),
            base_url: "http://127.0.0.1:3000/v1".to_string(),
            model: "openrouter/free".to_string(),
            approvals_reviewer: "user".to_string(),
            model_reasoning_effort: None,
            api_key_env: None,
            api_key: Some("local-test-key".to_string()),
            wire_session_token: None,
            account_id: None,
            account_name: None,
            account_email: None,
            workspace: None,
            permission_mode: super::PermissionMode::Normal,
            protocol: ProviderProtocol::ChatCompletions,
            features: super::AppFeatures::default(),
            feature_context: super::FeatureContextConfig::default(),
            model_providers: vec![super::CustomModelProvider {
                id: "qwenproxy".to_string(),
                name: Some("Qwen Proxy".to_string()),
                base_url: "http://127.0.0.1:3000/v1".to_string(),
                models: vec!["qwen3.7-max".to_string()],
                api_key_env: None,
                protocol: ProviderProtocol::ChatCompletions,
            }],
        }
        .with_provider_compatibility();

        assert_eq!(config.model, "qwen3.7-max");
    }

    #[test]
    fn config_save_encrypts_sensitive_values_and_loads_them() {
        let paths = test_paths();
        let secret = "wai_test_config_secret_value";
        let config = AppConfig {
            provider: "openrouter".to_string(),
            base_url: "http://localhost:3000/v1".to_string(),
            model: "wireai/auto".to_string(),
            approvals_reviewer: "user".to_string(),
            model_reasoning_effort: None,
            api_key_env: None,
            api_key: Some(secret.to_string()),
            wire_session_token: Some("wire_session_secret_value".to_string()),
            account_id: Some("acct_secret_value".to_string()),
            account_name: Some("Elaine".to_string()),
            account_email: Some("elaine@example.test".to_string()),
            workspace: None,
            permission_mode: super::PermissionMode::Normal,
            protocol: ProviderProtocol::ChatCompletions,
            features: super::AppFeatures::default(),
            feature_context: super::FeatureContextConfig::default(),
            model_providers: Vec::new(),
        };

        config.save(&paths).unwrap();
        let raw = fs::read_to_string(&paths.config_file).unwrap();
        assert!(raw.contains("model_provider = \"openrouter\""));
        assert!(!raw.trim_start().starts_with('{'));
        assert!(raw.contains("wireenc:v1:"));
        assert!(!raw.contains(secret));
        assert!(!raw.contains("wire_session_secret_value"));
        assert!(!raw.contains("elaine@example.test"));

        let loaded = AppConfig::load(&paths).unwrap();
        assert_eq!(loaded.api_key.as_deref(), Some(secret));
        assert_eq!(
            loaded.wire_session_token.as_deref(),
            Some("wire_session_secret_value")
        );
        assert_eq!(loaded.account_email.as_deref(), Some("elaine@example.test"));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn config_load_migrates_plaintext_sensitive_values() {
        let paths = test_paths();
        fs::create_dir_all(&paths.config_dir).unwrap();
        let secret = "wai_plaintext_config_secret_value";
        let legacy_json = paths.config_dir.join("config.json");
        fs::write(
            &legacy_json,
            serde_json::json!({
                "provider": "openrouter",
                "base_url": "http://localhost:3000/v1",
                "model": "wireai/auto",
                "approvals_reviewer": "user",
                "api_key": secret,
                "wire_session_token": "wire_plaintext_session_token",
                "account_email": "elaine@example.test",
                "permission_mode": "normal",
                "protocol": "chat_completions"
            })
            .to_string(),
        )
        .unwrap();

        let loaded = AppConfig::load(&paths).unwrap();
        assert_eq!(loaded.api_key.as_deref(), Some(secret));
        assert_eq!(
            loaded.wire_session_token.as_deref(),
            Some("wire_plaintext_session_token")
        );

        let raw = fs::read_to_string(&paths.config_file).unwrap();
        let legacy_raw = fs::read_to_string(&legacy_json).unwrap();
        assert!(raw.contains("wireenc:v1:"));
        assert!(raw.contains("model_provider = \"openrouter\""));
        assert!(!raw.contains(secret));
        assert!(!raw.contains("wire_plaintext_session_token"));
        assert!(!raw.contains("elaine@example.test"));
        assert!(!legacy_raw.contains(secret));
        assert!(legacy_raw.contains("migrated"));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    #[test]
    fn config_load_scrubs_legacy_json_even_when_toml_exists() {
        let paths = test_paths();
        fs::create_dir_all(&paths.config_dir).unwrap();
        fs::write(
            &paths.config_file,
            r#"
model_provider = "openrouter"
base_url = "https://openrouter.ai/api/v1"
model = ""
method = "chat_completions"
"#,
        )
        .unwrap();
        let legacy_json = paths.config_dir.join("config.json");
        fs::write(
            &legacy_json,
            serde_json::json!({
                "api_key": "wai_plaintext_config_secret_value",
                "account_id": "acct_plaintext_secret"
            })
            .to_string(),
        )
        .unwrap();

        let _ = AppConfig::load(&paths).unwrap();

        let legacy_raw = fs::read_to_string(&legacy_json).unwrap();
        assert!(legacy_raw.contains("intentionally inert"));
        assert!(!legacy_raw.contains("wai_plaintext_config_secret_value"));
        assert!(!legacy_raw.contains("acct_plaintext_secret"));

        let _ = fs::remove_dir_all(paths.root_dir);
    }

    fn test_paths() -> AppPaths {
        let root_dir = std::env::temp_dir().join(format!("wirecli-config-test-{}", next_id()));
        let wire_dir = root_dir.join(".wirecli");
        let config_dir = wire_dir.join("config");
        let data_dir = wire_dir.join("data");
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
}
