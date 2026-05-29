#[derive(Debug, Clone)]
pub struct CommandPolicy {
    deny_prefixes: Vec<String>,
    deny_exact: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct PolicyViolation {
    pub command: String,
    pub reason: String,
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
            ],
        }
    }

    pub fn validate(&self, command: &[String]) -> Result<(), PolicyViolation> {
        let Some(program) = command.first() else {
            return Err(PolicyViolation {
                command: String::new(),
                reason: "missing command".to_string(),
            });
        };

        let program_lower = program.to_ascii_lowercase();
        if self.deny_exact.iter().any(|item| item == &program_lower) {
            return Err(PolicyViolation {
                command: program.clone(),
                reason: "privileged command is blocked inside Box".to_string(),
            });
        }

        if self
            .deny_prefixes
            .iter()
            .any(|prefix| program_lower.starts_with(prefix))
        {
            return Err(PolicyViolation {
                command: program.clone(),
                reason: "privileged command is blocked inside Box".to_string(),
            });
        }

        if command
            .iter()
            .skip(1)
            .any(|arg| matches!(arg.as_str(), "sudo" | "su" | "doas" | "pkexec"))
        {
            return Err(PolicyViolation {
                command: program.clone(),
                reason: "privileged subcommand is blocked inside Box".to_string(),
            });
        }

        Ok(())
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
}
