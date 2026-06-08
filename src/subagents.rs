use crate::safekey::redact_secrets;
use crate::sandbox::{SandboxManager, SandboxRunOptions};
use crate::verifier::{VerificationStatus, VerifierPipeline};

const MAX_OUTPUT_CHARS: usize = 2200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentRole {
    Planner,
    CodebaseResearcher,
    Patcher,
    Reviewer,
    TestRunner,
    SecurityAuditor,
}

impl SubagentRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planner => "planner",
            Self::CodebaseResearcher => "codebase_researcher",
            Self::Patcher => "patcher",
            Self::Reviewer => "reviewer",
            Self::TestRunner => "test_runner",
            Self::SecurityAuditor => "security_auditor",
        }
    }

    pub fn allowed_tools(self) -> &'static [&'static str] {
        match self {
            Self::Planner => &["none"],
            Self::CodebaseResearcher => &["rg", "rg --files"],
            Self::Patcher => &["rg", "rg --files"],
            Self::Reviewer => &["git status", "git diff"],
            Self::TestRunner => &["verifier.pipeline"],
            Self::SecurityAuditor => &["rg"],
        }
    }

    pub fn from_value(value: &str) -> Option<Self> {
        match normalize_role(value).as_str() {
            "planner" => Some(Self::Planner),
            "codebase_researcher" | "researcher" | "codebase" => Some(Self::CodebaseResearcher),
            "patcher" | "patch_planner" => Some(Self::Patcher),
            "reviewer" | "code_reviewer" => Some(Self::Reviewer),
            "test_runner" | "tester" | "validator" => Some(Self::TestRunner),
            "security_auditor" | "security" | "auditor" => Some(Self::SecurityAuditor),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubagentCommandReport {
    pub label: String,
    pub command: Vec<String>,
    pub status: VerificationStatus,
    pub exit_code: Option<i32>,
    pub output: String,
}

#[derive(Debug, Clone)]
pub struct SubagentReport {
    pub role: SubagentRole,
    pub task: String,
    pub status: VerificationStatus,
    pub notes: Vec<String>,
    pub commands: Vec<SubagentCommandReport>,
}

impl SubagentReport {
    pub fn to_model_output(&self) -> String {
        let mut out = String::from("Subagent Report\n");
        out.push_str("role: ");
        out.push_str(self.role.as_str());
        out.push_str("\nstatus: ");
        out.push_str(self.status.as_str());
        out.push_str("\npermissions: scoped analysis only; no network; no file edits");
        out.push_str("\nallowed tools:");
        for tool in self.role.allowed_tools() {
            out.push_str("\n- ");
            out.push_str(tool);
        }
        if !self.task.trim().is_empty() {
            out.push_str("\ntask:\n");
            out.push_str(&redact_secrets(self.task.trim()));
        }
        if !self.notes.is_empty() {
            out.push_str("\nnotes:");
            for note in &self.notes {
                out.push_str("\n- ");
                out.push_str(note);
            }
        }
        if !self.commands.is_empty() {
            out.push_str("\ncommands:");
            for command in &self.commands {
                out.push_str("\n- ");
                out.push_str(command.status.as_str());
                out.push_str(": ");
                out.push_str(&command.command.join(" "));
                if let Some(code) = command.exit_code {
                    out.push_str(" (exit ");
                    out.push_str(&code.to_string());
                    out.push(')');
                }
                if !command.output.trim().is_empty() {
                    out.push_str("\n  ```text\n");
                    for line in command.output.trim().lines() {
                        out.push_str("  ");
                        out.push_str(line);
                        out.push('\n');
                    }
                    out.push_str("  ```");
                }
            }
        }
        out
    }
}

pub fn run_subagent(
    sandbox: &SandboxManager,
    box_id: &str,
    role: SubagentRole,
    task: &str,
    paths: &[String],
) -> SubagentReport {
    match role {
        SubagentRole::Planner => planner_report(role, task),
        SubagentRole::CodebaseResearcher => codebase_researcher_report(sandbox, box_id, role, task),
        SubagentRole::Patcher => patcher_report(sandbox, box_id, role, task),
        SubagentRole::Reviewer => reviewer_report(sandbox, box_id, role, task),
        SubagentRole::TestRunner => test_runner_report(sandbox, box_id, role, task, paths),
        SubagentRole::SecurityAuditor => security_auditor_report(sandbox, box_id, role, task),
    }
}

fn planner_report(role: SubagentRole, task: &str) -> SubagentReport {
    SubagentReport {
        role,
        task: task.to_string(),
        status: VerificationStatus::Passed,
        notes: vec![
            "Break the task into inspect, edit, verify, repair, and report phases.".to_string(),
            "Use specialized subagents for research, review, testing, and security checks when the task grows.".to_string(),
            "Command denials and tool errors are recoverable evidence; choose another route instead of stopping.".to_string(),
        ],
        commands: Vec::new(),
    }
}

fn codebase_researcher_report(
    sandbox: &SandboxManager,
    box_id: &str,
    role: SubagentRole,
    task: &str,
) -> SubagentReport {
    let commands = vec![
        run_readonly_command(
            sandbox,
            box_id,
            "file inventory",
            &[
                "rg",
                "--files",
                "--hidden",
                "--glob",
                "!target",
                "--glob",
                "!.git",
                "--glob",
                "!.wirecli",
            ],
        ),
        run_readonly_command(
            sandbox,
            box_id,
            "task search",
            &[
                "rg",
                "-n",
                "-C",
                "2",
                "--hidden",
                "--glob",
                "!target",
                "--glob",
                "!.git",
                "--glob",
                "!.wirecli",
                task,
                ".",
            ],
        ),
    ];
    report_from_commands(
        role,
        task,
        vec!["Read-only codebase research completed with bounded output.".to_string()],
        commands,
    )
}

fn patcher_report(
    sandbox: &SandboxManager,
    box_id: &str,
    role: SubagentRole,
    task: &str,
) -> SubagentReport {
    let commands = vec![run_readonly_command(
        sandbox,
        box_id,
        "candidate files",
        &[
            "rg",
            "-n",
            "-C",
            "1",
            "--hidden",
            "--glob",
            "!target",
            "--glob",
            "!.git",
            "--glob",
            "!.wirecli",
            task,
            ".",
        ],
    )];
    report_from_commands(
        role,
        task,
        vec![
            "Patcher subagent is intentionally read-only in this stage.".to_string(),
            "Apply edits through the main agent with apply_patch/write_file so verifier and checkpoints stay centralized.".to_string(),
        ],
        commands,
    )
}

fn reviewer_report(
    sandbox: &SandboxManager,
    box_id: &str,
    role: SubagentRole,
    task: &str,
) -> SubagentReport {
    let commands = vec![
        run_readonly_command(sandbox, box_id, "git status", &["git", "status", "--short"]),
        run_readonly_command(sandbox, box_id, "git diff stat", &["git", "diff", "--stat"]),
        run_readonly_command(
            sandbox,
            box_id,
            "git diff check",
            &["git", "diff", "--check"],
        ),
    ];
    report_from_commands(
        role,
        task,
        vec!["Review focuses on local diff and whitespace/conflict hazards.".to_string()],
        commands,
    )
}

fn test_runner_report(
    sandbox: &SandboxManager,
    box_id: &str,
    role: SubagentRole,
    task: &str,
    paths: &[String],
) -> SubagentReport {
    let verifier = VerifierPipeline::new(sandbox, box_id);
    let verifier_report = verifier.run_after_edit("subagent:test_runner", paths);
    let commands = verifier_report
        .commands
        .into_iter()
        .map(|command| SubagentCommandReport {
            label: command.label,
            command: command.command,
            status: command.status,
            exit_code: command.exit_code,
            output: command.output,
        })
        .collect::<Vec<_>>();
    SubagentReport {
        role,
        task: task.to_string(),
        status: verifier_report.status,
        notes: verifier_report.skipped,
        commands,
    }
}

fn security_auditor_report(
    sandbox: &SandboxManager,
    box_id: &str,
    role: SubagentRole,
    task: &str,
) -> SubagentReport {
    let commands = vec![run_readonly_command(
        sandbox,
        box_id,
        "security pattern scan",
        &[
            "rg",
            "-n",
            "--hidden",
            "--glob",
            "!target",
            "--glob",
            "!.git",
            "--glob",
            "!.wirecli",
            "sudo|curl\\s*\\|\\s*(sh|bash)|wget\\s+.*\\|\\s*(sh|bash)|chmod\\s+777|unwrap\\(\\)|expect\\(",
            ".",
        ],
    )];
    report_from_commands(
        role,
        task,
        vec![
            "Security auditor is read-only and reports suspicious patterns; findings still require human/code review.".to_string(),
        ],
        commands,
    )
}

fn report_from_commands(
    role: SubagentRole,
    task: &str,
    notes: Vec<String>,
    commands: Vec<SubagentCommandReport>,
) -> SubagentReport {
    let status = if commands
        .iter()
        .any(|command| command.status == VerificationStatus::Failed)
    {
        VerificationStatus::Failed
    } else if commands
        .iter()
        .any(|command| command.status == VerificationStatus::Blocked)
    {
        VerificationStatus::Blocked
    } else {
        VerificationStatus::Passed
    };
    SubagentReport {
        role,
        task: task.to_string(),
        status,
        notes,
        commands,
    }
}

fn run_readonly_command(
    sandbox: &SandboxManager,
    box_id: &str,
    label: &str,
    command: &[&str],
) -> SubagentCommandReport {
    let command = command
        .iter()
        .map(|part| part.to_string())
        .collect::<Vec<_>>();
    match sandbox.run_capture_approved(
        box_id,
        &command,
        SandboxRunOptions {
            network_access: false,
            ..SandboxRunOptions::default()
        },
    ) {
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
                Some(0) | Some(1) if command.first().map(String::as_str) == Some("rg") => {
                    VerificationStatus::Passed
                }
                Some(0) => VerificationStatus::Passed,
                _ => VerificationStatus::Failed,
            };
            SubagentCommandReport {
                label: label.to_string(),
                command,
                status,
                exit_code: result.status_code,
                output: clip_output(&output),
            }
        }
        Err(err) => SubagentCommandReport {
            label: label.to_string(),
            command,
            status: VerificationStatus::Blocked,
            exit_code: None,
            output: clip_output(&err),
        },
    }
}

fn normalize_role(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .replace('-', "_")
        .replace(' ', "_")
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
    clipped.push_str("\n... output truncated by Wire subagent ...");
    clipped
}

#[cfg(test)]
mod tests {
    use super::{planner_report, SubagentRole};
    use crate::verifier::VerificationStatus;

    #[test]
    fn parses_role_aliases() {
        assert_eq!(
            SubagentRole::from_value("codebase-researcher"),
            Some(SubagentRole::CodebaseResearcher)
        );
        assert_eq!(
            SubagentRole::from_value("security auditor"),
            Some(SubagentRole::SecurityAuditor)
        );
        assert_eq!(SubagentRole::from_value("unknown"), None);
    }

    #[test]
    fn planner_report_is_recoverable_guidance() {
        let report = planner_report(SubagentRole::Planner, "ship feature");
        let text = report.to_model_output();
        assert_eq!(report.status, VerificationStatus::Passed);
        assert!(text.contains("Command denials and tool errors are recoverable evidence"));
        assert!(text.contains("role: planner"));
    }
}
