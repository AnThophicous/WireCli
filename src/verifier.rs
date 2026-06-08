use crate::safekey::redact_secrets;
use crate::sandbox::{SandboxManager, SandboxRunOptions};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

const MAX_OUTPUT_CHARS: usize = 2400;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    Passed,
    Failed,
    Blocked,
    Skipped,
}

impl VerificationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone)]
pub struct VerifierCommandReport {
    pub label: String,
    pub command: Vec<String>,
    pub status: VerificationStatus,
    pub exit_code: Option<i32>,
    pub output: String,
}

#[derive(Debug, Clone)]
pub struct VerifierReport {
    pub trigger: String,
    pub edited_paths: Vec<String>,
    pub status: VerificationStatus,
    pub commands: Vec<VerifierCommandReport>,
    pub skipped: Vec<String>,
    pub undo_hint: Option<String>,
}

impl VerifierReport {
    pub fn exit_code(&self) -> i64 {
        match self.status {
            VerificationStatus::Passed | VerificationStatus::Skipped => 0,
            VerificationStatus::Failed | VerificationStatus::Blocked => 1,
        }
    }

    pub fn to_model_output(&self) -> String {
        let mut text = String::from("Verifier Pipeline\n");
        text.push_str("status: ");
        text.push_str(self.status.as_str());
        text.push_str("\ntrigger: ");
        text.push_str(&self.trigger);
        if !self.edited_paths.is_empty() {
            text.push_str("\nedited paths:");
            for path in self.edited_paths.iter().take(40) {
                text.push_str("\n- ");
                text.push_str(path);
            }
            if self.edited_paths.len() > 40 {
                text.push_str("\n- ... ");
                text.push_str(&(self.edited_paths.len() - 40).to_string());
                text.push_str(" more");
            }
        }
        if !self.skipped.is_empty() {
            text.push_str("\nskipped:");
            for item in &self.skipped {
                text.push_str("\n- ");
                text.push_str(item);
            }
        }
        if let Some(undo_hint) = self.undo_hint.as_deref() {
            text.push_str("\nundo:");
            text.push_str("\n- ");
            text.push_str(undo_hint);
        }
        if self.commands.is_empty() {
            text.push_str("\ncommands: none");
            return text;
        }
        text.push_str("\ncommands:");
        for report in &self.commands {
            text.push_str("\n- ");
            text.push_str(report.status.as_str());
            text.push_str(": ");
            text.push_str(&report.command.join(" "));
            if let Some(code) = report.exit_code {
                text.push_str(" (exit ");
                text.push_str(&code.to_string());
                text.push(')');
            }
            if !report.output.trim().is_empty() {
                text.push_str("\n  ```text\n");
                for line in report.output.trim().lines() {
                    text.push_str("  ");
                    text.push_str(line);
                    text.push('\n');
                }
                text.push_str("  ```");
            }
        }
        text
    }
}

#[derive(Debug, Clone)]
struct VerifierCommand {
    label: String,
    command: Vec<String>,
}

impl VerifierCommand {
    fn new(label: &str, command: &[&str]) -> Self {
        Self {
            label: label.to_string(),
            command: command.iter().map(|part| part.to_string()).collect(),
        }
    }
}

pub struct VerifierPipeline<'a> {
    sandbox: &'a SandboxManager,
    box_id: &'a str,
}

impl<'a> VerifierPipeline<'a> {
    pub fn new(sandbox: &'a SandboxManager, box_id: &'a str) -> Self {
        Self { sandbox, box_id }
    }

    pub fn run_after_edit(&self, trigger: &str, edited_paths: &[String]) -> VerifierReport {
        let workspace = self.sandbox.workspace_path(self.box_id);
        let (commands, mut skipped) = infer_commands(&workspace, edited_paths);
        let mut reports = Vec::new();

        for command in commands {
            let result = self.sandbox.run_capture_approved(
                self.box_id,
                &command.command,
                SandboxRunOptions {
                    network_access: false,
                    ..SandboxRunOptions::default()
                },
            );

            match result {
                Ok(result) => {
                    let mut output = String::new();
                    if !result.stdout.is_empty() {
                        output.push_str(&String::from_utf8_lossy(&result.stdout));
                    }
                    if !result.stderr.is_empty() {
                        if !output.is_empty() {
                            output.push('\n');
                        }
                        output.push_str(&String::from_utf8_lossy(&result.stderr));
                    }
                    let status = match result.status_code {
                        Some(0) => VerificationStatus::Passed,
                        _ => VerificationStatus::Failed,
                    };
                    reports.push(VerifierCommandReport {
                        label: command.label,
                        command: command.command,
                        status,
                        exit_code: result.status_code,
                        output: clip_output(&output),
                    });
                }
                Err(err) => {
                    let unavailable_sandbox = err.contains("OS sandbox is required")
                        || err.contains("bubblewrap namespace setup failed");
                    reports.push(VerifierCommandReport {
                        label: command.label,
                        command: command.command,
                        status: VerificationStatus::Blocked,
                        exit_code: None,
                        output: clip_output(&err),
                    });
                    if unavailable_sandbox {
                        skipped.push(
                            "stopped remaining validators because OS sandbox execution is unavailable"
                                .to_string(),
                        );
                        break;
                    }
                }
            }
        }

        let status = overall_status(&reports);
        VerifierReport {
            trigger: trigger.to_string(),
            edited_paths: sanitize_paths(edited_paths),
            status,
            commands: reports,
            skipped,
            undo_hint: undo_hint(&workspace, edited_paths),
        }
    }
}

fn infer_commands(
    workspace: &Path,
    edited_paths: &[String],
) -> (Vec<VerifierCommand>, Vec<String>) {
    let mut commands = Vec::new();
    let mut skipped = Vec::new();

    if workspace.join(".git").exists() {
        commands.push(VerifierCommand::new(
            "git diff whitespace check",
            &["git", "diff", "--check"],
        ));
    } else {
        skipped.push("git diff --check: workspace is not a git repository".to_string());
    }

    if workspace.join("Cargo.toml").exists() {
        commands.push(VerifierCommand::new(
            "rust format check",
            &["cargo", "fmt", "--check"],
        ));
        commands.push(VerifierCommand::new("rust tests", &["cargo", "test"]));
    }

    if workspace.join("package.json").exists() {
        match package_scripts(workspace) {
            Ok(scripts) => {
                let package_manager = package_manager(workspace);
                for script in ["lint", "test", "build"] {
                    if scripts.contains_key(script) {
                        commands.push(package_script_command(package_manager, script));
                    }
                }
                if !["lint", "test", "build"]
                    .iter()
                    .any(|script| scripts.contains_key(*script))
                {
                    skipped.push(
                        "package.json: no lint, test, or build script was declared".to_string(),
                    );
                }
            }
            Err(err) => skipped.push(format!("package.json: {err}")),
        }
    }

    if workspace.join("go.mod").exists() {
        commands.push(VerifierCommand::new("go tests", &["go", "test", "./..."]));
    }

    if touches_extension(edited_paths, "py")
        && (workspace.join("pyproject.toml").exists()
            || workspace.join("pytest.ini").exists()
            || workspace.join("setup.py").exists())
    {
        commands.push(VerifierCommand::new(
            "python tests",
            &["python3", "-m", "pytest"],
        ));
    }

    if commands.is_empty() {
        skipped.push("no deterministic validators matched the edited files".to_string());
    }

    (commands, skipped)
}

fn package_scripts(workspace: &Path) -> Result<BTreeMap<String, String>, String> {
    let raw = fs::read_to_string(workspace.join("package.json")).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let scripts = value
        .get("scripts")
        .and_then(|value| value.as_object())
        .ok_or_else(|| "no scripts object was declared".to_string())?;
    Ok(scripts
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_str()
                .map(|script| (key.clone(), script.to_string()))
        })
        .collect())
}

fn package_manager(workspace: &Path) -> &'static str {
    if workspace.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if workspace.join("yarn.lock").exists() {
        "yarn"
    } else if workspace.join("bun.lock").exists() || workspace.join("bun.lockb").exists() {
        "bun"
    } else {
        "npm"
    }
}

fn package_script_command(package_manager: &str, script: &str) -> VerifierCommand {
    let label = format!("{package_manager} {script}");
    if package_manager == "yarn" {
        VerifierCommand {
            label,
            command: vec!["yarn".to_string(), script.to_string()],
        }
    } else {
        VerifierCommand {
            label,
            command: vec![
                package_manager.to_string(),
                "run".to_string(),
                script.to_string(),
            ],
        }
    }
}

fn touches_extension(paths: &[String], extension: &str) -> bool {
    paths.iter().any(|path| {
        Path::new(path)
            .extension()
            .and_then(|value| value.to_str())
            .map(|value| value.eq_ignore_ascii_case(extension))
            .unwrap_or(false)
    })
}

fn overall_status(reports: &[VerifierCommandReport]) -> VerificationStatus {
    if reports.is_empty() {
        return VerificationStatus::Skipped;
    }
    if reports
        .iter()
        .any(|report| report.status == VerificationStatus::Failed)
    {
        return VerificationStatus::Failed;
    }
    if reports
        .iter()
        .any(|report| report.status == VerificationStatus::Blocked)
    {
        return VerificationStatus::Blocked;
    }
    VerificationStatus::Passed
}

fn sanitize_paths(paths: &[String]) -> Vec<String> {
    let mut out = paths
        .iter()
        .map(|path| path.trim())
        .filter(|path| !path.is_empty())
        .map(redact_secrets)
        .collect::<Vec<_>>();
    out.sort();
    out.dedup();
    out
}

fn undo_hint(workspace: &Path, edited_paths: &[String]) -> Option<String> {
    if edited_paths.is_empty() {
        return None;
    }
    let paths = sanitize_paths(edited_paths);
    if workspace.join(".git").exists() {
        let joined = paths
            .iter()
            .take(8)
            .map(|path| shell_quote(path))
            .collect::<Vec<_>>()
            .join(" ");
        let suffix = if paths.len() > 8 {
            " # plus remaining edited paths"
        } else {
            ""
        };
        return Some(format!(
            "Inspect with `git diff -- {joined}`; undo tracked edits with `git restore -- {joined}`.{suffix}"
        ));
    }
    Some(
        "No git repository was detected; use the persisted tool diff/timeline to reverse the listed edited paths."
            .to_string(),
    )
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | '+'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn clip_output(output: &str) -> String {
    let output = redact_secrets(output.trim());
    if output.chars().count() <= MAX_OUTPUT_CHARS {
        return output;
    }

    let mut clipped = output
        .chars()
        .take(MAX_OUTPUT_CHARS.saturating_sub(80))
        .collect::<String>();
    clipped.push_str("\n... output truncated by Wire verifier ...");
    clipped
}

#[cfg(test)]
mod tests {
    use super::{infer_commands, overall_status, package_manager, VerifierCommandReport};
    use super::{VerificationStatus, VerifierCommand};
    use std::fs;

    #[test]
    fn infers_rust_and_git_validators() {
        let root = test_workspace("rust_git");
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='demo'\nversion='0.1.0'\n",
        )
        .unwrap();

        let (commands, skipped) = infer_commands(&root, &["src/lib.rs".to_string()]);
        let rendered = commands
            .iter()
            .map(|command| command.command.join(" "))
            .collect::<Vec<_>>();

        assert!(rendered.contains(&"git diff --check".to_string()));
        assert!(rendered.contains(&"cargo fmt --check".to_string()));
        assert!(rendered.contains(&"cargo test".to_string()));
        assert!(skipped.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn infers_package_scripts_with_lockfile_manager() {
        let root = test_workspace("package_scripts");
        fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"scripts":{"lint":"eslint .","test":"vitest","build":"vite build"}}"#,
        )
        .unwrap();

        let (commands, _skipped) = infer_commands(&root, &["src/app.ts".to_string()]);
        let rendered = commands
            .iter()
            .map(|command| command.command.join(" "))
            .collect::<Vec<_>>();

        assert_eq!(package_manager(&root), "pnpm");
        assert!(rendered.contains(&"pnpm run lint".to_string()));
        assert!(rendered.contains(&"pnpm run test".to_string()));
        assert!(rendered.contains(&"pnpm run build".to_string()));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn failed_status_takes_precedence_over_blocked() {
        let reports = vec![
            report("blocked", VerificationStatus::Blocked),
            report("failed", VerificationStatus::Failed),
        ];
        assert_eq!(overall_status(&reports), VerificationStatus::Failed);
    }

    fn report(label: &str, status: VerificationStatus) -> VerifierCommandReport {
        VerifierCommandReport {
            label: label.to_string(),
            command: VerifierCommand::new(label, &[label]).command,
            status,
            exit_code: None,
            output: String::new(),
        }
    }

    fn test_workspace(name: &str) -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("wirecli-verifier-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }
}
