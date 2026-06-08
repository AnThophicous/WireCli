use std::process::ExitStatus;

#[derive(Debug, Clone)]
pub struct SandboxSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct CommandResult {
    pub status_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct SandboxRunOptions {
    pub network_access: bool,
    pub allow_portable_fallback: bool,
}

impl Default for SandboxRunOptions {
    fn default() -> Self {
        Self {
            network_access: false,
            allow_portable_fallback: std::env::var("WIRECLI_ALLOW_PORTABLE_SANDBOX")
                .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
                .unwrap_or(false),
        }
    }
}

pub(crate) struct CommandRun {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
}

impl CommandRun {
    pub(crate) fn is_bwrap_namespace_error(&self) -> bool {
        let stderr = String::from_utf8_lossy(&self.stderr);
        stderr.contains("bwrap:")
            && (stderr.contains("NETLINK_ROUTE") || stderr.contains("Operation not permitted"))
    }
}
