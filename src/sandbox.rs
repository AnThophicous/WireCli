mod metadata;
mod runtime;
mod types;

pub use types::{CommandResult, SandboxRunOptions, SandboxSummary};

use crate::approvals::{ApprovalResolution, ApprovalStore};
use crate::config::AppPaths;
use crate::id::next_id;
use crate::orchestrator::BoxScheduler;
use crate::policy::CommandPolicy;
use metadata::{now_string, sanitize_name};
use runtime::{execute_cell_command, replay_output, shared_scheduler, status_from_code};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::ExitStatus;
use std::sync::{mpsc, Arc};

pub struct SandboxManager {
    root_dir: PathBuf,
    sandboxes_dir: PathBuf,
    approvals: ApprovalStore,
    scheduler: Arc<BoxScheduler>,
}

impl SandboxManager {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        fs::create_dir_all(&paths.sandboxes_dir).map_err(|e| e.to_string())?;
        Ok(Self {
            root_dir: paths.root_dir.clone(),
            sandboxes_dir: paths.sandboxes_dir.clone(),
            approvals: ApprovalStore::new(paths)?,
            scheduler: shared_scheduler(),
        })
    }

    pub fn create(&self, name: &str) -> Result<SandboxSummary, String> {
        let summary = SandboxSummary {
            id: next_id(),
            name: sanitize_name(name),
            created_at: now_string(),
            state: "ready".to_string(),
        };

        let dir = self.sandbox_dir(&summary.id);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        self.write_metadata(&summary)?;
        Ok(summary)
    }

    pub fn list(&self) -> Result<Vec<SandboxSummary>, String> {
        let mut sandboxes = Vec::new();
        for entry in fs::read_dir(&self.sandboxes_dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(summary) = self.read_metadata(&path)? {
                sandboxes.push(summary);
            }
        }
        sandboxes.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(sandboxes)
    }

    pub fn get(&self, id: &str) -> Result<SandboxSummary, String> {
        let path = self.sandbox_dir(id);
        self.read_metadata(&path)?
            .ok_or_else(|| format!("sandbox not found: {id}"))
    }

    pub fn workspace_path(&self, id: &str) -> PathBuf {
        let _ = id;
        self.root_dir.clone()
    }

    pub fn destroy(&self, id: &str) -> Result<(), String> {
        let path = self.sandbox_dir(id);
        if path.exists() {
            fs::remove_dir_all(path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn run(&self, id: &str, command: &[String]) -> Result<ExitStatus, String> {
        let result = self.run_capture(id, command)?;
        replay_output(&result.stdout, &result.stderr);
        Ok(status_from_code(result.status_code))
    }

    pub fn run_capture(&self, id: &str, command: &[String]) -> Result<CommandResult, String> {
        let summary = self.get(id)?;
        let workspace = self.workspace_path(&summary.id);
        let assessment = CommandPolicy::standard().assess_for_workspace(command, &workspace);
        let approval = self.approve_command(command, "manual Box command", &assessment)?;
        self.run_capture_approved(
            id,
            command,
            SandboxRunOptions {
                network_access: approval.network_access,
                ..SandboxRunOptions::default()
            },
        )
    }

    pub fn run_capture_approved(
        &self,
        id: &str,
        command: &[String],
        options: SandboxRunOptions,
    ) -> Result<CommandResult, String> {
        if command.is_empty() {
            return Err("missing command".to_string());
        }

        let (tx, rx) = mpsc::channel();
        let summary = self.get(id)?;
        let workspace = self.workspace_path(&summary.id);
        CommandPolicy::standard()
            .validate_hard_for_workspace(command, &workspace)
            .map_err(|violation| format!("{}: {}", violation.command, violation.reason))?;
        self.append_trace(&summary.id, "queue", command, options)?;
        let sandbox_command = rewrite_command_paths_for_sandbox(command, &workspace);

        let summary_clone = summary.clone();
        let workspace_clone = workspace.clone();
        let command_clone = sandbox_command;
        let scheduler = Arc::clone(&self.scheduler);
        scheduler.submit(move || {
            let result =
                execute_cell_command(&summary_clone, &workspace_clone, &command_clone, options)
                    .map(|run| {
                        let exit_record = match run.status.code() {
                            Some(code) => format!("exit:{code}"),
                            None => "exit:signal".to_string(),
                        };
                        (run.status, exit_record, run.stdout, run.stderr)
                    });
            let _ = tx.send(result);
        })?;

        let result = rx.recv().map_err(|e| e.to_string())?;
        match result {
            Ok((status, exit_record, stdout, stderr)) => {
                self.append_trace(&summary.id, &exit_record, command, options)?;
                Ok(CommandResult {
                    status_code: status.code(),
                    stdout,
                    stderr,
                })
            }
            Err(err) => Err(err),
        }
    }

    pub fn approve_command(
        &self,
        command: &[String],
        reason: &str,
        assessment: &crate::policy::CommandAssessment,
    ) -> Result<ApprovalResolution, String> {
        self.approvals
            .ensure_command_approved(command, reason, assessment)
    }

    fn write_metadata(&self, summary: &SandboxSummary) -> Result<(), String> {
        let dir = self.sandbox_dir(&summary.id);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("sandbox.meta");
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        writeln!(file, "id={}", summary.id).map_err(|e| e.to_string())?;
        writeln!(file, "name={}", summary.name).map_err(|e| e.to_string())?;
        writeln!(file, "created_at={}", summary.created_at).map_err(|e| e.to_string())?;
        writeln!(file, "state={}", summary.state).map_err(|e| e.to_string())?;
        Ok(())
    }

    fn append_trace(
        &self,
        id: &str,
        kind: &str,
        command: &[String],
        options: SandboxRunOptions,
    ) -> Result<(), String> {
        let path = self.sandbox_dir(id).join("trace.log");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        writeln!(
            file,
            "{}\t{}\tnetwork={}\tportable_fallback={}\t{}",
            kind,
            now_string(),
            options.network_access,
            options.allow_portable_fallback,
            command.join(" ")
        )
        .map_err(|e| e.to_string())
    }

    fn read_metadata(&self, path: &Path) -> Result<Option<SandboxSummary>, String> {
        let meta_path = path.join("sandbox.meta");
        if !meta_path.exists() {
            return Ok(None);
        }

        let file = fs::File::open(meta_path).map_err(|e| e.to_string())?;
        let reader = BufReader::new(file);
        let mut id = String::new();
        let mut name = String::from("sandbox");
        let mut created_at = now_string();
        let mut state = String::from("ready");

        for line in reader.lines() {
            let line = line.map_err(|e| e.to_string())?;
            let mut parts = line.splitn(2, '=');
            let key = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("").to_string();
            match key {
                "id" => id = value,
                "name" => name = value,
                "created_at" => created_at = value,
                "state" => state = value,
                _ => {}
            }
        }

        if id.is_empty() {
            return Ok(None);
        }

        Ok(Some(SandboxSummary {
            id,
            name,
            created_at,
            state,
        }))
    }

    fn sandbox_dir(&self, id: &str) -> PathBuf {
        self.sandboxes_dir.join(id)
    }
}

fn rewrite_command_paths_for_sandbox(command: &[String], workspace: &Path) -> Vec<String> {
    let canonical_workspace =
        fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    command
        .iter()
        .map(|arg| rewrite_arg_path_for_sandbox(arg, workspace, &canonical_workspace))
        .collect()
}

fn rewrite_arg_path_for_sandbox(arg: &str, workspace: &Path, canonical_workspace: &Path) -> String {
    let path = Path::new(arg);
    if !path.is_absolute() || path.starts_with("/workspace") {
        return arg.to_string();
    }

    if let Ok(relative) = path.strip_prefix(workspace) {
        return workspace_alias(relative);
    }

    let canonical = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Ok(relative) = canonical.strip_prefix(canonical_workspace) {
        return workspace_alias(relative);
    }

    arg.to_string()
}

fn workspace_alias(relative: &Path) -> String {
    let path = Path::new("/workspace").join(relative);
    path.display().to_string()
}

pub fn probe_bwrap() -> Result<String, String> {
    runtime::probe_bwrap()
}

#[cfg(test)]
mod tests {
    use super::rewrite_command_paths_for_sandbox;
    use std::fs;

    #[test]
    fn rewrites_host_absolute_workspace_paths_to_sandbox_alias() {
        let workspace = std::env::temp_dir().join("wirecli-sandbox-path-rewrite");
        let _ = fs::remove_dir_all(&workspace);
        fs::create_dir_all(workspace.join("src")).unwrap();
        let file = workspace.join("src/lib.rs");
        fs::write(&file, "").unwrap();

        let command = vec![
            "rg".to_string(),
            "-n".to_string(),
            "fn".to_string(),
            file.display().to_string(),
        ];
        let rewritten = rewrite_command_paths_for_sandbox(&command, &workspace);

        assert_eq!(rewritten[3], "/workspace/src/lib.rs");
        let _ = fs::remove_dir_all(workspace);
    }

    #[test]
    fn keeps_system_binary_paths_and_workspace_aliases_unchanged() {
        let workspace = std::env::temp_dir().join("wirecli-sandbox-path-rewrite-system");
        let command = vec![
            "/usr/bin/rg".to_string(),
            "-n".to_string(),
            "fn".to_string(),
            "/workspace/src/lib.rs".to_string(),
        ];
        let rewritten = rewrite_command_paths_for_sandbox(&command, &workspace);

        assert_eq!(rewritten[0], "/usr/bin/rg");
        assert_eq!(rewritten[3], "/workspace/src/lib.rs");
    }
}
