use crate::config::AppPaths;
use crate::mcp::McpRegistry;
use crate::skills::SkillStore;

#[derive(Debug, Clone, Default)]
pub struct StartupReport {
    pub skills: usize,
    pub skill_names: Vec<String>,
    pub mcp_servers: usize,
    pub mcp_server_names: Vec<String>,
    pub warnings: Vec<String>,
}

impl StartupReport {
    pub fn notice(&self) -> Option<String> {
        if self.warnings.is_empty() {
            return None;
        }
        Some(format!("startup warnings: {}", self.warnings.join(" · ")))
    }

    pub fn inventory_notice(&self) -> Option<String> {
        if self.skills == 0 && self.mcp_servers == 0 {
            return None;
        }

        let mut parts = Vec::new();
        if self.skills > 0 {
            parts.push(format_inventory("skill", self.skills, &self.skill_names));
        }
        if self.mcp_servers > 0 {
            parts.push(format_inventory(
                "MCP server",
                self.mcp_servers,
                &self.mcp_server_names,
            ));
        }
        Some(format!(
            "startup loaded {}; use @ to mention files, skills, and MCP servers",
            parts.join(" · ")
        ))
    }
}

pub fn bootstrap(paths: &AppPaths) -> StartupReport {
    let mut report = StartupReport::default();

    match SkillStore::new(paths).and_then(|store| store.list()) {
        Ok(skills) => {
            report.skills = skills.len();
            report.skill_names = skills.into_iter().map(|skill| skill.name).collect();
        }
        Err(err) => report
            .warnings
            .push(format!("skills unavailable: {}", compact(&err, 160))),
    }

    match McpRegistry::load(paths) {
        Ok(registry) => {
            report.mcp_servers = registry.servers().len();
            report.mcp_server_names = registry
                .servers()
                .iter()
                .map(|server| server.name.clone())
                .collect();
        }
        Err(err) => report
            .warnings
            .push(format!("mcp config unavailable: {}", compact(&err, 160))),
    }

    report
}

fn compact(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    out.push('…');
    out
}

fn format_inventory(kind: &str, count: usize, names: &[String]) -> String {
    let suffix = if count == 1 { "" } else { "s" };
    let mut out = format!("{count} {kind}{suffix}");
    let preview = names
        .iter()
        .take(3)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if !preview.is_empty() {
        out.push_str(" (");
        out.push_str(&preview);
        if count > 3 {
            out.push_str(", ...");
        }
        out.push(')');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::StartupReport;

    #[test]
    fn inventory_notice_lists_loaded_skills_and_mcp_servers() {
        let report = StartupReport {
            skills: 2,
            skill_names: vec!["rust-review".to_string(), "openai-docs".to_string()],
            mcp_servers: 1,
            mcp_server_names: vec!["context7".to_string()],
            warnings: Vec::new(),
        };

        let notice = report.inventory_notice().unwrap();

        assert!(notice.contains("2 skills (rust-review, openai-docs)"));
        assert!(notice.contains("1 MCP server (context7)"));
        assert!(notice.contains("use @ to mention"));
    }
}
