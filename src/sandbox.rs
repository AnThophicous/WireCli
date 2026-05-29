use crate::config::AppPaths;
use crate::id::next_id;
use crate::orchestrator::BoxScheduler;
use crate::policy::CommandPolicy;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct SandboxSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub state: String,
}

pub struct SandboxManager {
    sandboxes_dir: PathBuf,
    scheduler: Arc<BoxScheduler>,
}

impl SandboxManager {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        fs::create_dir_all(&paths.sandboxes_dir).map_err(|e| e.to_string())?;
        Ok(Self {
            sandboxes_dir: paths.sandboxes_dir.clone(),
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
        fs::create_dir_all(dir.join("workspace")).map_err(|e| e.to_string())?;
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

    pub fn destroy(&self, id: &str) -> Result<(), String> {
        let path = self.sandbox_dir(id);
        if path.exists() {
            fs::remove_dir_all(path).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn run(&self, id: &str, command: &[String]) -> Result<ExitStatus, String> {
        if command.is_empty() {
            return Err("missing command".to_string());
        }

        CommandPolicy::standard()
            .validate(command)
            .map_err(|violation| format!("{}: {}", violation.command, violation.reason))?;

        let (tx, rx) = mpsc::channel();
        let summary = self.get(id)?;
        let sandbox_path = self.sandbox_dir(&summary.id);
        let workspace = sandbox_path.join("workspace");
        fs::create_dir_all(&workspace).map_err(|e| e.to_string())?;
        self.append_trace(&summary.id, "queue", command)?;

        let summary_clone = summary.clone();
        let workspace_clone = workspace.clone();
        let command_clone = command.to_vec();
        let scheduler = Arc::clone(&self.scheduler);
        scheduler.submit(move || {
            let result = execute_cell_command(&summary_clone, &workspace_clone, &command_clone)
                .map(|run| {
                    let exit_record = match run.status.code() {
                        Some(code) => format!("exit:{code}"),
                        None => "exit:signal".to_string(),
                    };
                    replay_output(&run.stdout, &run.stderr);
                    (run.status, exit_record)
                });
            let _ = tx.send(result);
        })?;

        let result = rx.recv().map_err(|e| e.to_string())?;
        match result {
            Ok((status, exit_record)) => {
                self.append_trace(&summary.id, &exit_record, command)?;
                Ok(status)
            }
            Err(err) => Err(err),
        }
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

    fn append_trace(&self, id: &str, kind: &str, command: &[String]) -> Result<(), String> {
        let path = self.sandbox_dir(id).join("trace.log");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        writeln!(file, "{}\t{}\t{}", kind, now_string(), command.join(" "))
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

fn bind_host_paths(process: &mut Command) -> Result<(), String> {
    for path in ["/usr", "/bin", "/lib", "/lib64", "/sbin"] {
        let p = Path::new(path);
        if p.exists() {
            process.arg("--ro-bind").arg(p).arg(path);
        }
    }
    Ok(())
}

struct CommandRun {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl CommandRun {
    fn is_bwrap_namespace_error(&self) -> bool {
        let stderr = String::from_utf8_lossy(&self.stderr);
        stderr.contains("bwrap:")
            && (stderr.contains("NETLINK_ROUTE") || stderr.contains("Operation not permitted"))
    }
}

fn execute_cell_command(
    summary: &SandboxSummary,
    workspace: &Path,
    command: &[String],
) -> Result<CommandRun, String> {
    #[cfg(target_os = "linux")]
    {
        match execute_bwrap(summary, workspace, command, true) {
            Ok(primary) => {
                if primary.is_bwrap_namespace_error() {
                    return execute_portable(summary, workspace, command);
                }
                Ok(primary)
            }
            Err(err) => {
                if err.contains("bwrap") || err.contains("Operation not permitted") {
                    execute_portable(summary, workspace, command)
                } else {
                    Err(err)
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        execute_portable(summary, workspace, command)
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        execute_portable(summary, workspace, command)
    }
}

#[cfg(target_os = "linux")]
fn execute_bwrap(
    summary: &SandboxSummary,
    workspace: &Path,
    command: &[String],
    with_net_namespace: bool,
) -> Result<CommandRun, String> {
    let mut process = Command::new("bwrap");
    process
        .arg("--die-with-parent")
        .arg("--unshare-user")
        .arg("--unshare-pid")
        .arg("--unshare-ipc")
        .arg("--unshare-uts")
        .arg("--uid")
        .arg("0")
        .arg("--gid")
        .arg("0")
        .arg("--clearenv")
        .arg("--setenv")
        .arg("HOME")
        .arg("/home/rift")
        .arg("--setenv")
        .arg("USER")
        .arg("rift")
        .arg("--setenv")
        .arg("LOGNAME")
        .arg("rift")
        .arg("--setenv")
        .arg("PATH")
        .arg("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        .arg("--setenv")
        .arg("SANDBOX_ID")
        .arg(&summary.id)
        .arg("--setenv")
        .arg("SANDBOX_NAME")
        .arg(&summary.name)
        .arg("--chdir")
        .arg("/workspace")
        .arg("--bind")
        .arg(workspace)
        .arg("/workspace")
        .arg("--tmpfs")
        .arg("/tmp")
        .arg("--proc")
        .arg("/proc")
        .arg("--dev")
        .arg("/dev")
        .arg("--dir")
        .arg("/home/rift");

    if with_net_namespace {
        process.arg("--unshare-net");
    }

    bind_host_paths(&mut process)?;

    let output = process
        .arg("--")
        .arg(&command[0])
        .args(&command[1..])
        .output()
        .map_err(|e| e.to_string())?;

    Ok(CommandRun {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn execute_portable(
    summary: &SandboxSummary,
    workspace: &Path,
    command: &[String],
) -> Result<CommandRun, String> {
    let mut process = Command::new(&command[0]);
    let home_dir = workspace.join("home");
    fs::create_dir_all(&home_dir).map_err(|e| e.to_string())?;
    process
        .current_dir(workspace)
        .env_clear()
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .env("PATH", portable_path())
        .env("SANDBOX_ID", &summary.id)
        .env("SANDBOX_NAME", &summary.name);
    let output = process
        .args(&command[1..])
        .output()
        .map_err(|e| e.to_string())?;

    Ok(CommandRun {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn replay_output(stdout: &[u8], stderr: &[u8]) {
    if !stdout.is_empty() {
        let _ = std::io::stdout().write_all(stdout);
    }
    if !stderr.is_empty() {
        let _ = std::io::stderr().write_all(stderr);
    }
}

fn shared_scheduler() -> Arc<BoxScheduler> {
    static SCHEDULER: OnceLock<Arc<BoxScheduler>> = OnceLock::new();
    Arc::clone(SCHEDULER.get_or_init(|| {
        let workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        Arc::new(BoxScheduler::new(workers))
    }))
}

pub fn probe_bwrap() -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        return Ok("not used on this OS".to_string());
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        return Ok("unsupported on this OS".to_string());
    }

    #[cfg(target_os = "linux")]
    {
        let output = Command::new("bwrap")
            .arg("--version")
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err("bwrap is installed but did not report a usable version".to_string());
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if version.is_empty() {
            Ok("available".to_string())
        } else {
            Ok(version)
        }
    }
}

fn portable_path() -> String {
    #[cfg(target_os = "windows")]
    {
        return "C:\\Windows\\System32;C:\\Windows;C:\\Windows\\System32\\Wbem".to_string();
    }

    #[cfg(not(target_os = "windows"))]
    {
        return "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string();
    }
}

fn now_string() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    secs.to_string()
}

fn sanitize_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "sandbox".to_string();
    }

    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }

    if out.is_empty() {
        "sandbox".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_name;

    #[test]
    fn sanitize_name_rewrites_spaces() {
        assert_eq!(sanitize_name("My Sandbox"), "My-Sandbox");
    }
}
