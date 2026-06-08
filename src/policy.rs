use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CommandPolicy {
    deny_prefixes: Vec<String>,
    deny_exact: Vec<String>,
    deny_strict_programs: Vec<String>,
    deny_network_programs: Vec<String>,
    deny_shell_entrypoints: Vec<String>,
    block_long_running: bool,
}

#[derive(Debug, Clone)]
pub struct PolicyViolation {
    pub command: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandAssessment {
    pub decision: CommandDecision,
    pub command: String,
    pub risk: String,
    pub explanation: String,
    pub needs_network: bool,
}

impl CommandPolicy {
    pub fn standard() -> Self {
        Self {
            deny_prefixes: vec![
                "sudo".to_string(),
                "su".to_string(),
                "doas".to_string(),
                "pkexec".to_string(),
            ],
            deny_exact: vec![
                "shutdown".to_string(),
                "reboot".to_string(),
                "poweroff".to_string(),
                "halt".to_string(),
                "mkfs".to_string(),
                "mount".to_string(),
                "umount".to_string(),
                "systemctl".to_string(),
                "service".to_string(),
                "chown".to_string(),
                "chmod".to_string(),
            ],
            deny_strict_programs: Vec::new(),
            deny_network_programs: vec![
                "curl".to_string(),
                "wget".to_string(),
                "ssh".to_string(),
                "scp".to_string(),
                "sftp".to_string(),
                "rsync".to_string(),
                "nc".to_string(),
                "ncat".to_string(),
                "netcat".to_string(),
                "socat".to_string(),
            ],
            deny_shell_entrypoints: vec![
                "bash".to_string(),
                "sh".to_string(),
                "zsh".to_string(),
                "fish".to_string(),
            ],
            block_long_running: false,
        }
    }

    pub fn strict() -> Self {
        Self {
            deny_prefixes: vec![
                "sudo".to_string(),
                "su".to_string(),
                "doas".to_string(),
                "pkexec".to_string(),
            ],
            deny_exact: vec![
                "shutdown".to_string(),
                "reboot".to_string(),
                "poweroff".to_string(),
                "halt".to_string(),
                "docker".to_string(),
                "podman".to_string(),
                "kubectl".to_string(),
                "ssh".to_string(),
                "scp".to_string(),
                "sftp".to_string(),
                "rsync".to_string(),
                "curl".to_string(),
                "wget".to_string(),
                "nc".to_string(),
                "ncat".to_string(),
                "netcat".to_string(),
                "socat".to_string(),
            ],
            deny_strict_programs: vec![
                "bash".to_string(),
                "sh".to_string(),
                "zsh".to_string(),
                "fish".to_string(),
                "node".to_string(),
                "python".to_string(),
                "python3".to_string(),
                "ruby".to_string(),
                "php".to_string(),
                "perl".to_string(),
                "deno".to_string(),
            ],
            deny_network_programs: vec![
                "curl".to_string(),
                "wget".to_string(),
                "ssh".to_string(),
                "scp".to_string(),
                "sftp".to_string(),
                "rsync".to_string(),
                "nc".to_string(),
                "ncat".to_string(),
                "netcat".to_string(),
                "socat".to_string(),
            ],
            deny_shell_entrypoints: vec![
                "bash".to_string(),
                "sh".to_string(),
                "zsh".to_string(),
                "fish".to_string(),
            ],
            block_long_running: true,
        }
    }

    pub fn validate(&self, command: &[String]) -> Result<(), PolicyViolation> {
        self.validate_inner(command, None, false)
    }

    pub fn validate_for_workspace(
        &self,
        command: &[String],
        workspace: &Path,
    ) -> Result<(), PolicyViolation> {
        self.validate_inner(command, Some(workspace), false)
    }

    pub fn validate_hard_for_workspace(
        &self,
        command: &[String],
        workspace: &Path,
    ) -> Result<(), PolicyViolation> {
        self.validate_inner(command, Some(workspace), true)
    }

    pub fn assess(&self, command: &[String]) -> CommandAssessment {
        self.assess_inner(command, None)
    }

    pub fn assess_for_workspace(&self, command: &[String], workspace: &Path) -> CommandAssessment {
        self.assess_inner(command, Some(workspace))
    }

    fn validate_inner(
        &self,
        command: &[String],
        workspace: Option<&Path>,
        hard_only: bool,
    ) -> Result<(), PolicyViolation> {
        let assessment = self.assess_inner(command, workspace);
        match assessment.decision {
            CommandDecision::Allow => Ok(()),
            CommandDecision::Ask if hard_only => Ok(()),
            CommandDecision::Ask | CommandDecision::Deny => Err(PolicyViolation {
                command: assessment.command,
                reason: assessment.explanation,
            }),
        }
    }

    fn assess_inner(&self, command: &[String], workspace: Option<&Path>) -> CommandAssessment {
        let Some(program) = command.first() else {
            return deny("", "missing", "missing command");
        };

        if command.iter().any(|arg| contains_control_char(arg)) {
            return deny(
                &command.join(" "),
                "high",
                "command contains control characters",
            );
        }

        let program_lower = program.to_ascii_lowercase();
        let program_name = Path::new(&program_lower)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(&program_lower)
            .to_string();

        if matches!(
            program_name.as_str(),
            "chmod" | "chown" | "docker" | "podman" | "kubectl"
        ) {
            return ask(
                command,
                "high",
                "command can change permissions, ownership, containers, or cluster state",
                false,
            );
        }

        if self
            .deny_exact
            .iter()
            .any(|item| item == &program_lower || item == &program_name)
        {
            return deny(
                program,
                "critical",
                "privileged host command is blocked inside Box",
            );
        }

        if self
            .deny_prefixes
            .iter()
            .any(|prefix| program_lower.starts_with(prefix))
        {
            return deny(
                program,
                "critical",
                "privilege escalation command is blocked inside Box",
            );
        }

        if self
            .deny_strict_programs
            .iter()
            .any(|item| item == &program_name)
        {
            return deny(
                program,
                "high",
                "strict execution blocks direct interpreter or shell entrypoints",
            );
        }

        if self
            .deny_shell_entrypoints
            .iter()
            .any(|item| item == &program_name)
        {
            return ask(
                command,
                "medium",
                "direct shell entrypoint requires explicit approval",
                false,
            );
        }

        if self
            .deny_network_programs
            .iter()
            .any(|item| item == &program_name)
        {
            return ask(
                command,
                "high",
                "direct network transfer tool requires explicit approval and temporary network access",
                true,
            );
        }

        if program_name == "git"
            && command
                .iter()
                .skip(1)
                .any(|arg| matches!(arg.as_str(), "clone" | "fetch" | "pull"))
        {
            return ask(
                command,
                "medium",
                "git network operation requires explicit approval and temporary network access",
                true,
            );
        }

        if command
            .iter()
            .skip(1)
            .any(|arg| matches!(arg.as_str(), "sudo" | "su" | "doas" | "pkexec"))
        {
            return deny(
                program,
                "critical",
                "privileged subcommand is blocked inside Box",
            );
        }

        if blocks_interpreter_inline_code(&program_name, command) {
            return ask(
                command,
                "medium",
                "inline interpreter execution requires explicit approval",
                false,
            );
        }

        if let Some(workspace) = workspace {
            if let Err(violation) = validate_command_paths(command, workspace) {
                return deny(&violation.command, "high", &violation.reason);
            }
        }

        if self.block_long_running && blocks_long_running_or_listener(&program_lower, command) {
            return deny(
                program,
                "medium",
                "strict execution blocks long-running servers, dev watchers, and listener commands",
            );
        }

        if blocks_long_running_or_listener(&program_lower, command) {
            return ask(
                command,
                "medium",
                "long-running server, watcher, or listener command requires explicit approval",
                false,
            );
        }

        allow(command, "low", "command is allowed by the current policy")
    }
}

fn allow(command: &[String], risk: &str, explanation: &str) -> CommandAssessment {
    CommandAssessment {
        decision: CommandDecision::Allow,
        command: command.join(" "),
        risk: risk.to_string(),
        explanation: explanation.to_string(),
        needs_network: false,
    }
}

fn ask(
    command: &[String],
    risk: &str,
    explanation: &str,
    needs_network: bool,
) -> CommandAssessment {
    CommandAssessment {
        decision: CommandDecision::Ask,
        command: command.join(" "),
        risk: risk.to_string(),
        explanation: explanation.to_string(),
        needs_network,
    }
}

fn deny(command: &str, risk: &str, explanation: &str) -> CommandAssessment {
    CommandAssessment {
        decision: CommandDecision::Deny,
        command: command.to_string(),
        risk: risk.to_string(),
        explanation: explanation.to_string(),
        needs_network: false,
    }
}

fn contains_control_char(value: &str) -> bool {
    value
        .chars()
        .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
}

fn blocks_interpreter_inline_code(program: &str, command: &[String]) -> bool {
    match program {
        "python" | "python3" | "node" | "ruby" | "php" | "perl" | "deno" => command
            .iter()
            .skip(1)
            .any(|arg| matches!(arg.as_str(), "-c" | "-e" | "--eval" | "eval")),
        _ => false,
    }
}

fn validate_command_paths(command: &[String], workspace: &Path) -> Result<(), PolicyViolation> {
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    for arg in command.iter().skip(1) {
        if arg.starts_with('-') || looks_like_url(arg) || is_env_assignment(arg) {
            continue;
        }
        if arg == ".." || arg.starts_with("../") || arg.contains("/../") {
            return Err(PolicyViolation {
                command: command.join(" "),
                reason: "parent-directory path traversal is blocked by Lattice".to_string(),
            });
        }
        let path = PathBuf::from(arg);
        if path.is_absolute() {
            let candidate = workspace_alias_path(&workspace, &path).unwrap_or(path);
            let canonical = canonical_existing_target_or_parent(&candidate);
            if !canonical.starts_with(&workspace) {
                return Err(PolicyViolation {
                    command: command.join(" "),
                    reason: "absolute paths outside the Box are blocked".to_string(),
                });
            }
        } else if looks_like_path(arg) {
            let candidate = workspace.join(&path);
            let canonical = canonical_existing_target_or_parent(&candidate);
            if !canonical.starts_with(&workspace) {
                return Err(PolicyViolation {
                    command: command.join(" "),
                    reason: "command path escapes the Box through a symlink".to_string(),
                });
            }
        }
    }
    Ok(())
}

fn canonical_existing_target_or_parent(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    let mut current = path;
    while let Some(parent) = current.parent() {
        if let Ok(canonical) = parent.canonicalize() {
            return canonical;
        }
        current = parent;
    }
    path.to_path_buf()
}

fn workspace_alias_path(workspace: &Path, path: &Path) -> Option<PathBuf> {
    path.strip_prefix("/workspace")
        .ok()
        .map(|relative| workspace.join(relative))
}

fn looks_like_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://") || value.starts_with("git@")
}

fn is_env_assignment(value: &str) -> bool {
    let Some((key, _)) = value.split_once('=') else {
        return false;
    };
    !key.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

fn looks_like_path(value: &str) -> bool {
    value.contains('/')
        || value.starts_with('.')
        || value.ends_with(".rs")
        || value.ends_with(".js")
}

fn blocks_long_running_or_listener(program: &str, command: &[String]) -> bool {
    let lower_args = command
        .iter()
        .skip(1)
        .map(|arg| arg.to_ascii_lowercase())
        .collect::<Vec<_>>();

    let has = |needle: &str| lower_args.iter().any(|arg| arg == needle);
    let contains = |needle: &str| lower_args.iter().any(|arg| arg.contains(needle));

    match program {
        "npm" | "pnpm" | "yarn" | "bun" => {
            has("start")
                || has("dev")
                || has("serve")
                || has("preview")
                || has("watch")
                || has("runserver")
                || contains("webpack-dev-server")
                || contains("vite")
                || contains("next")
        }
        "npx" => contains("vite") || contains("next") || contains("serve"),
        "cargo" => has("run") || has("watch"),
        "python" | "python3" => {
            contains("http.server")
                || contains("uvicorn")
                || contains("gunicorn")
                || contains("runserver")
        }
        "go" => has("run"),
        "java" => contains("server"),
        _ => {
            has("serve")
                || has("server")
                || has("dev")
                || has("preview")
                || has("watch")
                || contains("http.server")
                || contains("uvicorn")
                || contains("gunicorn")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CommandPolicy;

    #[test]
    fn blocks_privileged_command() {
        let policy = CommandPolicy::standard();
        let result = policy.validate(&vec!["sudo".to_string(), "rm".to_string()]);
        assert!(result.is_err());
    }

    #[test]
    fn allows_regular_command() {
        let policy = CommandPolicy::standard();
        let result = policy.validate(&vec!["echo".to_string(), "ok".to_string()]);
        assert!(result.is_ok());
    }

    #[test]
    fn allows_regex_metacharacters_as_argv_literals() {
        let policy = CommandPolicy::standard();
        for pattern in [
            "loop\\|turn\\|step",
            "panic!\\|todo!\\|unimplemented!",
            "foo;bar",
            "$(literal)",
            "`literal`",
            "a && b || c",
            "x > y < z",
        ] {
            let result = policy.validate(&vec![
                "rg".to_string(),
                "-n".to_string(),
                pattern.to_string(),
                "src/responses_agent.rs".to_string(),
            ]);
            assert!(result.is_ok(), "{pattern} should be an argv literal");
        }
    }

    #[test]
    fn blocks_direct_shell_entrypoint() {
        let policy = CommandPolicy::standard();
        let result = policy.validate(&vec![
            "sh".to_string(),
            "-lc".to_string(),
            "pwd".to_string(),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn normal_allows_npm_start() {
        let policy = CommandPolicy::standard();
        let result = policy.validate(&vec!["npm".to_string(), "start".to_string()]);
        assert!(result.is_err());
        let assessment = policy.assess(&vec!["npm".to_string(), "start".to_string()]);
        assert_eq!(assessment.decision, super::CommandDecision::Ask);
    }

    #[test]
    fn network_command_requires_approval_not_hard_deny() {
        let policy = CommandPolicy::standard();
        let command = vec!["curl".to_string(), "https://example.com".to_string()];
        let assessment = policy.assess(&command);
        assert_eq!(assessment.decision, super::CommandDecision::Ask);
        assert!(assessment.needs_network);

        let cwd = std::env::current_dir().unwrap();
        let result = policy.validate_hard_for_workspace(&command, &cwd);
        assert!(result.is_ok());
    }

    #[test]
    fn blocks_parent_path_traversal() {
        let policy = CommandPolicy::standard();
        let cwd = std::env::current_dir().unwrap();
        let result =
            policy.validate_for_workspace(&vec!["ls".to_string(), "../".to_string()], &cwd);
        assert!(result.is_err());
    }

    #[test]
    fn allows_workspace_absolute_alias_inside_box() {
        let policy = CommandPolicy::standard();
        let workspace = std::env::temp_dir().join("wirecli-policy-workspace-alias");
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/lib.rs"), "").unwrap();

        let result = policy.validate_for_workspace(
            &vec![
                "rg".to_string(),
                "-n".to_string(),
                "fn".to_string(),
                "/workspace/src/lib.rs".to_string(),
            ],
            &workspace,
        );

        assert!(result.is_ok());
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn allows_host_absolute_path_inside_workspace() {
        let policy = CommandPolicy::standard();
        let workspace = std::env::temp_dir().join("wirecli-policy-host-absolute");
        let _ = std::fs::remove_dir_all(&workspace);
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        let file = workspace.join("src/lib.rs");
        std::fs::write(&file, "").unwrap();

        let result = policy.validate_for_workspace(
            &vec![
                "rg".to_string(),
                "-n".to_string(),
                "fn".to_string(),
                file.display().to_string(),
            ],
            &workspace,
        );

        assert!(result.is_ok());
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[cfg(unix)]
    #[test]
    fn blocks_missing_command_path_under_symlink_to_outside_workspace() {
        use std::os::unix::fs::symlink;

        let policy = CommandPolicy::standard();
        let workspace = std::env::temp_dir().join("wirecli-policy-symlink-workspace");
        let outside = std::env::temp_dir().join("wirecli-policy-symlink-outside");
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, workspace.join("outside")).unwrap();

        let result = policy.validate_for_workspace(
            &vec!["touch".to_string(), "outside/new-file.txt".to_string()],
            &workspace,
        );

        assert!(result.is_err());
        let _ = std::fs::remove_dir_all(workspace);
        let _ = std::fs::remove_dir_all(outside);
    }
}
