use crate::agent_tools::BOX_TOOL_NAMES;
use crate::config::AppPaths;
use crate::wire_memory::WireMemoryBundle;
use std::fs;
use std::path::Path;

const WIRE_AGENT_PROMPT: &str = include_str!("../prompts/wire-agent.md");
const WIRE_SECURITY_PROMPT: &str = include_str!("../prompts/security-and-runtime.md");

pub fn base_developer_prompt(paths: &AppPaths) -> String {
    let tools = BOX_TOOL_NAMES
        .iter()
        .map(|tool| format!("- {tool}"))
        .collect::<Vec<_>>()
        .join("\n");
    let repo_facts = repository_grounding_facts(&paths.root_dir);
    let wire_memory = WireMemoryBundle::load(&paths.root_dir);

    let mut prompt = prompt_from_modules(&tools, &repo_facts);

    if let Some(instructions) = wire_memory.render_developer_context() {
        prompt.push_str("\nProject instructions loaded from the repository:\n");
        prompt.push_str(&instructions);
        prompt.push('\n');
    }

    prompt
}

fn prompt_from_modules(tools: &str, repo_facts: &str) -> String {
    let tool_contract = tool_invocation_contract();
    format!(
        "{WIRE_AGENT_PROMPT}\n\
         \n\
         {WIRE_SECURITY_PROMPT}\n\
         \n\
         Available tools:\n{tools}\n\
         \n\
         Tool invocation contract:\n{tool_contract}\n\
         \n\
         Repository facts observed before this turn:\n{repo_facts}\n\
         \n\
         Tool rules:\n\
         - `shell` runs project-local commands inside the Box project tree. Pass argv directly; do not use `bash -c`, `sh -c`, or shell wrapper commands. Regex and metacharacters inside argv literals are allowed; do not build shell pipelines or redirections as command strings.\n\
         - `git` and `gh` run git and GitHub CLI operations inside the Box.\n\
         - `apply_patch` edits files inside the Box and is the preferred tool for modifying existing files.\n\
         - For inspection, prefer `search`, `grep_lines`, `read_file`, `read_lines`, `glob_files`, `head_lines`, and `tail_lines` over shell commands.\n\
         - Paths may be project-relative from the repository root, current-directory-relative, or `/workspace/...` when they stay inside the Box.\n\
         - Do not treat technology names like Next.js, React, Claude, OpenAI, Qwen, or Google as local file paths unless the user used `@...`, a previous tool listed that exact path, or the request explicitly says to open that file.\n\
         - `read_lines` can be called with only `path`; add `start_line`, `end_line`, or `count` only when you need a specific range.\n\
         - `plan` creates the visible plan for non-trivial work.\n\
         - `subagent` runs specialized scoped analysis workers. It does not reduce your own ability to use tools, send messages, or continue the task; use its report as evidence.\n\
         - `update_plan` only reports plan state; it does not edit files or execute commands.\n\
         - `remember` and `recall` operate on Anchor memory scoped to the project.\n\
         - `session_remember` and `session_recall` operate on temporary session memory scoped to the current chat session.\n\
         - AFUP means Adaptive Framework for User Patterns. Use `lab_learn` and `lab_recall` only for durable user patterns, repeated preferences, and workflow adaptation.\n\
         - If Wire surfaces `memory.suggestion`, ask/confirm before saving durable memory unless the user explicitly requested saving.\n\
         - FCM means Flash Cache Memory. Treat injected FCM context as fast project-local cache evidence from `.wci/mm.fcm`; verify risky or stale facts with tools.\n\
         - `skill_list`, `skill_read`, and `skill_create` operate on local Wire skills only.\n\
         - If Wire surfaces `skill.suggestion`, consider creating a skill only after the workflow is clear and reusable.\n\
         - `mcp_list` lists configured MCP servers and tools.\n\
         - Use local skills under `~/.wirecli/skills` and MCP servers declared in `~/.wirecli/config/config.toml`.\n\
         - WIRE.md is the preferred project memory file. AGENTS.md remains supported for compatibility. Apply path/type-specific WIRE rules when they match the files involved.\n\
         - If a Context7 MCP server is available, use it for library, framework, SDK, and API research before guessing at version-sensitive details.\n"
    )
}

fn tool_invocation_contract() -> String {
    let mut out = String::new();
    out.push_str(
        "- Call tools by their exact bare schema names only. Do not prefix tool names with `Box.`, `Wire.`, `tools.`, provider names, namespaces, or visual group labels.\n",
    );
    out.push_str(
        "- The UI may describe project-local execution as Box, but Box is not part of any function name. Example: call `list_dir`, never `Box.list_dir`.\n",
    );
    out.push_str("- Valid built-in tool names are exactly: ");
    out.push_str(&BOX_TOOL_NAMES.join(", "));
    out.push_str(".\n");
    out.push_str("- Inspect and navigate: list_dir, navigate, read_file, read_lines, grep_lines, head_lines, tail_lines, glob_files, search.\n");
    out.push_str(
        "- Edit: apply_patch, write_file, replace_in_file, delete_file, copy_file, move_file.\n",
    );
    out.push_str("- Run commands: shell, git, gh.\n");
    out.push_str("- Plan, subagents, and review: plan, update_plan, subagent, review.\n");
    out.push_str("- Memory and learning: remember, recall, session_remember, session_recall, lab_learn, lab_recall. AFUP learning is exposed through the lab tools; FCM cache context is injected automatically and is not a callable tool.\n");
    out.push_str(
        "- Skills and MCP: skill_list, skill_read, skill_create, mcp_list. Dynamic MCP tools must be called by the exact discovered `mcp__server__tool` name from `mcp_list`; never invent or rename MCP tools.\n",
    );
    out.push_str(
        "- If a tool call fails as unknown, retry with a real name from the built-in list or from `mcp_list` instead of continuing with the same invalid name.\n",
    );
    out
}

fn repository_grounding_facts(root: &Path) -> String {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(err) => {
            return format!("Top-level entries unavailable: {err}");
        }
    };

    let mut names = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(mut name) = file_name.to_str().map(|value| value.to_string()) else {
            continue;
        };
        if entry.path().is_dir() {
            name.push('/');
        }
        names.push(name);
    }
    names.sort();

    const LIMIT: usize = 40;
    let truncated = names.len() > LIMIT;
    let mut out = String::from("Top-level entries observed at prompt build:\n");
    if names.is_empty() {
        out.push_str("- none\n");
    } else {
        for name in names.iter().take(LIMIT) {
            out.push_str("- ");
            out.push_str(name);
            out.push('\n');
        }
        if truncated {
            out.push_str("- ... truncated\n");
        }
    }
    out.push_str(
        "Treat this as startup evidence only; inspect with tools before relying on deeper paths.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::base_developer_prompt;
    use crate::config::AppPaths;
    use std::path::PathBuf;

    #[test]
    fn prompt_mentions_box_and_tools() {
        let prompt = base_developer_prompt(&AppPaths {
            root_dir: PathBuf::from("."),
            project_key: "test".to_string(),
            wire_dir: PathBuf::from("."),
            config_dir: PathBuf::from("."),
            config_file: PathBuf::from("."),
            secret_key_file: PathBuf::from("."),
            theme_file: PathBuf::from("."),
            mcp_file: PathBuf::from("."),
            data_dir: PathBuf::from("."),
            history_db: PathBuf::from("."),
            anchor_db: PathBuf::from("."),
            hooks_file: PathBuf::from("."),
            memory_context_file: PathBuf::from("."),
            sandboxes_dir: PathBuf::from("."),
        });
        assert!(prompt.contains("Box"));
        assert!(prompt.contains("apply_patch"));
        assert!(prompt.contains("closed set"));
        assert!(prompt.contains("Do not invent aliases"));
        assert!(prompt.contains("Tool invocation contract"));
        assert!(prompt.contains("never `Box.list_dir`"));
        assert!(prompt.contains("Valid built-in tool names are exactly"));
        assert!(prompt.contains("Repository facts observed"));
    }
}
