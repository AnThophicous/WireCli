use crate::config::AppPaths;
use crate::id::next_id;
use crate::policy::{CommandAssessment, CommandDecision};
use crate::safekey::redact_secrets;
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ApprovalFile {
    #[serde(default = "default_version")]
    v: u32,
    #[serde(default)]
    records: Vec<ApprovalRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    AllowOnce,
    AllowRepo,
    Denied,
    DenyAlways,
    Used,
}

impl ApprovalState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::AllowOnce => "allow_once",
            Self::AllowRepo => "allow_repo",
            Self::Denied => "denied",
            Self::DenyAlways => "deny_always",
            Self::Used => "used",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub id: String,
    pub project_key: String,
    pub command_key: String,
    pub command: Vec<String>,
    pub reason: String,
    pub risk: String,
    pub explanation: String,
    pub state: ApprovalState,
    pub created_at: i64,
    #[serde(default)]
    pub decided_at: Option<i64>,
    #[serde(default)]
    pub used_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct ApprovalResolution {
    pub network_access: bool,
}

pub struct ApprovalStore {
    path: PathBuf,
    audit_path: PathBuf,
    project_key: String,
}

impl ApprovalStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        fs::create_dir_all(&paths.data_dir).map_err(|e| e.to_string())?;
        let path = paths.data_dir.join("approvals.json");
        let audit_path = paths.data_dir.join("approval_audit.jsonl");
        if !path.exists() {
            fs::write(&path, "{\n  \"v\": 1,\n  \"records\": []\n}\n")
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            path,
            audit_path,
            project_key: paths.project_key.clone(),
        })
    }

    pub fn list(&self) -> Result<Vec<ApprovalRecord>, String> {
        let mut records = self.load()?.records;
        records.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(records)
    }

    pub fn decide(&self, id: &str, state: ApprovalState) -> Result<ApprovalRecord, String> {
        if !matches!(
            state,
            ApprovalState::AllowOnce
                | ApprovalState::AllowRepo
                | ApprovalState::Denied
                | ApprovalState::DenyAlways
        ) {
            return Err("invalid approval decision".to_string());
        }
        let mut file = self.load()?;
        let now = now_ts();
        let Some(record) = file
            .records
            .iter_mut()
            .find(|record| record.id == id && record.project_key == self.project_key)
        else {
            return Err(format!("approval request not found: {id}"));
        };
        record.state = state;
        record.decided_at = Some(now);
        let record = record.clone();
        self.save(&file)?;
        self.append_audit("decision", &record, None)?;
        Ok(record)
    }

    pub fn ensure_command_approved(
        &self,
        command: &[String],
        reason: &str,
        assessment: &CommandAssessment,
    ) -> Result<ApprovalResolution, String> {
        match assessment.decision {
            CommandDecision::Allow => {
                self.append_allowed_audit(command, reason, assessment)?;
                return Ok(ApprovalResolution {
                    network_access: false,
                });
            }
            CommandDecision::Deny => {
                self.append_raw_audit("deny", command, reason, assessment)?;
                return Err(format!(
                    "blocked by Lattice: {}: {}",
                    assessment.command, assessment.explanation
                ));
            }
            CommandDecision::Ask => {}
        }

        let mut file = self.load()?;
        let key = command_key(command);
        if let Some(record) = file.records.iter().find(|record| {
            record.project_key == self.project_key
                && record.command_key == key
                && record.state == ApprovalState::DenyAlways
        }) {
            self.append_audit("deny_always_match", record, Some(assessment))?;
            return Err(format!(
                "approval denied by repo policy: {} ({})",
                record.explanation, record.id
            ));
        }

        if let Some(record) = file.records.iter().find(|record| {
            record.project_key == self.project_key
                && record.command_key == key
                && record.state == ApprovalState::AllowRepo
        }) {
            self.append_audit("allow_repo_match", record, Some(assessment))?;
            return Ok(ApprovalResolution {
                network_access: assessment.needs_network,
            });
        }

        let now = now_ts();
        if let Some(index) = file.records.iter().position(|record| {
            record.project_key == self.project_key
                && record.command_key == key
                && record.state == ApprovalState::AllowOnce
        }) {
            file.records[index].state = ApprovalState::Used;
            file.records[index].used_at = Some(now);
            let record = file.records[index].clone();
            self.save(&file)?;
            self.append_audit("allow_once_used", &record, Some(assessment))?;
            return Ok(ApprovalResolution {
                network_access: assessment.needs_network,
            });
        }

        let pending = match file.records.iter().find(|record| {
            record.project_key == self.project_key
                && record.command_key == key
                && record.state == ApprovalState::Pending
        }) {
            Some(record) => record.clone(),
            None => {
                let record = ApprovalRecord {
                    id: next_id(),
                    project_key: self.project_key.clone(),
                    command_key: key,
                    command: command.iter().map(|part| redact_secrets(part)).collect(),
                    reason: redact_secrets(reason),
                    risk: assessment.risk.clone(),
                    explanation: assessment.explanation.clone(),
                    state: ApprovalState::Pending,
                    created_at: now,
                    decided_at: None,
                    used_at: None,
                };
                file.records.push(record.clone());
                self.save(&file)?;
                self.append_audit("request", &record, Some(assessment))?;
                record
            }
        };

        Err(format!(
            "approval required: {}\n\
             risk: {}\n\
             command: {}\n\
             request: {}\n\
             allow once: wirecli approvals allow-once {}\n\
             allow in this repo: wirecli approvals allow-repo {}\n\
             deny always: wirecli approvals deny-always {}",
            pending.explanation,
            pending.risk,
            pending.command.join(" "),
            pending.id,
            pending.id,
            pending.id,
            pending.id
        ))
    }

    fn load(&self) -> Result<ApprovalFile, String> {
        let raw = fs::read_to_string(&self.path).map_err(|e| e.to_string())?;
        let mut file: ApprovalFile = serde_json::from_str(&raw).unwrap_or_default();
        if file.v == 0 {
            file.v = 1;
        }
        Ok(file)
    }

    fn save(&self, file: &ApprovalFile) -> Result<(), String> {
        let raw = serde_json::to_string_pretty(file).map_err(|e| e.to_string())?;
        fs::write(&self.path, raw).map_err(|e| e.to_string())
    }

    fn append_allowed_audit(
        &self,
        command: &[String],
        reason: &str,
        assessment: &CommandAssessment,
    ) -> Result<(), String> {
        let record = ApprovalRecord {
            id: "policy".to_string(),
            project_key: self.project_key.clone(),
            command_key: command_key(command),
            command: command.iter().map(|part| redact_secrets(part)).collect(),
            reason: redact_secrets(reason),
            risk: assessment.risk.clone(),
            explanation: assessment.explanation.clone(),
            state: ApprovalState::Used,
            created_at: now_ts(),
            decided_at: None,
            used_at: Some(now_ts()),
        };
        self.append_audit("allow_policy", &record, Some(assessment))
    }

    fn append_raw_audit(
        &self,
        event: &str,
        command: &[String],
        reason: &str,
        assessment: &CommandAssessment,
    ) -> Result<(), String> {
        let record = ApprovalRecord {
            id: "policy".to_string(),
            project_key: self.project_key.clone(),
            command_key: command_key(command),
            command: command.iter().map(|part| redact_secrets(part)).collect(),
            reason: redact_secrets(reason),
            risk: assessment.risk.clone(),
            explanation: assessment.explanation.clone(),
            state: ApprovalState::Denied,
            created_at: now_ts(),
            decided_at: None,
            used_at: None,
        };
        self.append_audit(event, &record, Some(assessment))
    }

    fn append_audit(
        &self,
        event: &str,
        record: &ApprovalRecord,
        assessment: Option<&CommandAssessment>,
    ) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_path)
            .map_err(|e| e.to_string())?;
        let value = serde_json::json!({
            "ts": now_ts(),
            "event": event,
            "id": record.id,
            "project_key": record.project_key,
            "command": record.command,
            "reason": record.reason,
            "risk": record.risk,
            "state": record.state.as_str(),
            "needs_network": assessment.map(|value| value.needs_network).unwrap_or(false),
        });
        writeln!(file, "{value}").map_err(|e| e.to_string())
    }
}

fn command_key(command: &[String]) -> String {
    command
        .iter()
        .map(|part| redact_secrets(part.trim()))
        .collect::<Vec<_>>()
        .join("\u{1f}")
}

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn default_version() -> u32 {
    1
}

#[cfg(test)]
mod tests {
    use super::{ApprovalState, ApprovalStore};
    use crate::config::AppPaths;
    use crate::policy::CommandPolicy;
    use std::path::PathBuf;

    #[test]
    fn approval_once_is_consumed() {
        let root =
            std::env::temp_dir().join(format!("wirecli-approval-test-{}", crate::id::next_id()));
        std::fs::create_dir_all(root.join("data")).unwrap();
        let paths = test_paths(&root);
        let store = ApprovalStore::new(&paths).unwrap();
        let command = vec!["curl".to_string(), "https://example.com".to_string()];
        let assessment = CommandPolicy::standard().assess_for_workspace(&command, &root);
        let err = store
            .ensure_command_approved(&command, "test", &assessment)
            .unwrap_err();
        let id = err
            .lines()
            .find_map(|line| line.strip_prefix("request: "))
            .unwrap()
            .to_string();
        store.decide(&id, ApprovalState::AllowOnce).unwrap();
        store
            .ensure_command_approved(&command, "test", &assessment)
            .unwrap();
        let err = store
            .ensure_command_approved(&command, "test", &assessment)
            .unwrap_err();
        assert!(err.contains("approval required"));
        let _ = std::fs::remove_dir_all(root);
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
        AppPaths {
            root_dir: root.to_path_buf(),
            project_key: root.display().to_string(),
            wire_dir: root.to_path_buf(),
            config_dir: root.to_path_buf(),
            config_file: root.join("config.toml"),
            secret_key_file: root.join("secret.key"),
            theme_file: root.join("theme.yaml"),
            mcp_file: root.join("mcp.json"),
            data_dir: root.join("data"),
            history_db: root.join("history.sqlite3"),
            anchor_db: root.join("anchor.sqlite3"),
            hooks_file: root.join("hooks.json"),
            memory_context_file: root.join("memory.json"),
            sandboxes_dir: PathBuf::from(root).join("boxes"),
        }
    }
}
