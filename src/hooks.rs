use crate::config::AppPaths;
use crate::safekey::redact_secrets;
use crate::sandbox::SandboxManager;
use serde::{Deserialize, Serialize};
use std::fs;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct HookFile {
    #[serde(default = "default_version")]
    v: u32,
    #[serde(default)]
    hooks: Vec<HookEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HookEntry {
    id: String,
    event: String,
    command: Vec<String>,
    #[serde(default)]
    match_command: Option<String>,
    #[serde(default)]
    match_mode: Option<String>,
    #[serde(default)]
    match_tool: Option<String>,
    #[serde(default)]
    match_status: Option<String>,
    #[serde(default)]
    match_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HookRecord {
    pub id: String,
    pub event: String,
    pub command: Vec<String>,
    pub match_command: Option<String>,
    pub match_mode: Option<String>,
    pub match_tool: Option<String>,
    pub match_status: Option<String>,
    pub match_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HookExecution {
    pub id: String,
    pub event: String,
    pub command: Vec<String>,
    pub output: String,
    pub status: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub session_id: Option<String>,
    pub tool_name: Option<String>,
    pub command: Option<Vec<String>>,
    pub paths: Vec<String>,
    pub status: Option<String>,
    pub reason: Option<String>,
    pub payload: Option<String>,
}

impl HookContext {
    pub fn session(mut self, session_id: &str) -> Self {
        self.session_id = Some(session_id.to_string());
        self
    }

    pub fn tool(mut self, tool_name: &str) -> Self {
        self.tool_name = Some(tool_name.to_string());
        self
    }

    pub fn command(mut self, command: &[String]) -> Self {
        self.command = Some(command.to_vec());
        self
    }

    pub fn paths(mut self, paths: &[String]) -> Self {
        self.paths = paths.to_vec();
        self
    }

    pub fn status(mut self, status: &str) -> Self {
        self.status = Some(status.to_string());
        self
    }

    pub fn reason(mut self, reason: &str) -> Self {
        self.reason = Some(reason.to_string());
        self
    }

    pub fn payload(mut self, payload: &str) -> Self {
        self.payload = Some(payload.to_string());
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HookMatchMode {
    Exact,
    StartsWith,
    Contains,
}

impl HookMatchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::StartsWith => "starts_with",
            Self::Contains => "contains",
        }
    }

    pub fn from_value(value: Option<&str>) -> Self {
        match value
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "starts_with" | "starts-with" | "prefix" => Self::StartsWith,
            "contains" | "substring" => Self::Contains,
            _ => Self::Exact,
        }
    }
}

pub struct HookStore {
    path: std::path::PathBuf,
}

impl HookStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        if let Some(parent) = paths.hooks_file.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if !paths.hooks_file.exists() {
            fs::write(&paths.hooks_file, "{\n  \"v\": 1,\n  \"hooks\": []\n}\n")
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            path: paths.hooks_file.clone(),
        })
    }

    pub fn list(&self) -> Result<Vec<HookRecord>, String> {
        let file = self.load()?;
        Ok(file
            .hooks
            .into_iter()
            .map(|hook| HookRecord {
                id: hook.id,
                event: hook.event,
                command: hook.command,
                match_command: hook.match_command,
                match_mode: hook.match_mode,
                match_tool: hook.match_tool,
                match_status: hook.match_status,
                match_path: hook.match_path,
            })
            .collect())
    }

    pub fn add(&self, event: &str, command: Vec<String>) -> Result<HookRecord, String> {
        self.add_with_match(event, command, None, HookMatchMode::Exact)
    }

    pub fn add_with_match(
        &self,
        event: &str,
        command: Vec<String>,
        match_command: Option<String>,
        match_mode: HookMatchMode,
    ) -> Result<HookRecord, String> {
        self.add_scoped(event, command, match_command, match_mode, None, None, None)
    }

    pub fn add_scoped(
        &self,
        event: &str,
        command: Vec<String>,
        match_command: Option<String>,
        match_mode: HookMatchMode,
        match_tool: Option<String>,
        match_status: Option<String>,
        match_path: Option<String>,
    ) -> Result<HookRecord, String> {
        if command.is_empty() {
            return Err("hook command cannot be empty".to_string());
        }
        let mut file = self.load()?;
        let record = HookEntry {
            id: crate::id::next_id(),
            event: normalize_event(event),
            command,
            match_command: match_command
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            match_mode: Some(match_mode.as_str().to_string()),
            match_tool: normalize_filter(match_tool),
            match_status: normalize_filter(match_status),
            match_path: normalize_filter(match_path),
        };
        file.hooks.push(record.clone());
        self.save(&file)?;
        Ok(HookRecord {
            id: record.id,
            event: record.event,
            command: record.command,
            match_command: record.match_command,
            match_mode: record.match_mode,
            match_tool: record.match_tool,
            match_status: record.match_status,
            match_path: record.match_path,
        })
    }

    pub fn remove(&self, id: &str) -> Result<bool, String> {
        let mut file = self.load()?;
        let before = file.hooks.len();
        file.hooks.retain(|hook| hook.id != id);
        let removed = before != file.hooks.len();
        if removed {
            self.save(&file)?;
        }
        Ok(removed)
    }

    pub fn run_event(
        &self,
        sandbox: &SandboxManager,
        box_id: &str,
        event: &str,
    ) -> Result<Vec<HookExecution>, String> {
        self.run_event_with_context(sandbox, box_id, event, &HookContext::default())
    }

    pub fn run_event_with_command(
        &self,
        sandbox: &SandboxManager,
        box_id: &str,
        event: &str,
        executed_command: Option<&[String]>,
    ) -> Result<Vec<HookExecution>, String> {
        let context = executed_command
            .map(|command| HookContext::default().command(command))
            .unwrap_or_default();
        self.run_event_with_context(sandbox, box_id, event, &context)
    }

    pub fn run_event_with_context(
        &self,
        sandbox: &SandboxManager,
        box_id: &str,
        event: &str,
        context: &HookContext,
    ) -> Result<Vec<HookExecution>, String> {
        let file = self.load()?;
        let mut executions = Vec::new();
        let executed_joined = context.command.as_ref().map(|parts| parts.join(" "));
        let event = normalize_event(event);
        for hook in file.hooks.into_iter().filter(|hook| hook.event == event) {
            if !hook_matches_context(&hook, context, executed_joined.as_deref()) {
                continue;
            }
            let result = sandbox.run_capture(box_id, &hook.command);
            let mut output = String::new();
            let mut status = "ok".to_string();
            let mut exit_code = None;
            match result {
                Ok(result) => {
                    exit_code = result.status_code;
                    if result.status_code.unwrap_or(1) != 0 {
                        status = "failed".to_string();
                    }
                    if !result.stdout.is_empty() {
                        output.push_str(&String::from_utf8_lossy(&result.stdout));
                    }
                    if !result.stderr.is_empty() {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(&String::from_utf8_lossy(&result.stderr));
                    }
                }
                Err(err) => {
                    status = "blocked".to_string();
                    output.push_str("Hook execution failed without stopping the agent.\n");
                    output.push_str(&err);
                }
            }
            executions.push(HookExecution {
                id: hook.id,
                event: hook.event,
                command: hook.command,
                output: redact_secrets(&output),
                status,
                exit_code,
            });
        }
        Ok(executions)
    }

    fn load(&self) -> Result<HookFile, String> {
        let raw = fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let mut file: HookFile = serde_json::from_str(&raw).unwrap_or_default();
        if file.v == 0 {
            file.v = 1;
        }
        Ok(file)
    }

    fn save(&self, file: &HookFile) -> Result<(), String> {
        let raw = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
        fs::write(&self.path, raw).map_err(|e| e.to_string())
    }
}

fn normalize_event(event: &str) -> String {
    let mut out = String::new();
    let mut previous_was_sep = false;
    let mut previous_was_lower_or_digit = false;
    for ch in event.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && previous_was_lower_or_digit && !previous_was_sep {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            previous_was_sep = false;
            previous_was_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !out.is_empty() && !previous_was_sep {
                out.push('_');
            }
            previous_was_sep = true;
            previous_was_lower_or_digit = false;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

fn normalize_filter(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn default_version() -> u32 {
    1
}

fn hook_matches_command(hook: &HookEntry, executed_command: Option<&str>) -> bool {
    let Some(expected) = hook
        .match_command
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return true;
    };

    let Some(executed) = executed_command
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };

    match HookMatchMode::from_value(hook.match_mode.as_deref()) {
        HookMatchMode::Exact => executed == expected,
        HookMatchMode::StartsWith => executed.starts_with(expected),
        HookMatchMode::Contains => executed.contains(expected),
    }
}

fn hook_matches_context(
    hook: &HookEntry,
    context: &HookContext,
    executed_command: Option<&str>,
) -> bool {
    if !hook_matches_command(hook, executed_command) {
        return false;
    }
    if !matches_optional_filter(
        hook.match_tool.as_deref(),
        context.tool_name.as_deref(),
        HookMatchMode::Exact,
    ) {
        return false;
    }
    if !matches_optional_filter(
        hook.match_status.as_deref(),
        context.status.as_deref(),
        HookMatchMode::Exact,
    ) {
        return false;
    }
    if let Some(expected) = hook.match_path.as_deref() {
        let mode = HookMatchMode::from_value(hook.match_mode.as_deref());
        if !context
            .paths
            .iter()
            .any(|path| matches_filter(expected, path, mode))
        {
            return false;
        }
    }
    true
}

fn matches_optional_filter(
    expected: Option<&str>,
    actual: Option<&str>,
    mode: HookMatchMode,
) -> bool {
    let Some(expected) = expected.map(str::trim).filter(|value| !value.is_empty()) else {
        return true;
    };
    let Some(actual) = actual.map(str::trim).filter(|value| !value.is_empty()) else {
        return false;
    };
    matches_filter(expected, actual, mode)
}

fn matches_filter(expected: &str, actual: &str, mode: HookMatchMode) -> bool {
    match mode {
        HookMatchMode::Exact => actual == expected,
        HookMatchMode::StartsWith => actual.starts_with(expected),
        HookMatchMode::Contains => actual.contains(expected),
    }
}

pub fn canonical_event_name(event: &str) -> String {
    normalize_event(event)
}

#[cfg(test)]
mod tests {
    use super::{canonical_event_name, hook_matches_context, HookContext, HookEntry};

    #[test]
    fn normalizes_lifecycle_event_names() {
        assert_eq!(canonical_event_name("SessionStart"), "session_start");
        assert_eq!(canonical_event_name("pre-tool-use"), "pre_tool_use");
        assert_eq!(canonical_event_name("after_edit"), "after_edit");
    }

    #[test]
    fn matches_tool_status_and_path_context() {
        let hook = HookEntry {
            id: "h1".to_string(),
            event: "post_tool_use".to_string(),
            command: vec!["echo".to_string(), "ok".to_string()],
            match_command: None,
            match_mode: Some("contains".to_string()),
            match_tool: Some("apply_patch".to_string()),
            match_status: Some("ok".to_string()),
            match_path: Some("src/".to_string()),
        };
        let context = HookContext::default()
            .tool("apply_patch")
            .status("ok")
            .paths(&["src/main.rs".to_string()]);

        assert!(hook_matches_context(&hook, &context, None));
    }
}
