#![allow(dead_code)]

mod paths;

use crate::config::{AppConfig, PermissionMode};
use crate::hooks::{HookContext, HookStore};
use crate::memory::{AnchorInput, AnchorStore};
use crate::policy::CommandPolicy;
use crate::sandbox::{CommandResult, SandboxManager, SandboxRunOptions};
use paths::{display_rel, resolve_existing_project_path, resolve_patch_path};
use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

pub const BOX_TOOL_NAMES: &[&str] = &[
    "shell",
    "git",
    "gh",
    "update_plan",
    "plan",
    "subagent",
    "hook",
    "review",
    "apply_patch",
    "navigate",
    "list_dir",
    "read_file",
    "write_file",
    "read_lines",
    "grep_lines",
    "head_lines",
    "tail_lines",
    "glob_files",
    "replace_in_file",
    "delete_file",
    "copy_file",
    "move_file",
    "search",
    "remember",
    "recall",
    "lab_learn",
    "lab_recall",
    "session_remember",
    "session_recall",
    "mcp_list",
    "skill_list",
    "skill_read",
    "skill_create",
];

#[derive(Debug, Clone)]
pub struct ToolResponse {
    pub text: String,
}

pub struct BoxTools<'a> {
    sandbox: &'a SandboxManager,
    anchors: &'a AnchorStore,
    hooks: &'a HookStore,
    config: &'a AppConfig,
    permission_mode: PermissionMode,
    command_policy: CommandPolicy,
    current_dirs: Mutex<HashMap<String, PathBuf>>,
}

impl<'a> BoxTools<'a> {
    pub fn new(
        sandbox: &'a SandboxManager,
        anchors: &'a AnchorStore,
        hooks: &'a HookStore,
        config: &'a AppConfig,
        permission_mode: PermissionMode,
        command_policy: CommandPolicy,
    ) -> Self {
        Self {
            sandbox,
            anchors,
            hooks,
            config,
            permission_mode,
            command_policy,
            current_dirs: Mutex::new(HashMap::new()),
        }
    }

    pub fn list(&self) -> &'static [&'static str] {
        BOX_TOOL_NAMES
    }

    pub fn sandbox(&self) -> &SandboxManager {
        self.sandbox
    }

    pub fn shell(
        &self,
        box_id: &str,
        command: &[String],
        reason: Option<&str>,
    ) -> Result<ToolResponse, String> {
        if self.permission_mode == PermissionMode::FullAccess {
            let current_dir = self.current_path(box_id)?;
            return run_unrestricted(command, &current_dir).map(format_shell_result);
        }
        let approval =
            self.validate_command_for_box(box_id, command, reason.unwrap_or("shell command"))?;
        let result = self.sandbox.run_capture_approved(
            box_id,
            command,
            SandboxRunOptions {
                network_access: approval.network_access,
                ..SandboxRunOptions::default()
            },
        )?;
        Ok(format_shell_result(result))
    }

    pub fn git(
        &self,
        box_id: &str,
        command: &[String],
        reason: Option<&str>,
    ) -> Result<ToolResponse, String> {
        let mut args = vec!["git".to_string()];
        args.extend(command.iter().cloned());
        if self.permission_mode == PermissionMode::FullAccess {
            let current_dir = self.current_path(box_id)?;
            return run_unrestricted(&args, &current_dir).map(format_shell_result);
        }
        let approval =
            self.validate_command_for_box(box_id, &args, reason.unwrap_or("git command"))?;
        let result = self.sandbox.run_capture_approved(
            box_id,
            &args,
            SandboxRunOptions {
                network_access: approval.network_access,
                ..SandboxRunOptions::default()
            },
        )?;
        Ok(format_shell_result(result))
    }

    pub fn gh(
        &self,
        box_id: &str,
        command: &[String],
        reason: Option<&str>,
    ) -> Result<ToolResponse, String> {
        let mut args = vec!["gh".to_string()];
        args.extend(command.iter().cloned());
        if self.permission_mode == PermissionMode::FullAccess {
            let current_dir = self.current_path(box_id)?;
            return run_unrestricted(&args, &current_dir).map(format_shell_result);
        }
        let approval =
            self.validate_command_for_box(box_id, &args, reason.unwrap_or("github cli command"))?;
        let result = self.sandbox.run_capture_approved(
            box_id,
            &args,
            SandboxRunOptions {
                network_access: approval.network_access,
                ..SandboxRunOptions::default()
            },
        )?;
        Ok(format_shell_result(result))
    }

    pub fn update_plan(&self, explanation: &str, items: &[(String, String)]) -> ToolResponse {
        let mut text = String::from("## Plan\n");
        if !explanation.trim().is_empty() {
            text.push('\n');
            text.push_str(explanation.trim());
            text.push_str("\n\n");
        }
        for (step, status) in items {
            text.push_str("- [");
            text.push_str(status);
            text.push_str("] ");
            text.push_str(step);
            text.push('\n');
        }
        ToolResponse { text }
    }

    pub fn plan(&self, goal: &str, items: &[(String, String)]) -> ToolResponse {
        let mut text = String::from("## Plan\n");
        if !goal.trim().is_empty() {
            text.push_str("\n**Goal:** ");
            text.push_str(goal.trim());
            text.push_str("\n\n");
        }
        for (index, (step, status)) in items.iter().enumerate() {
            text.push_str(&(index + 1).to_string());
            text.push_str(". [");
            text.push_str(status);
            text.push_str("] ");
            text.push_str(step.trim());
            text.push('\n');
        }
        ToolResponse { text }
    }

    pub fn review(&self, questions: &[String]) -> ToolResponse {
        let mut text = String::from("Review questions\n");
        for (index, question) in questions.iter().take(10).enumerate() {
            text.push_str(&(index + 1).to_string());
            text.push_str(". ");
            text.push_str(question.trim());
            text.push('\n');
        }
        ToolResponse { text }
    }

    pub fn hook_summary(
        &self,
        action: &str,
        hook_id: Option<&str>,
        event: Option<&str>,
        command: Option<&[String]>,
        match_command: Option<&str>,
        match_mode: Option<&str>,
        match_tool: Option<&str>,
        match_status: Option<&str>,
        match_path: Option<&str>,
        hooks: &[crate::hooks::HookRecord],
    ) -> ToolResponse {
        let mut text = String::new();
        match action {
            "list" => {
                text.push_str("Hooks\n");
                if hooks.is_empty() {
                    text.push_str("No hooks configured.");
                } else {
                    for hook in hooks {
                        text.push_str("- ");
                        text.push_str(&hook.id);
                        text.push_str(" :: ");
                        text.push_str(&hook.event);
                        if let Some(expected) = hook.match_command.as_deref() {
                            text.push_str(" when `");
                            text.push_str(expected);
                            text.push_str("` [");
                            text.push_str(
                                hook.match_mode
                                    .as_deref()
                                    .filter(|value| !value.trim().is_empty())
                                    .unwrap_or("exact"),
                            );
                            text.push(']');
                        }
                        append_hook_filter(&mut text, "tool", hook.match_tool.as_deref());
                        append_hook_filter(&mut text, "status", hook.match_status.as_deref());
                        append_hook_filter(&mut text, "path", hook.match_path.as_deref());
                        text.push_str(" => ");
                        text.push_str(&hook.command.join(" "));
                        text.push('\n');
                    }
                }
            }
            "remove" => {
                text.push_str("Hook removed\n");
                if let Some(id) = hook_id {
                    text.push_str(id);
                }
            }
            _ => {
                text.push_str("Hook saved\n");
                if let Some(id) = hook_id {
                    text.push_str("id: ");
                    text.push_str(id);
                    text.push('\n');
                }
                if let Some(event) = event {
                    text.push_str("event: ");
                    text.push_str(event);
                    text.push('\n');
                }
                if let Some(expected) = match_command.filter(|value| !value.trim().is_empty()) {
                    text.push_str("match: ");
                    text.push_str(expected);
                    text.push('\n');
                    text.push_str("match_mode: ");
                    text.push_str(match_mode.unwrap_or("exact"));
                    text.push('\n');
                }
                append_hook_saved_filter(&mut text, "match_tool", match_tool);
                append_hook_saved_filter(&mut text, "match_status", match_status);
                append_hook_saved_filter(&mut text, "match_path", match_path);
                if let Some(command) = command {
                    text.push_str("run: ");
                    text.push_str(&command.join(" "));
                }
            }
        }
        ToolResponse { text }
    }

    pub fn navigate(&self, box_id: &str, relative_path: &str) -> Result<ToolResponse, String> {
        let path = self.workspace_path(box_id, relative_path)?;
        if !path.is_dir() {
            return Err(format!("{relative_path} is not a directory"));
        }
        self.current_dirs
            .lock()
            .map_err(|e| e.to_string())?
            .insert(box_id.to_string(), path.clone());
        let current = self.display_path(box_id, &path)?;
        Ok(ToolResponse {
            text: format!("## Navigated\n\nCurrent directory: `{current}`"),
        })
    }

    pub fn list_dir(&self, box_id: &str, relative_path: &str) -> Result<ToolResponse, String> {
        let path = self.workspace_path(box_id, relative_path)?;
        let root = self.workspace_root(box_id)?;
        let current = self.display_path(box_id, &path)?;
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in fs::read_dir(&path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                if is_wire_state_dir_name(&name) {
                    continue;
                }
                dirs.push(format!("{name}/"));
            } else {
                if should_skip_file_candidate(&root, &entry.path(), &name) {
                    continue;
                }
                files.push(name);
            }
        }
        dirs.sort();
        files.sort();
        if path.parent().is_some() && self.can_navigate_parent(box_id, &path)? {
            dirs.insert(0, "../".to_string());
        }
        Ok(ToolResponse {
            text: format_dir_listing(&current, &dirs, &files),
        })
    }

    pub fn read_file(&self, box_id: &str, relative_path: &str) -> Result<ToolResponse, String> {
        let path = self.workspace_path(box_id, relative_path)?;
        Ok(ToolResponse {
            text: fs::read_to_string(path).map_err(|e| e.to_string())?,
        })
    }

    pub fn write_file(
        &self,
        box_id: &str,
        relative_path: &str,
        content: &str,
    ) -> Result<ToolResponse, String> {
        let workspace = self.workspace_root(box_id)?;
        let current = self.current_path(box_id)?;
        let affected_paths = vec![relative_path.to_string()];
        let before = snapshot_files(&workspace, &current, self.permission_mode, &affected_paths)?;
        let path = self.workspace_path(box_id, relative_path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(path, content).map_err(|e| e.to_string())?;
        let after = snapshot_files(&workspace, &current, self.permission_mode, &affected_paths)?;
        let diff = render_patch_diff(&before, &after);
        let _ = self.hooks.run_event(self.sandbox, box_id, "after_edit");
        Ok(ToolResponse {
            text: if diff.trim().is_empty() {
                "written\n(no diff produced)".to_string()
            } else {
                diff
            },
        })
    }

    pub fn read_lines(
        &self,
        box_id: &str,
        relative_path: &str,
        start_line: usize,
        end_line: usize,
    ) -> Result<ToolResponse, String> {
        let path = self.workspace_path(box_id, relative_path)?;
        let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let start = start_line.max(1);
        let end = end_line.max(start);
        let mut out = String::new();
        for (index, line) in raw.lines().enumerate() {
            let line_no = index + 1;
            if line_no < start || line_no > end {
                continue;
            }
            out.push_str(&format!("{line_no:>6} | {line}\n"));
        }
        Ok(ToolResponse {
            text: if out.is_empty() {
                format!(
                    "no lines in range {}..={} for `{}`",
                    start, end, relative_path
                )
            } else {
                out
            },
        })
    }

    pub fn grep_lines(
        &self,
        box_id: &str,
        relative_path: &str,
        pattern: &str,
        before: usize,
        after: usize,
        literal: bool,
        max_matches: usize,
    ) -> Result<ToolResponse, String> {
        let path = self.workspace_path(box_id, relative_path)?;
        let mut cmd = Command::new("rg");
        cmd.arg("-n")
            .arg("-H")
            .arg("--no-heading")
            .arg("--color")
            .arg("never")
            .arg("-B")
            .arg(before.to_string())
            .arg("-A")
            .arg(after.to_string())
            .arg("-m")
            .arg(max_matches.to_string());
        if literal {
            cmd.arg("--fixed-strings");
        }
        cmd.arg(pattern).arg(path);
        let output = cmd.output().map_err(|e| e.to_string())?;
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(ToolResponse {
            text: if output.stdout.is_empty() {
                format!("no matches for `{pattern}` in `{relative_path}`")
            } else {
                String::from_utf8_lossy(&output.stdout).to_string()
            },
        })
    }

    pub fn head_lines(
        &self,
        box_id: &str,
        relative_path: &str,
        count: usize,
    ) -> Result<ToolResponse, String> {
        self.read_lines(box_id, relative_path, 1, count)
    }

    pub fn tail_lines(
        &self,
        box_id: &str,
        relative_path: &str,
        count: usize,
    ) -> Result<ToolResponse, String> {
        let path = self.workspace_path(box_id, relative_path)?;
        let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let total = raw.lines().count();
        let start = total.saturating_sub(count).saturating_add(1);
        self.read_lines(box_id, relative_path, start, total.max(1))
    }

    pub fn glob_files(
        &self,
        box_id: &str,
        pattern: &str,
        max_items: usize,
    ) -> Result<ToolResponse, String> {
        let workspace = self.workspace_root(box_id)?;
        let output = Command::new("rg")
            .arg("--files")
            .arg("-g")
            .arg(pattern)
            .current_dir(&workspace)
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        let mut lines = String::new();
        for (idx, line) in String::from_utf8_lossy(&output.stdout)
            .lines()
            .take(max_items)
            .enumerate()
        {
            lines.push_str(&format!("{:>4} | {line}\n", idx + 1));
        }
        Ok(ToolResponse {
            text: if lines.is_empty() {
                format!("no files matched `{pattern}`")
            } else {
                lines
            },
        })
    }

    pub fn replace_in_file(
        &self,
        box_id: &str,
        relative_path: &str,
        find: &str,
        replace: &str,
        all: bool,
    ) -> Result<ToolResponse, String> {
        let path = self.workspace_path(box_id, relative_path)?;
        let original = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let (updated, replaced) = if all {
            let count = original.matches(find).count();
            (original.replace(find, replace), count)
        } else {
            let updated = original.replacen(find, replace, 1);
            let replaced = usize::from(updated != original);
            (updated, replaced)
        };
        if replaced == 0 {
            return Ok(ToolResponse {
                text: format!("no matches for `{find}` in `{relative_path}`"),
            });
        }
        fs::write(&path, updated).map_err(|e| e.to_string())?;
        let _ = self.hooks.run_event(self.sandbox, box_id, "after_edit");
        Ok(ToolResponse {
            text: format!("replaced {replaced} occurrence(s) in `{relative_path}`"),
        })
    }

    pub fn search(&self, box_id: &str, pattern: &str) -> Result<ToolResponse, String> {
        let workspace = if self.permission_mode == PermissionMode::FullAccess {
            self.current_path(box_id)?
        } else {
            self.workspace_root(box_id)?
        };
        let output = Command::new("rg")
            .arg("-n")
            .arg("-H")
            .arg("-C")
            .arg("2")
            .arg("--smart-case")
            .arg("--hidden")
            .arg("--glob")
            .arg("!target")
            .arg("--glob")
            .arg("!.git")
            .arg("--glob")
            .arg("!.wire")
            .arg("--glob")
            .arg("!.wirecli")
            .arg("--glob")
            .arg(format!("!{}", pre_wire_state_dir_name()))
            .arg(pattern)
            .arg(workspace)
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() && output.status.code() != Some(1) {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(ToolResponse {
            text: String::from_utf8_lossy(&output.stdout).to_string(),
        })
    }

    pub fn delete_file(&self, box_id: &str, relative_path: &str) -> Result<ToolResponse, String> {
        let path = self.workspace_path(box_id, relative_path)?;
        if path.is_dir() {
            fs::remove_dir_all(&path).map_err(|e| e.to_string())?;
        } else {
            fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        let _ = self.hooks.run_event(self.sandbox, box_id, "after_edit");
        Ok(ToolResponse {
            text: format!("deleted `{relative_path}`"),
        })
    }

    pub fn copy_file(
        &self,
        box_id: &str,
        source_path: &str,
        destination_path: &str,
    ) -> Result<ToolResponse, String> {
        let source = self.workspace_path(box_id, source_path)?;
        let destination = self.workspace_path(box_id, destination_path)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::copy(&source, &destination).map_err(|e| e.to_string())?;
        let _ = self.hooks.run_event(self.sandbox, box_id, "after_edit");
        Ok(ToolResponse {
            text: format!("copied `{source_path}` -> `{destination_path}`"),
        })
    }

    pub fn move_file(
        &self,
        box_id: &str,
        source_path: &str,
        destination_path: &str,
    ) -> Result<ToolResponse, String> {
        let source = self.workspace_path(box_id, source_path)?;
        let destination = self.workspace_path(box_id, destination_path)?;
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::rename(&source, &destination).map_err(|e| e.to_string())?;
        let _ = self.hooks.run_event(self.sandbox, box_id, "after_edit");
        Ok(ToolResponse {
            text: format!("moved `{source_path}` -> `{destination_path}`"),
        })
    }

    pub fn apply_patch(&self, box_id: &str, patch_text: &str) -> Result<ToolResponse, String> {
        let workspace = self.workspace_root(box_id)?;
        let current = self.current_path(box_id)?;
        let affected_paths = patch_paths(patch_text);
        let before = snapshot_files(&workspace, &current, self.permission_mode, &affected_paths)?;
        apply_patch_text(&workspace, &current, self.permission_mode, patch_text)?;
        let after = snapshot_files(&workspace, &current, self.permission_mode, &affected_paths)?;
        let diff = render_patch_diff(&before, &after);
        let _ = self.hooks.run_event(self.sandbox, box_id, "after_edit");
        Ok(ToolResponse {
            text: if diff.trim().is_empty() {
                "patch applied\n(no diff produced)".to_string()
            } else {
                diff
            },
        })
    }

    pub fn remember(&self, project_key: &str, input: AnchorInput) -> Result<ToolResponse, String> {
        let _record = self.anchors.remember(project_key, input)?;
        Ok(ToolResponse {
            text: "Stored in Anchor memory for this project.".to_string(),
        })
    }

    pub fn recall(&self, project_key: &str, query: &str) -> Result<ToolResponse, String> {
        let records = self.anchors.recall(project_key, query, 6)?;
        let mut lines = Vec::new();
        for record in records {
            lines.push(format!("- [{}] {}", record.kind, record.content));
        }
        Ok(ToolResponse {
            text: if lines.is_empty() {
                "No matching Anchor memory found.".to_string()
            } else {
                lines.join("\n")
            },
        })
    }

    fn workspace_root(&self, box_id: &str) -> Result<PathBuf, String> {
        Ok(self.sandbox.workspace_path(box_id))
    }

    fn current_path(&self, box_id: &str) -> Result<PathBuf, String> {
        self.current_dirs
            .lock()
            .map_err(|e| e.to_string())?
            .get(box_id)
            .cloned()
            .map(Ok)
            .unwrap_or_else(|| self.workspace_root(box_id))
    }

    fn workspace_path(&self, box_id: &str, relative_path: &str) -> Result<PathBuf, String> {
        let workspace = self.workspace_root(box_id)?;
        let current = self.current_path(box_id)?;
        resolve_existing_project_path(&workspace, &current, relative_path, self.permission_mode)
    }

    fn validate_command_for_box(
        &self,
        box_id: &str,
        command: &[String],
        reason: &str,
    ) -> Result<crate::approvals::ApprovalResolution, String> {
        let workspace = self.workspace_root(box_id)?;
        let assessment = self
            .command_policy
            .assess_for_workspace(command, &workspace);
        let approval = match self.sandbox.approve_command(command, reason, &assessment) {
            Ok(approval) => approval,
            Err(err) => {
                if err.contains("approval required") || err.contains("approval denied") {
                    let _ = self.hooks.run_event_with_context(
                        self.sandbox,
                        box_id,
                        "permission_request",
                        &HookContext::default()
                            .command(command)
                            .status(if err.contains("approval required") {
                                "pending"
                            } else {
                                "denied"
                            })
                            .reason(reason),
                    );
                }
                return Err(err);
            }
        };
        if self.permission_mode == PermissionMode::Guardian {
            let current = self.current_path(box_id)?;
            let context = format!(
                "permission=guardian current_dir={} box_id={}",
                current.display(),
                box_id
            );
            let decision = crate::guardian::review_command(
                self.config,
                command,
                &workspace,
                reason,
                &context,
            )?;
            if !decision.allow {
                return Err(format!(
                    "Guardian denied command (risk={}): {}",
                    decision.risk, decision.reason
                ));
            }
        }
        Ok(approval)
    }

    fn display_path(&self, box_id: &str, path: &Path) -> Result<String, String> {
        let workspace = self.workspace_root(box_id)?;
        Ok(display_rel(&workspace, path))
    }

    fn can_navigate_parent(&self, box_id: &str, path: &Path) -> Result<bool, String> {
        if self.permission_mode == PermissionMode::FullAccess {
            return Ok(path.parent().is_some());
        }
        let workspace = self.workspace_root(box_id)?;
        Ok(path != workspace)
    }
}

fn append_hook_filter(text: &mut String, label: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    text.push(' ');
    text.push_str(label);
    text.push('=');
    text.push_str(value);
}

fn append_hook_saved_filter(text: &mut String, label: &str, value: Option<&str>) {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    text.push_str(label);
    text.push_str(": ");
    text.push_str(value);
    text.push('\n');
}

fn format_dir_listing(current: &str, dirs: &[String], files: &[String]) -> String {
    let mut text = String::new();
    text.push_str("Current directory: ");
    text.push_str(current);
    text.push('\n');
    text.push_str("Folders");
    if dirs.is_empty() {
        text.push_str("\n- none\n");
    } else {
        for dir in dirs.iter().take(80) {
            text.push_str("\n- ");
            text.push_str(dir);
        }
        if dirs.len() > 80 {
            text.push_str("\n- ... ");
            text.push_str(&(dirs.len() - 80).to_string());
            text.push_str(" more folders");
        }
        text.push('\n');
    }
    text.push_str("\nFiles");
    if files.is_empty() {
        text.push_str("\n- none");
    } else {
        for file in files.iter().take(120) {
            text.push_str("\n- ");
            text.push_str(file);
        }
        if files.len() > 120 {
            text.push_str("\n- ... ");
            text.push_str(&(files.len() - 120).to_string());
            text.push_str(" more files");
        }
    }
    text
}

fn format_shell_result(result: CommandResult) -> ToolResponse {
    let mut text = String::new();
    if !result.stdout.is_empty() {
        text.push_str(&String::from_utf8_lossy(&result.stdout));
    }
    if !result.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&String::from_utf8_lossy(&result.stderr));
    }
    ToolResponse { text }
}

fn run_unrestricted(command: &[String], cwd: &Path) -> Result<CommandResult, String> {
    if command.is_empty() {
        return Err("missing command".to_string());
    }
    let mut process = Command::new(&command[0]);
    process.current_dir(cwd).args(&command[1..]);
    run_command_with_timeout(&mut process)
}

fn run_command_with_timeout(process: &mut Command) -> Result<CommandResult, String> {
    let mut child = process
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "missing stdout pipe".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "missing stderr pipe".to_string())?;
    let stdout_handle = thread::spawn(move || read_all(stdout));
    let stderr_handle = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            let stdout = stdout_handle
                .join()
                .unwrap_or_else(|_| Vec::from("stdout reader panicked".as_bytes()));
            let stderr = stderr_handle
                .join()
                .unwrap_or_else(|_| Vec::from("stderr reader panicked".as_bytes()));
            return Ok(CommandResult {
                status_code: status.code(),
                stdout,
                stderr,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Err("tool watchdog timed out after 90s while waiting for command".to_string());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn read_all(mut reader: impl Read + Send + 'static) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = reader.read_to_end(&mut buffer);
    buffer
}

fn patch_paths(patch_text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in patch_text.lines() {
        let path = line
            .strip_prefix("*** Add File: ")
            .or_else(|| line.strip_prefix("*** Delete File: "))
            .or_else(|| line.strip_prefix("*** Update File: "));
        if let Some(path) = path {
            let path = path.trim().to_string();
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

fn snapshot_files(
    workspace: &Path,
    current_dir: &Path,
    permission_mode: PermissionMode,
    relative_paths: &[String],
) -> Result<Vec<(String, Option<String>)>, String> {
    let mut files = Vec::new();
    for relative_path in relative_paths {
        let path = resolve_patch_path(workspace, current_dir, permission_mode, relative_path)?;
        let contents = if path.exists() {
            Some(fs::read_to_string(path).map_err(|e| e.to_string())?)
        } else {
            None
        };
        files.push((relative_path.clone(), contents));
    }
    Ok(files)
}

fn render_patch_diff(
    before: &[(String, Option<String>)],
    after: &[(String, Option<String>)],
) -> String {
    let mut out = String::from("patch applied\n\n");
    for (path, before_text) in before {
        let after_text = after
            .iter()
            .find(|(after_path, _)| after_path == path)
            .and_then(|(_, text)| text.as_ref());
        out.push_str("diff -- ");
        out.push_str(path);
        out.push('\n');
        out.push_str("```diff\n");
        out.push_str(&unified_diff(
            path,
            before_text.as_deref().unwrap_or_default(),
            after_text.map(|s| s.as_str()).unwrap_or_default(),
            before_text.is_none(),
            after_text.is_none(),
        ));
        out.push_str("```\n\n");
    }
    out
}

fn unified_diff(path: &str, before: &str, after: &str, created: bool, deleted: bool) -> String {
    let before_lines: Vec<&str> = before.lines().collect();
    let after_lines: Vec<&str> = after.lines().collect();
    let mut out = String::new();
    out.push_str("--- ");
    out.push_str(if created { "/dev/null" } else { path });
    out.push('\n');
    out.push_str("+++ ");
    out.push_str(if deleted { "/dev/null" } else { path });
    out.push('\n');

    let mut i = 0usize;
    let mut j = 0usize;
    while i < before_lines.len() || j < after_lines.len() {
        if i < before_lines.len() && j < after_lines.len() && before_lines[i] == after_lines[j] {
            out.push(' ');
            out.push_str(before_lines[i]);
            out.push('\n');
            i += 1;
            j += 1;
            continue;
        }

        if j + 1 < after_lines.len()
            && i < before_lines.len()
            && before_lines[i] == after_lines[j + 1]
        {
            out.push('+');
            out.push_str(after_lines[j]);
            out.push('\n');
            j += 1;
            continue;
        }

        if i + 1 < before_lines.len()
            && j < after_lines.len()
            && before_lines[i + 1] == after_lines[j]
        {
            out.push('-');
            out.push_str(before_lines[i]);
            out.push('\n');
            i += 1;
            continue;
        }

        if i < before_lines.len() {
            out.push('-');
            out.push_str(before_lines[i]);
            out.push('\n');
            i += 1;
        }
        if j < after_lines.len() {
            out.push('+');
            out.push_str(after_lines[j]);
            out.push('\n');
            j += 1;
        }
    }

    out
}

fn apply_patch_text(
    workspace: &Path,
    current_dir: &Path,
    permission_mode: PermissionMode,
    patch_text: &str,
) -> Result<(), String> {
    let mut lines = patch_text.lines().peekable();
    if lines.next().map(|line| line.trim()) != Some("*** Begin Patch") {
        return Err("patch must start with *** Begin Patch".to_string());
    }

    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        if trimmed == "*** End Patch" {
            return Ok(());
        }

        if let Some(path) = trimmed.strip_prefix("*** Add File: ") {
            let target = resolve_patch_path(workspace, current_dir, permission_mode, path.trim())?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut content = String::new();
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") {
                    break;
                }
                let next = lines.next().unwrap();
                if let Some(stripped) = next.strip_prefix('+') {
                    content.push_str(stripped);
                    content.push('\n');
                }
            }
            fs::write(target, content).map_err(|e| e.to_string())?;
            continue;
        }

        if let Some(path) = trimmed.strip_prefix("*** Delete File: ") {
            let target = resolve_patch_path(workspace, current_dir, permission_mode, path.trim())?;
            if target.exists() {
                fs::remove_file(target).map_err(|e| e.to_string())?;
            }
            continue;
        }

        if let Some(path) = trimmed.strip_prefix("*** Update File: ") {
            let target = resolve_patch_path(workspace, current_dir, permission_mode, path.trim())?;
            let original = if target.exists() {
                fs::read_to_string(&target).map_err(|e| e.to_string())?
            } else {
                String::new()
            };
            let original_lines: Vec<String> =
                original.lines().map(|line| line.to_string()).collect();
            let mut output: Vec<String> = Vec::new();
            let mut cursor = 0usize;
            while let Some(next) = lines.peek() {
                if next.starts_with("*** ") {
                    break;
                }
                let next = lines.next().unwrap();
                if next.starts_with("@@") || next.trim() == "*** End of File" {
                    continue;
                }
                if let Some(rest) = next.strip_prefix(' ') {
                    let current = original_lines.get(cursor).ok_or_else(|| {
                        format!("update context mismatch in {}", target.display())
                    })?;
                    if current != rest {
                        return Err(format!("update context mismatch in {}", target.display()));
                    }
                    output.push(rest.to_string());
                    cursor += 1;
                } else if let Some(rest) = next.strip_prefix('-') {
                    let current = original_lines
                        .get(cursor)
                        .ok_or_else(|| format!("update delete mismatch in {}", target.display()))?;
                    if current != rest {
                        return Err(format!("update delete mismatch in {}", target.display()));
                    }
                    cursor += 1;
                } else if let Some(rest) = next.strip_prefix('+') {
                    output.push(rest.to_string());
                }
            }
            output.extend(original_lines.into_iter().skip(cursor));
            let mut text = output.join("\n");
            if !text.is_empty() {
                text.push('\n');
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            fs::write(target, text).map_err(|e| e.to_string())?;
            continue;
        }

        return Err(format!("unsupported patch line: {trimmed}"));
    }

    Ok(())
}

fn is_wire_state_dir_name(name: &str) -> bool {
    name == ".wire" || name == ".wirecli" || name == pre_wire_state_dir_name()
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

#[cfg(test)]
mod tests {
    use super::apply_patch_text;
    use crate::config::PermissionMode;
    use std::fs;

    #[test]
    fn applies_simple_add_patch() {
        let root = std::env::temp_dir().join("wire_tools_patch_test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let patch = r#"*** Begin Patch
*** Add File: hello.txt
+hello
*** End Patch
"#;
        apply_patch_text(&root, &root, PermissionMode::Normal, patch).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hello\n"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
