//! MCP config extractor: `.mcp.json`, `mcp.json`, `mcp_servers.json`,
//! `claude_desktop_config.json`.
//!
//! Routed by **filename**, before the generic `.json` extractor, so an MCP
//! config produces server topology instead of bare top-level keys.
//!
//! Server nodes are scoped to the config file's **full path**, so two projects
//! each defining a server called `github` stay distinct rather than collapsing
//! last-writer-wins. Command, package, and env-var nodes deliberately use
//! **global** IDs, so the same runtime or package referenced from several
//! configs becomes one hub node.
//!
//! Environment **values are never read** — only key names become nodes.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use super::{make_edge, make_file_node, path_str};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use graphify_security::sanitize_label;
use regex::Regex;
use serde_json::Value;

/// Same 1 MiB cap the generic JSON extractor uses.
const MAX_BYTES: usize = 1_048_576;
/// Generous, but flags pathological configs.
const MAX_SERVERS_PER_FILE: usize = 200;

// Patterns observed in real MCP server configs:
//   ["-y", "@modelcontextprotocol/server-filesystem", "/data"]  (npx)
//   ["-y", "@org/pkg@1.2.3"]
//   ["mcp-server-fetch"]                                        (uvx)
//   ["mcp-server-time", "--local-timezone=UTC"]
static RE_NPM_PKG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^@[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*(?:@[\w.\-+]+)?$").unwrap()
});
static RE_PY_MCP_PKG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-z0-9][a-z0-9._-]*-mcp(?:-[a-z0-9._-]+)?$|^mcp-[a-z0-9][a-z0-9._-]*$").unwrap()
});
static RE_ARG_FLAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^-{1,2}\w").unwrap());

pub(crate) fn extract_mcp_config(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    if source.len() > MAX_BYTES {
        return result;
    }

    let Ok(doc) = serde_json::from_str::<Value>(source) else {
        return result;
    };
    let Some(obj) = doc.as_object() else {
        return result;
    };

    // Some tools nest the map (e.g. {"mcp": {"servers": {...}}}). Try that one
    // well-known alternate shape, but do not search exhaustively.
    let servers = obj
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .or_else(|| {
            obj.get("mcp")
                .and_then(|v| v.as_object())
                .and_then(|m| m.get("servers"))
                .and_then(|v| v.as_object())
        });
    let Some(servers) = servers else {
        return result;
    };

    let scope = path_str(path);
    let mut ctx = EmitCtx {
        path,
        result: &mut result,
        seen_nodes: HashSet::new(),
        seen_edges: HashSet::new(),
    };
    ctx.seen_nodes.insert(file_id.clone());

    for (server_name, spec) in servers.iter().take(MAX_SERVERS_PER_FILE) {
        // A broken entry is the user's, not ours — skip it silently.
        let Some(spec) = spec.as_object() else {
            continue;
        };
        if server_name.is_empty() {
            continue;
        }
        emit_server(&mut ctx, &file_id, &scope, server_name, spec);
    }

    result
}

struct EmitCtx<'a> {
    path: &'a Path,
    result: &'a mut ExtractionResult,
    seen_nodes: HashSet<String>,
    seen_edges: HashSet<(String, String, String)>,
}

impl EmitCtx<'_> {
    fn add_node(&mut self, id: &str, label: &str, kind: &str, node_type: NodeType) {
        if id.is_empty() || !self.seen_nodes.insert(id.to_string()) {
            return;
        }
        let mut extra = HashMap::new();
        extra.insert("mcp_kind".to_string(), Value::String(kind.to_string()));
        self.result.nodes.push(GraphNode {
            id: id.to_string(),
            label: sanitize_label(label),
            source_file: path_str(self.path),
            // JSON offers no line numbers without a second parser pass.
            source_location: Some("L1".to_string()),
            node_type,
            community: None,
            extra,
        });
    }

    fn add_edge(&mut self, source: &str, target: &str, relation: &str, context: Option<&str>) {
        if source.is_empty() || target.is_empty() || source == target {
            return;
        }
        let key = (source.to_string(), target.to_string(), relation.to_string());
        if !self.seen_edges.insert(key) {
            return;
        }
        let mut edge = make_edge(source, target, relation, self.path, Confidence::Extracted);
        edge.source_location = Some("L1".to_string());
        if let Some(ctx) = context {
            edge.extra
                .insert("context".to_string(), Value::String(ctx.to_string()));
        }
        self.result.edges.push(edge);
    }
}

fn emit_server(
    ctx: &mut EmitCtx<'_>,
    file_id: &str,
    scope: &str,
    server_name: &str,
    spec: &serde_json::Map<String, Value>,
) {
    let server_id = make_id(&[scope, "mcp_server", server_name]);
    ctx.add_node(&server_id, server_name, "mcp_server", NodeType::Module);
    ctx.add_edge(file_id, &server_id, "contains", None);

    if let Some(command) = spec.get("command").and_then(|v| v.as_str()) {
        let command = command.trim();
        if !command.is_empty() {
            let cmd_id = make_id(&["mcp_command", command]);
            ctx.add_node(&cmd_id, command, "mcp_command", NodeType::Function);
            ctx.add_edge(&server_id, &cmd_id, "references", Some("command"));
        }
    }

    if let Some(args) = spec.get("args").and_then(|v| v.as_array())
        && let Some(package) = detect_package_from_args(args)
    {
        let pkg_id = make_id(&["mcp_package", &package]);
        ctx.add_node(&pkg_id, &package, "mcp_package", NodeType::Package);
        ctx.add_edge(&server_id, &pkg_id, "references", Some("package"));
    }

    // ONLY KEYS. Values may contain secrets and are never read here.
    if let Some(env) = spec.get("env").and_then(|v| v.as_object()) {
        for env_name in env.keys() {
            if env_name.is_empty() {
                continue;
            }
            let env_id = make_id(&["env_var", env_name]);
            ctx.add_node(&env_id, env_name, "env_var", NodeType::Variable);
            ctx.add_edge(&server_id, &env_id, "requires_env", None);
        }
    }
}

/// First arg that looks like an npm or PyPI package id, else `None`.
///
/// Skips short flags (`-y`) and option arguments (`--local-timezone=UTC`).
fn detect_package_from_args(args: &[Value]) -> Option<String> {
    for raw in args {
        let Some(arg) = raw.as_str() else { continue };
        let arg = arg.trim();
        if arg.is_empty() || RE_ARG_FLAG.is_match(arg) {
            continue;
        }
        if RE_NPM_PKG.is_match(arg) {
            return Some(strip_version(arg));
        }
        if RE_PY_MCP_PKG.is_match(arg) {
            return Some(arg.to_string());
        }
    }
    None
}

/// Drop the `@version` suffix from an npm package id, preserving the scope.
///
/// A scoped id has at most two `@` chars; the second is the version separator.
fn strip_version(pkg: &str) -> String {
    let search_from = if pkg.starts_with('@') { 1 } else { 0 };
    match pkg[search_from..].find('@') {
        Some(at) => pkg[..search_from + at].to_string(),
        None => pkg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn extract(src: &str) -> ExtractionResult {
        extract_mcp_config(&PathBuf::from("/repo/.mcp.json"), src)
    }

    #[test]
    fn emits_server_command_package_and_env() {
        let r = extract(
            r#"{"mcpServers": {"fs": {
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem@1.2.3", "/data"],
                "env": {"API_KEY": "secret-value-here"}
            }}}"#,
        );
        let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
        assert!(labels.contains(&"fs"));
        assert!(labels.contains(&"npx"));
        // Version suffix stripped, scope preserved.
        assert!(labels.contains(&"@modelcontextprotocol/server-filesystem"));
        assert!(labels.contains(&"API_KEY"));
        // The env *value* must never reach the graph.
        assert!(!labels.iter().any(|l| l.contains("secret-value-here")));

        let relations: Vec<&str> = r.edges.iter().map(|e| e.relation.as_str()).collect();
        assert!(relations.contains(&"contains"));
        assert!(relations.contains(&"references"));
        assert!(relations.contains(&"requires_env"));
    }

    #[test]
    fn command_ids_are_global_but_server_ids_are_file_scoped() {
        let src = r#"{"mcpServers": {"a": {"command": "uvx"}}}"#;
        let one = extract_mcp_config(Path::new("/repo/one/.mcp.json"), src);
        let two = extract_mcp_config(Path::new("/repo/two/.mcp.json"), src);

        let cmd = |r: &ExtractionResult| {
            r.nodes
                .iter()
                .find(|n| n.label == "uvx")
                .map(|n| n.id.clone())
                .unwrap()
        };
        assert_eq!(cmd(&one), cmd(&two), "commands should be hub nodes");

        let server = |r: &ExtractionResult| {
            r.nodes
                .iter()
                .find(|n| n.label == "a")
                .map(|n| n.id.clone())
                .unwrap()
        };
        assert_ne!(server(&one), server(&two), "servers are file-scoped");
    }

    #[test]
    fn accepts_nested_mcp_servers_shape() {
        let r = extract(r#"{"mcp": {"servers": {"git": {"command": "uvx"}}}}"#);
        assert!(r.nodes.iter().any(|n| n.label == "git"));
    }

    #[test]
    fn python_style_package_detected_from_bare_arg() {
        let r = extract(
            r#"{"mcpServers": {"t": {"command": "uvx", "args": ["mcp-server-time", "--local-timezone=UTC"]}}}"#,
        );
        assert!(r.nodes.iter().any(|n| n.label == "mcp-server-time"));
        assert!(!r.nodes.iter().any(|n| n.label.starts_with("--")));
    }

    #[test]
    fn malformed_input_keeps_only_the_file_node() {
        for src in [r#"{not json"#, r#"[1,2,3]"#, r#"{"other": {}}"#] {
            let r = extract(src);
            assert_eq!(r.nodes.len(), 1, "unexpected nodes for {src}");
            assert!(r.edges.is_empty());
        }
    }

    #[test]
    fn non_object_server_entries_are_skipped() {
        let r = extract(r#"{"mcpServers": {"good": {"command": "npx"}, "bad": "nope"}}"#);
        assert!(r.nodes.iter().any(|n| n.label == "good"));
        assert!(!r.nodes.iter().any(|n| n.label == "bad"));
    }

    #[test]
    fn strips_npm_version_suffixes() {
        assert_eq!(strip_version("@scope/name@1.2.3"), "@scope/name");
        assert_eq!(strip_version("@scope/name"), "@scope/name");
        assert_eq!(strip_version("name@1.0.0"), "name");
        assert_eq!(strip_version("name"), "name");
    }
}
