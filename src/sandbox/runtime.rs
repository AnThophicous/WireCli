use super::types::{CommandRun, SandboxRunOptions, SandboxSummary};
use crate::orchestrator::BoxScheduler;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const COMMAND_TIMEOUT_SECS: u64 = 90;

pub(crate) fn execute_cell_command(
    summary: &SandboxSummary,
    workspace: &Path,
    command: &[String],
    options: SandboxRunOptions,
) -> Result<CommandRun, String> {
    #[cfg(target_os = "linux")]
    {
        match execute_bwrap(summary, workspace, command, !options.network_access) {
            Ok(primary) => {
                if primary.is_bwrap_namespace_error() && options.allow_portable_fallback {
                    return execute_portable(summary, workspace, command);
                }
                if primary.is_bwrap_namespace_error() {
                    return Err(os_sandbox_required_error(
                        "bubblewrap namespace setup failed",
                    ));
                }
                Ok(primary)
            }
            Err(err) => {
                if options.allow_portable_fallback
                    && (err.contains("bwrap") || err.contains("Operation not permitted"))
                {
                    execute_portable(summary, workspace, command)
                } else if err.contains("bwrap") || err.contains("Operation not permitted") {
                    Err(os_sandbox_required_error(&err))
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
        .arg("/home/wire")
        .arg("--setenv")
        .arg("USER")
        .arg("wire")
        .arg("--setenv")
        .arg("LOGNAME")
        .arg("wire")
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
        .arg("/home/wire");

    if with_net_namespace {
        process.arg("--unshare-net");
    }

    bind_host_paths(&mut process);

    process.arg("--").arg(&command[0]).args(&command[1..]);
    execute_with_timeout(&mut process)
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn execute_portable(
    summary: &SandboxSummary,
    workspace: &Path,
    command: &[String],
) -> Result<CommandRun, String> {
    let mut process = Command::new(&command[0]);
    let home_dir = workspace
        .join(".wirecli")
        .join("boxes")
        .join(&summary.id)
        .join("home");
    fs::create_dir_all(&home_dir).map_err(|e| e.to_string())?;
    process
        .current_dir(workspace)
        .env_clear()
        .env("HOME", &home_dir)
        .env("USERPROFILE", &home_dir)
        .env("PATH", portable_path())
        .env("SANDBOX_ID", &summary.id)
        .env("SANDBOX_NAME", &summary.name);
    process.args(&command[1..]);
    execute_with_timeout(&mut process)
}

fn execute_with_timeout(process: &mut Command) -> Result<CommandRun, String> {
    configure_child_process(process);
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
    let deadline = Instant::now() + Duration::from_secs(COMMAND_TIMEOUT_SECS);

    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            let stdout = stdout_handle
                .join()
                .unwrap_or_else(|_| Vec::from("stdout reader panicked".as_bytes()));
            let stderr = stderr_handle
                .join()
                .unwrap_or_else(|_| Vec::from("stderr reader panicked".as_bytes()));
            return Ok(CommandRun {
                status,
                stdout,
                stderr,
            });
        }

        if Instant::now() >= deadline {
            kill_child_tree(&mut child);
            let _status = child.wait().map_err(|e| e.to_string())?;
            let _ = stdout_handle.join();
            let _ = stderr_handle.join();
            return Err(format!(
                "tool watchdog timed out after {}s while waiting for command",
                COMMAND_TIMEOUT_SECS
            ));
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn os_sandbox_required_error(detail: &str) -> String {
    format!(
        "OS sandbox is required and no portable fallback is enabled. \
         Install/fix bubblewrap or set WIRECLI_ALLOW_PORTABLE_SANDBOX=1 for an explicit degraded run. Detail: {detail}"
    )
}

fn configure_child_process(process: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            process.pre_exec(|| {
                let _ = setpgid(0, 0);
                set_limit(RLIMIT_CPU, COMMAND_TIMEOUT_SECS, COMMAND_TIMEOUT_SECS);
                set_limit(RLIMIT_AS, 4 * 1024 * 1024 * 1024, 4 * 1024 * 1024 * 1024);
                set_limit(RLIMIT_NPROC, 256, 256);
                Ok(())
            });
        }
    }
}

fn kill_child_tree(child: &mut std::process::Child) {
    #[cfg(target_os = "linux")]
    unsafe {
        let pid = child.id() as i32;
        if pid > 0 {
            let _ = kill(-pid, SIGKILL);
        }
    }
    let _ = child.kill();
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct RLimit {
    rlim_cur: u64,
    rlim_max: u64,
}

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn setpgid(pid: i32, pgid: i32) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
    fn setrlimit(resource: i32, rlim: *const RLimit) -> i32;
}

#[cfg(target_os = "linux")]
const SIGKILL: i32 = 9;
#[cfg(target_os = "linux")]
const RLIMIT_CPU: i32 = 0;
#[cfg(target_os = "linux")]
const RLIMIT_NPROC: i32 = 6;
#[cfg(target_os = "linux")]
const RLIMIT_AS: i32 = 9;

#[cfg(target_os = "linux")]
fn set_limit(resource: i32, soft: u64, hard: u64) {
    let limit = RLimit {
        rlim_cur: soft,
        rlim_max: hard,
    };
    unsafe {
        let _ = setrlimit(resource, &limit);
    }
}

fn read_all(mut reader: impl Read + Send + 'static) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = reader.read_to_end(&mut buffer);
    buffer
}

pub(crate) fn replay_output(stdout: &[u8], stderr: &[u8]) {
    if !stdout.is_empty() {
        let _ = std::io::stdout().write_all(stdout);
    }
    if !stderr.is_empty() {
        let _ = std::io::stderr().write_all(stderr);
    }
}

pub(crate) fn status_from_code(code: Option<i32>) -> ExitStatus {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        return match code {
            Some(code) => ExitStatusExt::from_raw(code << 8),
            None => ExitStatusExt::from_raw(1 << 8),
        };
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::ExitStatusExt;
        return match code {
            Some(code) => ExitStatusExt::from_raw(code as u32),
            None => ExitStatusExt::from_raw(1),
        };
    }
}

pub(crate) fn shared_scheduler() -> Arc<BoxScheduler> {
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

#[cfg(target_os = "linux")]
fn bind_host_paths(process: &mut Command) {
    for path in ["/usr", "/bin", "/lib", "/lib64", "/sbin"] {
        let p = Path::new(path);
        if p.exists() {
            process.arg("--ro-bind").arg(p).arg(path);
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
