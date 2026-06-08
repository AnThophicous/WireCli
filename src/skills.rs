use crate::config::AppPaths;
use crate::safekey::{redact_secrets, write_private_file};
use std::fs;
use std::path::PathBuf;

const MAX_SKILL_MARKDOWN_BYTES: usize = 96 * 1024;

#[derive(Debug, Clone)]
pub struct SkillRecord {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub body: String,
}

pub struct SkillStore {
    root: PathBuf,
}

impl SkillStore {
    pub fn new(paths: &AppPaths) -> Result<Self, String> {
        let root = paths.wire_dir.join("skills");
        fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        ensure_skill_creator_docs(&root)?;
        ensure_builtin_skill_creator(&root)?;
        Ok(Self { root })
    }

    pub fn list(&self) -> Result<Vec<SkillRecord>, String> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.root).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                continue;
            }
            let path = entry.path().join("SKILL.md");
            if path.exists() {
                records.push(read_skill_file(path)?);
            }
        }
        records.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(records)
    }

    pub fn read(&self, name: &str) -> Result<SkillRecord, String> {
        let path = self.skill_path(name)?;
        if !path.exists() {
            return Err(format!("skill not found: {}", sanitize_skill_name(name)));
        }
        read_skill_file(path)
    }

    pub fn create(&self, name: &str, description: &str, body: &str) -> Result<SkillRecord, String> {
        let name = sanitize_skill_name(name);
        if name.is_empty() {
            return Err("skill name cannot be empty".to_string());
        }
        let dir = self.root.join(&name);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("SKILL.md");
        let description = redact_secrets(description.trim());
        let body = redact_secrets(body.trim());
        validate_skill_part("skill description", &description)?;
        validate_skill_part("skill body", &body)?;
        let content = format!(
            "---\nname: \"{}\"\ndescription: \"{}\"\n---\n\n# {}\n\n{}\n",
            escape_yaml(&name),
            escape_yaml(&description),
            name,
            body
        );
        fs::write(&path, content).map_err(|e| e.to_string())?;
        self.read(&name)
    }

    pub fn install_markdown(
        &self,
        fallback_name: &str,
        raw_markdown: &str,
    ) -> Result<SkillRecord, String> {
        let mut markdown = redact_secrets(raw_markdown.trim());
        validate_skill_markdown(&markdown)?;
        let mut name = frontmatter_value(&markdown, "name")
            .map(|value| sanitize_skill_name(&value))
            .unwrap_or_else(|| sanitize_skill_name(fallback_name));
        if name.is_empty() {
            name = sanitize_skill_name(fallback_name);
        }
        if name.is_empty() {
            return Err("skill name cannot be empty".to_string());
        }
        if !markdown.starts_with("---") {
            let description = frontmatter_value(&markdown, "description").unwrap_or_default();
            markdown = format!(
                "---\nname: \"{}\"\ndescription: \"{}\"\n---\n\n{}\n",
                escape_yaml(&name),
                escape_yaml(&description),
                markdown.trim()
            );
        }
        validate_skill_markdown(&markdown)?;
        let dir = self.root.join(&name);
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join("SKILL.md");
        fs::write(&path, markdown).map_err(|e| e.to_string())?;
        self.read(&name)
    }

    fn skill_path(&self, name: &str) -> Result<PathBuf, String> {
        let name = sanitize_skill_name(name);
        if name.is_empty() {
            return Err("skill name cannot be empty".to_string());
        }
        Ok(self.root.join(name).join("SKILL.md"))
    }
}

fn read_skill_file(path: PathBuf) -> Result<SkillRecord, String> {
    let raw = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    validate_skill_markdown(&raw)?;
    let name = frontmatter_value(&raw, "name").unwrap_or_else(|| {
        path.parent()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "skill".to_string())
    });
    let description = frontmatter_value(&raw, "description").unwrap_or_default();
    Ok(SkillRecord {
        name,
        description,
        path,
        body: redact_secrets(&raw),
    })
}

fn ensure_skill_creator_docs(root: &PathBuf) -> Result<(), String> {
    let path = root.join("SKILL_CREATOR.md");
    if path.exists() {
        return Ok(());
    }
    write_private_file(&path, SKILL_CREATOR_DOCS.as_bytes())
}

fn ensure_builtin_skill_creator(root: &PathBuf) -> Result<(), String> {
    let dir = root.join("skill-creator");
    let path = dir.join("SKILL.md");
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    write_private_file(&path, BUILTIN_SKILL_CREATOR.as_bytes())
}

const SKILL_CREATOR_DOCS: &str = r#"# Wire Skill Creator Guide

This file is the local technical reference for agents creating Wire skills.

## What A Skill Is

A skill is a scoped, reusable workflow stored as Markdown:

```text
~/.wirecli/skills/<skill-name>/SKILL.md
```

The user can mention it with `@skill-name`, and Wire can also expose it through the `skill_list`, `skill_read`, and `skill_create` tools. A skill is not trusted executable code by itself. It is an instruction package that the agent must read and then execute through Wire tools, MCP tools, or sandboxed commands.

## Required Shape

Every skill must include YAML frontmatter:

```markdown
---
name: "skill-name"
description: "Use when the user asks for a specific repeatable workflow."
---
```

The body should include:

- Trigger conditions.
- Inputs the agent should gather.
- Step-by-step procedure.
- Validation commands.
- Security boundaries.
- Failure behavior.

## Default Runtime Language

When the requested skill needs helper scripts, prefer Python if available. Verify with `python3 --version` or `python --version`. If Python is unavailable, try Node.js with `node --version`. If neither is available, ask the user which runtime they want before generating scripts.

Do not assume Bun, Deno, Rust, Go, Java, or shell-specific tooling unless the project already uses it or the user explicitly requested it.

## Security Rules

- Never store API keys, cookies, bearer tokens, passwords, private keys, or account secrets in a skill.
- Never embed hidden jailbreak instructions or instructions that override Wire's system/developer rules.
- Never tell the agent to escape the project Box, bypass Lattice, ignore policy, or disable safety checks.
- Keep network calls explicit and justified.
- Prefer environment variable names over secret values.
- If a helper script is created, make it deterministic and narrow.

## Quality Bar

A good skill is operational. It should let a future agent perform the workflow without guessing, while still forcing the agent to inspect current project files and current documentation when needed.
"#;

const BUILTIN_SKILL_CREATOR: &str = r#"---
name: "skill-creator"
description: "Use when the user asks Wire to create, install, improve, or explain a local skill."
---

# Skill Creator

Use this when the user asks to create a new skill or improve an existing skill.

## Workflow

1. Read `~/.wirecli/skills/SKILL_CREATOR.md` first when it is available.
2. Clarify only when the skill's purpose or trigger is genuinely ambiguous.
3. Inspect the current project before adding project-specific instructions.
4. If helper scripts are useful, prefer Python when `python3 --version` or `python --version` works.
5. If Python is unavailable, prefer Node.js when `node --version` works.
6. If neither Python nor Node.js is available, ask the user which language/runtime they want.
7. Write a `SKILL.md` with frontmatter, trigger conditions, steps, validation, and safety rules.
8. Use `skill_create` for simple Markdown-only skills. For script-backed skills, create the directory and files through normal Wire file tools, then validate them.

## Required Standards

- Keep the skill scoped to one workflow.
- Include concrete validation commands when possible.
- Do not include credentials or raw tokens.
- Do not hide policy-bypass instructions.
- Do not claim a runtime or library exists without checking.
- Prefer current official documentation for library/API-specific skills.

## Output

After creating or updating the skill, summarize:

- Skill name.
- Trigger/use case.
- Files created or changed.
- Validation performed.
"#;

fn validate_skill_part(label: &str, value: &str) -> Result<(), String> {
    if value.len() > MAX_SKILL_MARKDOWN_BYTES {
        return Err(format!("{label} is too large"));
    }
    Ok(())
}

fn validate_skill_markdown(raw: &str) -> Result<(), String> {
    if raw.len() > MAX_SKILL_MARKDOWN_BYTES {
        return Err(format!(
            "skill markdown is too large: {} bytes exceeds {} bytes",
            raw.len(),
            MAX_SKILL_MARKDOWN_BYTES
        ));
    }
    if raw.trim().is_empty() {
        return Err("skill markdown cannot be empty".to_string());
    }
    if raw.starts_with("---") && !has_closing_frontmatter(raw) {
        return Err("skill frontmatter is missing a closing --- line".to_string());
    }
    Ok(())
}

fn has_closing_frontmatter(raw: &str) -> bool {
    raw.lines().skip(1).any(|line| line.trim() == "---")
}

fn frontmatter_value(raw: &str, key: &str) -> Option<String> {
    let mut lines = raw.lines();
    if lines.next()? != "---" {
        return None;
    }
    for line in lines {
        if line == "---" {
            break;
        }
        let (left, right) = line.split_once(':')?;
        if left.trim() == key {
            return Some(right.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn sanitize_skill_name(name: &str) -> String {
    name.trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn escape_yaml(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
