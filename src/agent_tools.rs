#![allow(dead_code)]

use crate::memory::{AnchorInput, AnchorStore};
use crate::sandbox::{CommandResult, SandboxManager};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

pub const BOX_TOOL_NAMES: &[&str] = &[
    "shell",
    "apply_patch",
    "list_dir",
    "read_file",
    "write_file",
    "search",
    "remember",
    "recall",
];

#[derive(Debug, Clone)]
pub struct ToolResponse {
    pub text: String,
}

pub struct BoxTools<'a> {
    sandbox: &'a SandboxManager,
    anchors: &'a AnchorStore,
}

impl<'a> BoxTools<'a> {
    pub fn new(sandbox: &'a SandboxManager, anchors: &'a AnchorStore) -> Self {
        Self { sandbox, anchors }
    }

    pub fn list(&self) -> &'static [&'static str] {
        BOX_TOOL_NAMES
    }

    pub fn shell(&self, box_id: &str, command: &[String]) -> Result<ToolResponse, String> {
        let result = self.sandbox.run_capture(box_id, command)?;
        Ok(format_shell_result(result))
    }

    pub fn list_dir(&self, box_id: &str, relative_path: &str) -> Result<ToolResponse, String> {
        let path = self.workspace_path(box_id, relative_path)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name().to_string_lossy().to_string();
            let kind = if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                "dir"
            } else {
                "file"
            };
            entries.push(format!("{kind}\t{name}"));
        }
        entries.sort();
        Ok(ToolResponse {
            text: entries.join("\n"),
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
        let path = self.workspace_path(box_id, relative_path)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        fs::write(path, content).map_err(|e| e.to_string())?;
        Ok(ToolResponse {
            text: "written".to_string(),
        })
    }

    pub fn search(&self, box_id: &str, pattern: &str) -> Result<ToolResponse, String> {
        let workspace = self.workspace_root(box_id)?;
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
            .arg("!.rift")
            .arg("--glob")
            .arg("!.riftcode")
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

    pub fn apply_patch(&self, box_id: &str, patch_text: &str) -> Result<ToolResponse, String> {
        let workspace = self.workspace_root(box_id)?;
        apply_patch_text(&workspace, patch_text)?;
        Ok(ToolResponse {
            text: "patch applied".to_string(),
        })
    }

    pub fn remember(&self, project_key: &str, input: AnchorInput) -> Result<ToolResponse, String> {
        let record = self.anchors.remember(project_key, input)?;
        Ok(ToolResponse {
            text: format!("remembered {}", record.id),
        })
    }

    pub fn recall(&self, project_key: &str, query: &str) -> Result<ToolResponse, String> {
        let records = self.anchors.recall(project_key, query, 6)?;
        let mut lines = Vec::new();
        for record in records {
            lines.push(format!(
                "[{}] {} :: {}",
                record.kind, record.id, record.content
            ));
        }
        Ok(ToolResponse {
            text: lines.join("\n"),
        })
    }

    fn workspace_root(&self, box_id: &str) -> Result<PathBuf, String> {
        Ok(self.sandbox.workspace_path(box_id))
    }

    fn workspace_path(&self, box_id: &str, relative_path: &str) -> Result<PathBuf, String> {
        let workspace = self.workspace_root(box_id)?;
        resolve_within_workspace(&workspace, relative_path)
    }
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

fn resolve_within_workspace(workspace: &Path, relative_path: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(relative_path);
    if candidate.is_absolute() {
        return Err("absolute paths are blocked inside the box".to_string());
    }

    let mut resolved = workspace.to_path_buf();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() || !resolved.starts_with(workspace) {
                    return Err("path escapes the box workspace".to_string());
                }
            }
            _ => return Err("unsupported path component".to_string()),
        }
    }

    if !resolved.starts_with(workspace) {
        return Err("path escapes the box workspace".to_string());
    }
    Ok(resolved)
}

fn apply_patch_text(workspace: &Path, patch_text: &str) -> Result<(), String> {
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
            let target = resolve_within_workspace(workspace, path.trim())?;
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
            let target = resolve_within_workspace(workspace, path.trim())?;
            if target.exists() {
                fs::remove_file(target).map_err(|e| e.to_string())?;
            }
            continue;
        }

        if let Some(path) = trimmed.strip_prefix("*** Update File: ") {
            let target = resolve_within_workspace(workspace, path.trim())?;
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

#[cfg(test)]
mod tests {
    use super::{apply_patch_text, resolve_within_workspace};
    use std::fs;

    #[test]
    fn resolves_relative_paths() {
        let root = std::env::temp_dir().join("rift_tools_path_test");
        fs::create_dir_all(&root).unwrap();
        let resolved = resolve_within_workspace(&root, "src/lib.rs").unwrap();
        assert!(resolved.starts_with(&root));
    }

    #[test]
    fn applies_simple_add_patch() {
        let root = std::env::temp_dir().join("rift_tools_patch_test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let patch = r#"*** Begin Patch
*** Add File: hello.txt
+hello
*** End Patch
"#;
        apply_patch_text(&root, patch).unwrap();
        assert_eq!(
            fs::read_to_string(root.join("hello.txt")).unwrap(),
            "hello\n"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
