use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

use crate::skill::SKILL_CONTENT;

/// Current package version for staleness checks.
const VERSION: &str = env!("CARGO_PKG_VERSION");

struct PlatformConfig {
    skill_dst: &'static str,
    register_claude_md: bool,
}

const PLATFORMS: &[(&str, PlatformConfig)] = &[
    (
        "claude",
        PlatformConfig {
            skill_dst: ".claude/skills/graphify-rs/SKILL.md",
            register_claude_md: true,
        },
    ),
    (
        "codex",
        PlatformConfig {
            skill_dst: ".agents/skills/graphify-rs/SKILL.md",
            register_claude_md: false,
        },
    ),
    (
        "opencode",
        PlatformConfig {
            skill_dst: ".config/opencode/skills/graphify-rs/SKILL.md",
            register_claude_md: false,
        },
    ),
    (
        "claw",
        PlatformConfig {
            skill_dst: ".claw/skills/graphify-rs/SKILL.md",
            register_claude_md: false,
        },
    ),
    (
        "droid",
        PlatformConfig {
            skill_dst: ".factory/skills/graphify-rs/SKILL.md",
            register_claude_md: false,
        },
    ),
    (
        "trae",
        PlatformConfig {
            skill_dst: ".trae/skills/graphify-rs/SKILL.md",
            register_claude_md: false,
        },
    ),
    (
        "trae-cn",
        PlatformConfig {
            skill_dst: ".trae-cn/skills/graphify-rs/SKILL.md",
            register_claude_md: false,
        },
    ),
    (
        "codebuddy",
        PlatformConfig {
            skill_dst: ".codebuddy/skills/graphify-rs/SKILL.md",
            register_claude_md: false,
        },
    ),
    (
        "copilot",
        PlatformConfig {
            skill_dst: ".config/github-copilot/skills/graphify-rs/SKILL.md",
            register_claude_md: false,
        },
    ),
    (
        "vscode",
        PlatformConfig {
            skill_dst: ".vscode/skills/graphify-rs/SKILL.md",
            register_claude_md: false,
        },
    ),
    (
        "windows",
        PlatformConfig {
            skill_dst: ".claude/skills/graphify-rs/SKILL.md",
            register_claude_md: true,
        },
    ),
];

const SKILL_REGISTRATION: &str = r#"
# graphify-rs
- **graphify-rs** (`~/.claude/skills/graphify-rs/SKILL.md`) - any input to knowledge graph. Trigger: `/graphify-rs`
When the user types `/graphify-rs`, invoke the Skill tool with `skill: "graphify-rs"` before doing anything else.
"#;

/// Render the output directory the way it should read inside a committed
/// instructions file: relative to the project root whenever it lives there.
///
/// `resolve_default_output` canonicalizes, so it hands back something like
/// `/Users/me/code/proj/graphify-rs-out`. Writing that into CLAUDE.md bakes
/// one machine's home directory into a file the whole team reads, and the
/// paths stop resolving for everyone else.
fn display_output_dir(project_root: &Path, output_dir: &str) -> String {
    let root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    Path::new(output_dir)
        .strip_prefix(&root)
        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| output_dir.to_string())
}

/// The instructions block written into CLAUDE.md / AGENTS.md and friends.
///
/// Ordering is the point. `query`/`path`/`explain` return a subgraph scoped to
/// the question, while GRAPH_REPORT.md is the whole map — 110 KB in this
/// repository. Naming the report first, as this block used to, sends agents to
/// the most expensive option before the cheap one.
fn graph_md_section(project_root: &Path, output_dir: &str) -> String {
    let dir = display_output_dir(project_root, output_dir);
    format!(
        r#"## graphify-rs

This project has a graphify-rs knowledge graph at {dir}/ with god nodes, community structure, and cross-file relationships.

Rules:
- For codebase questions, first run `graphify-rs query "<question>"` when {dir}/graph.json exists. Use `graphify-rs path "<A>" "<B>"` for relationships and `graphify-rs explain "<concept>"` for focused concepts. These return a scoped subgraph, usually much smaller than GRAPH_REPORT.md or raw grep output.
- If {dir}/wiki/index.md exists, use it for broad navigation instead of raw source browsing.
- Read {dir}/GRAPH_REPORT.md only for broad architecture review or when query/path/explain do not surface enough context.
- After modifying code, run `graphify-rs update .` to keep the graph current (AST-only, no API cost).
"#
    )
}

const CLAUDE_MD_MARKER: &str = "## graphify-rs";

const AGENTS_MD_MARKER: &str = "## graphify-rs";

const COPILOT_MD_MARKER: &str = "## graphify-rs";

const VSCODE_MD_MARKER: &str = "## graphify-rs";

const VSCODE_INSTRUCTIONS_FILE: &str = ".vscode/graphify-rs-instructions.md";

/// Check all known skill install locations for stale versions.
/// Call this on startup (before executing any subcommand).
pub fn check_skill_versions() {
    let home = match home_dir() {
        Ok(h) => h,
        Err(_) => return,
    };
    for (_, config) in PLATFORMS {
        let version_file = home
            .join(config.skill_dst)
            .parent()
            .map(|p| p.join(".graphify_rs_version"))
            .unwrap_or_default();
        if version_file.exists() {
            if let Ok(installed) = fs::read_to_string(&version_file) {
                let installed = installed.trim();
                if !installed.is_empty() && installed != VERSION {
                    eprintln!(
                        "  warning: skill is from graphify-rs {installed}, package is {VERSION}. Run 'graphify-rs install' to update."
                    );
                    return; // Only warn once
                }
            }
        }
    }
}

/// Install graphify skill file for a given platform (global install).
pub fn install_skill(platform: &str) -> Result<()> {
    let config = PLATFORMS
        .iter()
        .find(|(name, _)| *name == platform)
        .map(|(_, cfg)| cfg)
        .with_context(|| {
            let valid: Vec<&str> = PLATFORMS.iter().map(|(n, _)| *n).collect();
            format!(
                "Unknown platform '{}'. Valid platforms: {}",
                platform,
                valid.join(", ")
            )
        })?;

    let home = home_dir()?;
    let skill_path = home.join(config.skill_dst);

    if let Some(parent) = skill_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create directory {}", parent.display()))?;
    }

    fs::write(&skill_path, SKILL_CONTENT)
        .with_context(|| format!("Failed to write skill file to {}", skill_path.display()))?;
    println!("  Wrote skill file to {}", skill_path.display());

    if let Some(parent) = skill_path.parent() {
        let version_file = parent.join(".graphify_rs_version");
        let _ = fs::write(&version_file, VERSION);
    }

    if config.register_claude_md {
        let claude_md_path = home.join(".claude/CLAUDE.md");
        register_in_file(&claude_md_path, SKILL_REGISTRATION, "# graphify-rs")?;
        println!("  Registered in {}", claude_md_path.display());
    }

    println!("\n  Installed graphify-rs skill for '{platform}'.");
    println!("  Use `/graphify-rs` in your AI assistant to trigger the skill.");

    Ok(())
}

/// `graphify-rs claude install` — project-level Claude integration.
pub fn claude_install(project_root: &Path) -> Result<()> {
    let output_dir = crate::paths::resolve_default_output(project_root)
        .to_string_lossy()
        .to_string();
    let claude_md = project_root.join("CLAUDE.md");
    append_section(
        &claude_md,
        &graph_md_section(project_root, &output_dir),
        CLAUDE_MD_MARKER,
    )?;
    println!("  Updated {}", claude_md.display());

    let settings_path = project_root.join(".claude/settings.json");
    write_claude_settings_hook(&settings_path, &output_dir)?;
    println!("  Wrote hook to {}", settings_path.display());

    println!("\n  Claude integration installed.");
    Ok(())
}

/// `graphify-rs claude uninstall` — remove project-level Claude integration.
pub fn claude_uninstall(project_root: &Path) -> Result<()> {
    let claude_md = project_root.join("CLAUDE.md");
    remove_section(&claude_md, CLAUDE_MD_MARKER)?;
    println!("  Cleaned {}", claude_md.display());

    let settings_path = project_root.join(".claude/settings.json");
    remove_claude_settings_hook(&settings_path)?;
    println!("  Cleaned {}", settings_path.display());

    println!("\n  Claude integration uninstalled.");
    Ok(())
}

/// `graphify-rs codebuddy install` — project-level CodeBuddy integration.
pub fn codebuddy_install(project_root: &Path) -> Result<()> {
    let output_dir = crate::paths::resolve_default_output(project_root)
        .to_string_lossy()
        .to_string();
    let agents_md = project_root.join("AGENTS.md");
    append_section(
        &agents_md,
        &graph_md_section(project_root, &output_dir),
        AGENTS_MD_MARKER,
    )?;
    println!("  Updated {}", agents_md.display());

    let settings_path = project_root.join(".codebuddy/settings.json");
    write_codebuddy_settings_hook(&settings_path, &output_dir)?;
    println!("  Wrote hook to {}", settings_path.display());

    println!("\n  CodeBuddy integration installed.");
    Ok(())
}

/// `graphify-rs codebuddy uninstall` — remove project-level CodeBuddy integration.
pub fn codebuddy_uninstall(project_root: &Path) -> Result<()> {
    let agents_md = project_root.join("AGENTS.md");
    remove_section(&agents_md, AGENTS_MD_MARKER)?;
    println!("  Cleaned {}", agents_md.display());

    let settings_path = project_root.join(".codebuddy/settings.json");
    remove_codebuddy_settings_hook(&settings_path)?;
    println!("  Cleaned {}", settings_path.display());

    println!("\n  CodeBuddy integration uninstalled.");
    Ok(())
}

/// `graphify-rs codex install` — project-level Codex integration.
pub fn codex_install(project_root: &Path) -> Result<()> {
    let output_dir = crate::paths::resolve_default_output(project_root)
        .to_string_lossy()
        .to_string();
    let agents_md = project_root.join("AGENTS.md");
    append_section(
        &agents_md,
        &graph_md_section(project_root, &output_dir),
        AGENTS_MD_MARKER,
    )?;
    println!("  Updated {}", agents_md.display());

    let hooks_path = project_root.join(".codex/hooks.json");
    write_codex_hooks(&hooks_path, &output_dir)?;
    println!("  Wrote hook to {}", hooks_path.display());

    println!("\n  Codex integration installed.");
    Ok(())
}

/// `graphify-rs codex uninstall`
pub fn codex_uninstall(project_root: &Path) -> Result<()> {
    let agents_md = project_root.join("AGENTS.md");
    remove_section(&agents_md, AGENTS_MD_MARKER)?;
    println!("  Cleaned {}", agents_md.display());

    let hooks_path = project_root.join(".codex/hooks.json");
    if hooks_path.exists() {
        fs::remove_file(&hooks_path)?;
        println!("  Removed {}", hooks_path.display());
    }

    println!("\n  Codex integration uninstalled.");
    Ok(())
}

/// `graphify-rs opencode install` — project-level OpenCode integration.
pub fn opencode_install(project_root: &Path) -> Result<()> {
    let output_dir = crate::paths::resolve_default_output(project_root)
        .to_string_lossy()
        .to_string();
    let agents_md = project_root.join("AGENTS.md");
    append_section(
        &agents_md,
        &graph_md_section(project_root, &output_dir),
        AGENTS_MD_MARKER,
    )?;
    println!("  Updated {}", agents_md.display());

    let plugin_path = project_root.join(".opencode/plugins/graphify-rs.js");
    write_opencode_plugin(&plugin_path, &output_dir)?;
    println!("  Wrote plugin to {}", plugin_path.display());

    let config_path = project_root.join("opencode.json");
    register_opencode_config(&config_path)?;
    println!("  Updated {}", config_path.display());

    println!("\n  OpenCode integration installed.");
    Ok(())
}

/// `graphify-rs opencode uninstall`
pub fn opencode_uninstall(project_root: &Path) -> Result<()> {
    let agents_md = project_root.join("AGENTS.md");
    remove_section(&agents_md, AGENTS_MD_MARKER)?;
    println!("  Cleaned {}", agents_md.display());

    let plugin_path = project_root.join(".opencode/plugins/graphify-rs.js");
    if plugin_path.exists() {
        fs::remove_file(&plugin_path)?;
        println!("  Removed {}", plugin_path.display());
    }

    let config_path = project_root.join("opencode.json");
    unregister_opencode_config(&config_path)?;
    println!("  Cleaned {}", config_path.display());

    println!("\n  OpenCode integration uninstalled.");
    Ok(())
}

/// `graphify-rs copilot install` — writes graphify-rs section into
/// `.github/copilot-instructions.md`, the repo-wide instructions file that
/// GitHub Copilot Chat reads.
pub fn copilot_install(project_root: &Path) -> Result<()> {
    let output_dir = crate::paths::resolve_default_output(project_root)
        .to_string_lossy()
        .to_string();
    let instructions = project_root.join(".github/copilot-instructions.md");
    append_section(
        &instructions,
        &graph_md_section(project_root, &output_dir),
        COPILOT_MD_MARKER,
    )?;
    println!("  Updated {}", instructions.display());

    println!("\n  GitHub Copilot integration installed.");
    Ok(())
}

/// `graphify-rs copilot uninstall`
pub fn copilot_uninstall(project_root: &Path) -> Result<()> {
    let instructions = project_root.join(".github/copilot-instructions.md");
    remove_section(&instructions, COPILOT_MD_MARKER)?;
    println!("  Cleaned {}", instructions.display());

    println!("\n  GitHub Copilot integration uninstalled.");
    Ok(())
}

/// `graphify-rs vscode install` — writes a dedicated instructions file under
/// `.vscode/` and registers it with the VSCode Copilot Chat settings so it is
/// pulled into every prompt.
pub fn vscode_install(project_root: &Path) -> Result<()> {
    let output_dir = crate::paths::resolve_default_output(project_root)
        .to_string_lossy()
        .to_string();
    let instructions = project_root.join(VSCODE_INSTRUCTIONS_FILE);
    append_section(
        &instructions,
        &graph_md_section(project_root, &output_dir),
        VSCODE_MD_MARKER,
    )?;
    println!("  Updated {}", instructions.display());

    let settings_path = project_root.join(".vscode/settings.json");
    write_vscode_settings_hook(&settings_path)?;
    println!(
        "  Wrote instructions reference to {}",
        settings_path.display()
    );

    println!("\n  VSCode integration installed.");
    Ok(())
}

/// `graphify-rs vscode uninstall`
pub fn vscode_uninstall(project_root: &Path) -> Result<()> {
    let instructions = project_root.join(VSCODE_INSTRUCTIONS_FILE);
    if instructions.exists() {
        remove_section(&instructions, VSCODE_MD_MARKER)?;
        println!("  Cleaned {}", instructions.display());
    }

    let settings_path = project_root.join(".vscode/settings.json");
    remove_vscode_settings_hook(&settings_path)?;
    println!("  Cleaned {}", settings_path.display());

    println!("\n  VSCode integration uninstalled.");
    Ok(())
}

/// Generic platform install — just writes AGENTS.md section.
pub fn generic_platform_install(project_root: &Path, platform: &str) -> Result<()> {
    let output_dir = crate::paths::resolve_default_output(project_root)
        .to_string_lossy()
        .to_string();
    let agents_md = project_root.join("AGENTS.md");
    append_section(
        &agents_md,
        &graph_md_section(project_root, &output_dir),
        AGENTS_MD_MARKER,
    )?;
    println!("  Updated {}", agents_md.display());
    println!("\n  {platform} integration installed.");
    Ok(())
}

/// Generic platform uninstall — just removes AGENTS.md section.
pub fn generic_platform_uninstall(project_root: &Path, platform: &str) -> Result<()> {
    let agents_md = project_root.join("AGENTS.md");
    remove_section(&agents_md, AGENTS_MD_MARKER)?;
    println!("  Cleaned {}", agents_md.display());
    println!("\n  {platform} integration uninstalled.");
    Ok(())
}

fn home_dir() -> Result<std::path::PathBuf> {
    dirs::home_dir().context("Could not determine home directory")
}

/// Append a section to a file if the marker is not already present.
fn append_section(path: &Path, section: &str, marker: &str) -> Result<()> {
    let existing = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };

    if existing.contains(marker) {
        println!("  Section already present in {}, skipping.", path.display());
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push('\n');
    content.push_str(section);

    fs::write(path, content)?;
    Ok(())
}

/// Remove a section from a file identified by a marker line.
/// Removes from the marker line until the next `##` heading or end of file.
fn remove_section(path: &Path, marker: &str) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    if !content.contains(marker) {
        return Ok(());
    }

    let mut result = String::new();
    let mut skipping = false;

    for line in content.lines() {
        if line.starts_with(marker) {
            skipping = true;
            continue;
        }
        if skipping {
            if line.starts_with("## ") {
                skipping = false;
                result.push_str(line);
                result.push('\n');
            }
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }

    let trimmed = result.trim_end().to_string() + "\n";
    fs::write(path, trimmed)?;
    Ok(())
}

/// Register a skill reference in a file (like ~/.claude/CLAUDE.md).
fn register_in_file(path: &Path, registration_text: &str, marker: &str) -> Result<()> {
    let existing = if path.exists() {
        fs::read_to_string(path)?
    } else {
        String::new()
    };

    if existing.contains(marker) {
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(registration_text);

    fs::write(path, content)?;
    Ok(())
}

/// Write Claude PreToolUse hook to .claude/settings.json.
fn write_claude_settings_hook(path: &Path, output_dir: &str) -> Result<()> {
    let mut settings: serde_json::Value = if path.exists() {
        let content = fs::read_to_string(path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let cmd = format!(
        "[ -f \"{output_dir}/graph.json\" ] && echo '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"graphify-rs: Knowledge graph exists. Read {output_dir}/GRAPH_REPORT.md for god nodes and community structure before searching raw files.\"}}}}' || true"
    );
    let hook_entry = serde_json::json!({
        "matcher": "Glob|Grep",
        "hooks": [{
            "type": "command",
            "command": cmd
        }]
    });

    let hooks = settings
        .as_object_mut()
        .context("settings is not an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let pre_tool_use = hooks
        .as_object_mut()
        .context("hooks is not an object")?
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]));

    let arr = pre_tool_use
        .as_array_mut()
        .context("PreToolUse is not an array")?;

    let already = arr
        .iter()
        .any(|v| v.get("matcher").and_then(|m| m.as_str()) == Some("Glob|Grep"));
    if !already {
        arr.push(hook_entry);
    }

    let output = serde_json::to_string_pretty(&settings)?;
    fs::write(path, output)?;
    Ok(())
}

/// Remove Claude PreToolUse hook from .claude/settings.json.
fn remove_claude_settings_hook(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));

    if let Some(hooks) = settings.get_mut("hooks") {
        if let Some(pre_tool_use) = hooks.get_mut("PreToolUse") {
            if let Some(arr) = pre_tool_use.as_array_mut() {
                arr.retain(|v| v.get("matcher").and_then(|m| m.as_str()) != Some("Glob|Grep"));
            }
        }
    }

    let output = serde_json::to_string_pretty(&settings)?;
    fs::write(path, output)?;
    Ok(())
}

/// Write CodeBuddy PreToolUse hook to .codebuddy/settings.json.
fn write_codebuddy_settings_hook(path: &Path, output_dir: &str) -> Result<()> {
    let mut settings: serde_json::Value = if path.exists() {
        let content = fs::read_to_string(path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let cmd = format!(
        "[ -f \"{output_dir}/graph.json\" ] && echo '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"graphify-rs: Knowledge graph exists. Read {output_dir}/GRAPH_REPORT.md for god nodes and community structure before searching raw files.\"}}}}' || true"
    );
    let hook_entry = serde_json::json!({
        "matcher": "Glob|Grep",
        "hooks": [{
            "type": "command",
            "command": cmd
        }]
    });

    let hooks = settings
        .as_object_mut()
        .context("settings is not an object")?
        .entry("hooks")
        .or_insert_with(|| serde_json::json!({}));
    let pre_tool_use = hooks
        .as_object_mut()
        .context("hooks is not an object")?
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]));

    let arr = pre_tool_use
        .as_array_mut()
        .context("PreToolUse is not an array")?;

    let already = arr
        .iter()
        .any(|v| v.get("matcher").and_then(|m| m.as_str()) == Some("Glob|Grep"));
    if !already {
        arr.push(hook_entry);
    }

    let output = serde_json::to_string_pretty(&settings)?;
    fs::write(path, output)?;
    Ok(())
}

/// Remove CodeBuddy PreToolUse hook from .codebuddy/settings.json.
fn remove_codebuddy_settings_hook(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));

    if let Some(hooks) = settings.get_mut("hooks") {
        if let Some(pre_tool_use) = hooks.get_mut("PreToolUse") {
            if let Some(arr) = pre_tool_use.as_array_mut() {
                arr.retain(|v| v.get("matcher").and_then(|m| m.as_str()) != Some("Glob|Grep"));
            }
        }
    }

    let output = serde_json::to_string_pretty(&settings)?;
    fs::write(path, output)?;
    Ok(())
}

/// Write Codex hooks.json.
fn write_codex_hooks(path: &Path, output_dir: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let cmd = format!(
        "[ -f \"{output_dir}/graph.json\" ] && echo '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\",\"systemMessage\":\"graphify-rs: Knowledge graph exists. Read {output_dir}/GRAPH_REPORT.md for god nodes and community structure before searching raw files.\"}}}}' || true"
    );
    let hooks = serde_json::json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [{
                    "type": "command",
                    "command": cmd
                }]
            }]
        }
    });

    let output = serde_json::to_string_pretty(&hooks)?;
    fs::write(path, output)?;
    Ok(())
}

/// Write OpenCode plugin file.
fn write_opencode_plugin(path: &Path, output_dir: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let plugin_content = format!(
        r#"// graphify-rs plugin for OpenCode
module.exports = {{
    name: "graphify-rs",
    description: "Knowledge graph integration",
    hooks: {{
        preToolUse: async (ctx) => {{
            const fs = require("fs");
            if (fs.existsSync("{output_dir}/graph.json")) {{
                return {{
                    prefix: "[graphify-rs] Knowledge graph available. Read {output_dir}/GRAPH_REPORT.md for architecture overview."
                }};
            }}
        }}
    }}
}};
"#
    );

    fs::write(path, plugin_content)?;
    Ok(())
}

/// Register graphify-rs plugin in opencode.json.
fn register_opencode_config(path: &Path) -> Result<()> {
    let mut config: serde_json::Value = if path.exists() {
        let content = fs::read_to_string(path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    let plugins = config
        .as_object_mut()
        .context("config is not an object")?
        .entry("plugin")
        .or_insert_with(|| serde_json::json!([]));

    if let Some(arr) = plugins.as_array_mut() {
        let already = arr
            .iter()
            .any(|v| v.as_str() == Some(".opencode/plugins/graphify-rs.js"));
        if !already {
            arr.push(serde_json::json!(".opencode/plugins/graphify-rs.js"));
        }
    }

    let output = serde_json::to_string_pretty(&config)?;
    fs::write(path, output)?;
    Ok(())
}

/// Remove graphify-rs plugin from opencode.json.
fn unregister_opencode_config(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let mut config: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));

    if let Some(plugins) = config.get_mut("plugin") {
        if let Some(arr) = plugins.as_array_mut() {
            arr.retain(|v| v.as_str() != Some(".opencode/plugins/graphify-rs.js"));
        }
    }

    let output = serde_json::to_string_pretty(&config)?;
    fs::write(path, output)?;
    Ok(())
}

/// Register the graphify-rs instructions file with VSCode Copilot Chat.
///
/// VSCode Copilot reads `github.copilot.chat.codeGeneration.instructions` and
/// merges the referenced file into every prompt. We append (not replace) so an
/// existing user configuration is preserved.
fn write_vscode_settings_hook(path: &Path) -> Result<()> {
    let mut settings: serde_json::Value = if path.exists() {
        let content = fs::read_to_string(path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let entry = serde_json::json!({ "file": VSCODE_INSTRUCTIONS_FILE });
    let instructions = settings
        .as_object_mut()
        .context("settings is not an object")?
        .entry("github.copilot.chat.codeGeneration.instructions")
        .or_insert_with(|| serde_json::json!([]));

    let arr = instructions
        .as_array_mut()
        .context("github.copilot.chat.codeGeneration.instructions is not an array")?;

    let already = arr
        .iter()
        .any(|v| v.get("file").and_then(|f| f.as_str()) == Some(VSCODE_INSTRUCTIONS_FILE));
    if !already {
        arr.push(entry);
    }

    let output = serde_json::to_string_pretty(&settings)?;
    fs::write(path, output)?;
    Ok(())
}

/// Remove the graphify-rs instructions entry from `.vscode/settings.json`.
fn remove_vscode_settings_hook(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let mut settings: serde_json::Value =
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::json!({}));

    if let Some(instructions) = settings.get_mut("github.copilot.chat.codeGeneration.instructions")
    {
        if let Some(arr) = instructions.as_array_mut() {
            arr.retain(|v| {
                v.get("file").and_then(|f| f.as_str()) != Some(VSCODE_INSTRUCTIONS_FILE)
            });
        }
    }

    let output = serde_json::to_string_pretty(&settings)?;
    fs::write(path, output)?;
    Ok(())
}

/// Needle identifying graphify-rs content inside generated JSON/JS configs.
const GRAPHIFY_TRACE: &str = "graphify-rs";

/// A project-level platform integration that the `uninstall` sweep can remove.
///
/// `detect` exists so that a platform which was never installed is skipped
/// entirely rather than "uninstalled": several removal routines rewrite the
/// config file they touch, and rewriting somebody's untouched `settings.json`
/// just to reformat it is not a no-op.
struct Integration {
    /// Label used in the summary; matches the per-platform subcommand names.
    label: &'static str,
    /// Whether this platform currently has anything left to remove.
    detect: fn(&Path) -> bool,
    /// Removal routine, reused verbatim from the per-platform subcommands.
    remove: fn(&Path) -> Result<()>,
}

/// Every platform `uninstall_all` sweeps, in the order it sweeps them.
///
/// The AGENTS.md-based platforms share one section, so whichever runs first is
/// the one that physically deletes it; the rest then find nothing and no-op.
/// That is why detection is sampled up front (see [`uninstall_all`]).
fn integrations() -> Vec<Integration> {
    vec![
        Integration {
            label: "Claude",
            detect: claude_present,
            remove: claude_uninstall,
        },
        Integration {
            label: "CodeBuddy",
            detect: codebuddy_present,
            remove: codebuddy_uninstall,
        },
        Integration {
            label: "Codex",
            detect: codex_present,
            remove: codex_uninstall,
        },
        Integration {
            label: "OpenCode",
            detect: opencode_present,
            remove: opencode_uninstall,
        },
        Integration {
            label: "Claw",
            detect: agents_md_present,
            remove: |root| generic_platform_uninstall(root, "Claw"),
        },
        Integration {
            label: "Droid",
            detect: agents_md_present,
            remove: |root| generic_platform_uninstall(root, "Droid"),
        },
        Integration {
            label: "Trae",
            detect: agents_md_present,
            remove: |root| generic_platform_uninstall(root, "Trae"),
        },
        Integration {
            label: "Trae CN",
            detect: agents_md_present,
            remove: |root| generic_platform_uninstall(root, "Trae CN"),
        },
        Integration {
            label: "GitHub Copilot",
            detect: copilot_present,
            remove: copilot_uninstall,
        },
        Integration {
            label: "VSCode",
            detect: vscode_present,
            remove: vscode_uninstall,
        },
    ]
}

/// True when `path` exists, is readable, and contains `needle`.
fn file_contains(path: &Path, needle: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|content| content.contains(needle))
}

/// The shared `AGENTS.md` section every AGENTS-based platform installs.
fn agents_md_present(project_root: &Path) -> bool {
    file_contains(&project_root.join("AGENTS.md"), AGENTS_MD_MARKER)
}

fn claude_present(project_root: &Path) -> bool {
    file_contains(&project_root.join("CLAUDE.md"), CLAUDE_MD_MARKER)
        || file_contains(&project_root.join(".claude/settings.json"), GRAPHIFY_TRACE)
}

fn codebuddy_present(project_root: &Path) -> bool {
    agents_md_present(project_root)
        || file_contains(
            &project_root.join(".codebuddy/settings.json"),
            GRAPHIFY_TRACE,
        )
}

fn codex_present(project_root: &Path) -> bool {
    agents_md_present(project_root) || project_root.join(".codex/hooks.json").exists()
}

fn opencode_present(project_root: &Path) -> bool {
    agents_md_present(project_root)
        || project_root
            .join(".opencode/plugins/graphify-rs.js")
            .exists()
        || file_contains(&project_root.join("opencode.json"), GRAPHIFY_TRACE)
}

fn copilot_present(project_root: &Path) -> bool {
    file_contains(
        &project_root.join(".github/copilot-instructions.md"),
        COPILOT_MD_MARKER,
    )
}

fn vscode_present(project_root: &Path) -> bool {
    file_contains(
        &project_root.join(VSCODE_INSTRUCTIONS_FILE),
        VSCODE_MD_MARKER,
    ) || file_contains(&project_root.join(".vscode/settings.json"), GRAPHIFY_TRACE)
}

/// Remove graphify-rs integration from every supported agent platform.
///
/// Detection is snapshotted before anything is removed: the AGENTS.md-based
/// platforms share a single section, so probing after the first removal would
/// under-report what was actually there. Platforms with nothing installed are
/// skipped (a no-op, never an error), and one platform failing is collected as
/// a warning so the remaining platforms still get cleaned.
pub fn uninstall_all(project_root: &Path) -> Result<()> {
    println!("Removing graphify-rs integration from all detected platforms...\n");

    let (present, absent): (Vec<Integration>, Vec<Integration>) = integrations()
        .into_iter()
        .partition(|integration| (integration.detect)(project_root));
    let skipped: Vec<&str> = absent.iter().map(|integration| integration.label).collect();

    let mut removed: Vec<&str> = Vec::new();
    let mut failed: Vec<(&str, String)> = Vec::new();
    for integration in &present {
        match (integration.remove)(project_root) {
            Ok(()) => removed.push(integration.label),
            Err(err) => failed.push((integration.label, format!("{err:#}"))),
        }
    }

    // Git hooks are not an agent platform, but a full uninstall should leave no
    // graphify-rs trigger behind. Mirrors the Python `uninstall_all`.
    let mut hooks_removed = false;
    if graphify_hooks::hooks_installed(project_root) {
        match graphify_hooks::uninstall_hooks(project_root) {
            Ok(msg) => {
                println!("  {msg}");
                hooks_removed = true;
            }
            Err(err) => failed.push(("Git hooks", err.to_string())),
        }
    }

    println!("\nSummary");
    if removed.is_empty() && !hooks_removed {
        println!(
            "  Nothing to remove - no graphify-rs integration found in {}",
            project_root.display()
        );
    } else {
        let mut items = removed;
        if hooks_removed {
            items.push("Git hooks");
        }
        println!("  Removed: {}", items.join(", "));
    }
    if !skipped.is_empty() {
        println!("  Not installed (skipped): {}", skipped.join(", "));
    }
    for (label, err) in &failed {
        eprintln!("  warning: could not remove {label}: {err}");
    }
    println!("\n  Global skill files under your home directory were left untouched.");

    if !failed.is_empty() {
        anyhow::bail!(
            "{} integration(s) could not be removed; see the warnings above",
            failed.len()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Install-shaped fixtures for every platform the sweep knows about.
    fn write_full_integration(root: &Path) {
        fs::write(
            root.join("CLAUDE.md"),
            "# Project\n\n## graphify-rs\nrules\n",
        )
        .unwrap();
        fs::write(
            root.join("AGENTS.md"),
            "# Agents\n\n## graphify-rs\nrules\n",
        )
        .unwrap();

        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::write(
            root.join(".claude/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"matcher":"Glob|Grep","hooks":[{"type":"command","command":"graphify-rs"}]}]}}"#,
        )
        .unwrap();

        fs::create_dir_all(root.join(".codex")).unwrap();
        fs::write(root.join(".codex/hooks.json"), "{}").unwrap();

        fs::create_dir_all(root.join(".opencode/plugins")).unwrap();
        fs::write(root.join(".opencode/plugins/graphify-rs.js"), "// plugin").unwrap();
        fs::write(
            root.join("opencode.json"),
            r#"{"plugin":[".opencode/plugins/graphify-rs.js"]}"#,
        )
        .unwrap();
    }

    #[test]
    fn test_uninstall_all_with_nothing_installed() {
        let tmp = tempfile::tempdir().unwrap();

        uninstall_all(tmp.path()).unwrap();

        // A no-op must not create or rewrite anything in the project.
        let entries: Vec<_> = fs::read_dir(tmp.path()).unwrap().collect();
        assert!(entries.is_empty(), "uninstall created files in a clean dir");
    }

    #[test]
    fn test_uninstall_all_removes_every_platform() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_full_integration(root);

        uninstall_all(root).unwrap();

        let claude_md = fs::read_to_string(root.join("CLAUDE.md")).unwrap();
        assert!(!claude_md.contains(CLAUDE_MD_MARKER), "{claude_md}");
        assert!(claude_md.contains("# Project"), "other content dropped");

        let agents_md = fs::read_to_string(root.join("AGENTS.md")).unwrap();
        assert!(!agents_md.contains(AGENTS_MD_MARKER), "{agents_md}");

        let claude_settings = fs::read_to_string(root.join(".claude/settings.json")).unwrap();
        assert!(!claude_settings.contains("Glob|Grep"), "{claude_settings}");

        assert!(!root.join(".codex/hooks.json").exists());
        assert!(!root.join(".opencode/plugins/graphify-rs.js").exists());

        let opencode = fs::read_to_string(root.join("opencode.json")).unwrap();
        assert!(!opencode.contains(GRAPHIFY_TRACE), "{opencode}");
    }

    #[test]
    fn test_uninstall_all_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        write_full_integration(tmp.path());

        uninstall_all(tmp.path()).unwrap();
        // Second sweep finds nothing and must still succeed.
        uninstall_all(tmp.path()).unwrap();
    }

    #[test]
    fn test_uninstall_all_removes_git_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git/hooks")).unwrap();
        graphify_hooks::install_hooks(tmp.path()).unwrap();
        assert!(graphify_hooks::hooks_installed(tmp.path()));

        uninstall_all(tmp.path()).unwrap();

        assert!(!graphify_hooks::hooks_installed(tmp.path()));
    }

    #[test]
    fn test_detection_only_fires_on_our_markers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("CLAUDE.md"), "# Project\n\n## something else\n").unwrap();
        fs::write(root.join("AGENTS.md"), "# Agents\n").unwrap();

        assert!(!claude_present(root));
        assert!(!agents_md_present(root));
        assert!(!codex_present(root));
        assert!(!opencode_present(root));
        assert!(!codebuddy_present(root));
    }
}
