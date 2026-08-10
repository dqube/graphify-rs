//! Cargo workspace introspection: crate nodes and internal dependency edges.
//!
//! Reads `Cargo.toml` manifests directly rather than shelling out to
//! `cargo metadata`, so introspection works without a toolchain, without a
//! network fetch, and without building anything.
//!
//! Only **workspace-internal** dependencies produce edges. A dependency that
//! resolves to no member manifest (anything from crates.io) is skipped rather
//! than becoming a node, which keeps the graph about the workspace's own shape
//! instead of drowning it in third-party crates.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphEdge, GraphNode, NodeType};
use serde_json::Value;
use tracing::debug;

/// Canonical crate node id.
fn crate_id(name: &str) -> String {
    make_id(&["crate", name])
}

fn tag(key: &str, value: &str) -> HashMap<String, Value> {
    let mut m = HashMap::new();
    m.insert(key.to_string(), Value::String(value.to_string()));
    m
}

fn load_toml(path: &Path) -> Option<toml::Value> {
    let text = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            debug!("cargo introspection: cannot parse {}: {e}", path.display());
            None
        }
    }
}

/// Manifest paths for the root package (if any) plus every workspace member.
fn member_manifest_paths(root: &Path, root_data: &toml::Value) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if root_data.get("package").and_then(|v| v.as_table()).is_some() {
        paths.push(root.join("Cargo.toml"));
    }

    let members = root_data
        .get("workspace")
        .and_then(|v| v.as_table())
        .and_then(|w| w.get("members"))
        .and_then(|v| v.as_array());
    let Some(members) = members else {
        return paths;
    };

    for pattern in members.iter().filter_map(|v| v.as_str()) {
        for member in expand_pattern(root, pattern) {
            let manifest = member.join("Cargo.toml");
            if manifest.is_file() && !paths.contains(&manifest) {
                paths.push(manifest);
            }
        }
    }
    paths
}

/// Expand a `workspace.members` glob into concrete directories, sorted.
///
/// Supports the shapes Cargo workspaces actually use: literal paths, `*` and
/// `?` within a segment, and `**` spanning any number of directories.
fn expand_pattern(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let segments: Vec<&str> = pattern
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let mut out = Vec::new();
    expand_segments(root, &segments, &mut out);
    out.sort();
    out.dedup();
    out
}

fn expand_segments(base: &Path, segments: &[&str], out: &mut Vec<PathBuf>) {
    let Some((segment, rest)) = segments.split_first() else {
        if base.is_dir() {
            out.push(base.to_path_buf());
        }
        return;
    };

    if *segment == "**" {
        // `**` matches zero directories as well as any number of them.
        expand_segments(base, rest, out);
        for child in sub_dirs(base) {
            expand_segments(&child, segments, out);
        }
        return;
    }

    if segment.contains('*') || segment.contains('?') {
        for child in sub_dirs(base) {
            let name = child.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if glob_match(segment, name) {
                expand_segments(&child, rest, out);
            }
        }
        return;
    }

    expand_segments(&base.join(segment), rest, out);
}

fn sub_dirs(base: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(base) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Match one path segment against a `*`/`?` wildcard pattern.
fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // Standard iterative wildcard match: `star`/`mark` remember the last `*` so
    // a failed branch can backtrack to consuming one more character.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);

    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Crate nodes and `crate_depends_on` edges for the workspace rooted at `root`.
pub fn introspect_cargo(root: &Path) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let root_path = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let root_manifest = root_path.join("Cargo.toml");

    let Some(root_data) = load_toml(&root_manifest) else {
        debug!("cargo introspection: no readable {}", root_manifest.display());
        return result;
    };

    // Sorted by crate name so node and edge order is deterministic.
    let mut crates: BTreeMap<String, (PathBuf, toml::Value)> = BTreeMap::new();
    for manifest in member_manifest_paths(&root_path, &root_data) {
        let data = if manifest == root_manifest {
            root_data.clone()
        } else {
            match load_toml(&manifest) {
                Some(d) => d,
                None => continue,
            }
        };
        let name = data
            .get("package")
            .and_then(|v| v.as_table())
            .and_then(|p| p.get("name"))
            .and_then(|v| v.as_str());
        if let Some(name) = name {
            crates.insert(name.to_string(), (manifest, data));
        }
    }

    let rel = |manifest: &Path| -> String {
        manifest
            .strip_prefix(&root_path)
            .unwrap_or(manifest)
            .to_string_lossy()
            .replace('\\', "/")
    };

    for (name, (manifest, _)) in &crates {
        result.nodes.push(GraphNode {
            id: crate_id(name),
            label: name.clone(),
            source_file: rel(manifest),
            source_location: Some("L1".to_string()),
            node_type: NodeType::Package,
            community: None,
            extra: tag("ecosystem", "cargo"),
        });
    }

    for (name, (manifest, data)) in &crates {
        let Some(deps) = data.get("dependencies").and_then(|v| v.as_table()) else {
            continue;
        };
        let source_file = rel(manifest);
        for dep_name in deps.keys() {
            if !crates.contains_key(dep_name) || dep_name == name {
                continue;
            }
            let mut extra = tag("context", "cargo_dependency");
            extra.insert("ecosystem".to_string(), Value::String("cargo".to_string()));
            result.edges.push(GraphEdge {
                source: crate_id(name),
                target: crate_id(dep_name),
                relation: "crate_depends_on".to_string(),
                confidence: Confidence::Extracted,
                confidence_score: Confidence::Extracted.default_score(),
                source_file: source_file.clone(),
                source_location: Some("L1".to_string()),
                weight: 1.0,
                provenance: Some("cargo:crate_depends_on".to_string()),
                extra,
            });
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// Workspace: root virtual manifest + crates/a, crates/b (b depends on a),
    /// plus an external dependency that must not become a node.
    fn sample_workspace() -> TempDir {
        let td = TempDir::new().unwrap();
        let r = td.path();
        write(r, "Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
        write(r, "crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
        write(
            r,
            "crates/b/Cargo.toml",
            "[package]\nname = \"b\"\n[dependencies]\na = { path = \"../a\" }\nserde = \"1\"\n",
        );
        td
    }

    #[test]
    fn emits_member_crates_and_internal_edges() {
        let td = sample_workspace();
        let r = introspect_cargo(td.path());

        let labels: Vec<&str> = r.nodes.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["a", "b"]);
        assert!(r.nodes.iter().all(|n| n.node_type == NodeType::Package));

        assert_eq!(r.edges.len(), 1);
        assert_eq!(r.edges[0].relation, "crate_depends_on");
        assert_eq!(r.edges[0].source, crate_id("b"));
        assert_eq!(r.edges[0].target, crate_id("a"));
    }

    #[test]
    fn external_dependencies_are_not_emitted() {
        let td = sample_workspace();
        let r = introspect_cargo(td.path());
        assert!(!r.nodes.iter().any(|n| n.label == "serde"));
        assert!(!r.edges.iter().any(|e| e.target == crate_id("serde")));
    }

    #[test]
    fn source_files_are_workspace_relative() {
        let td = sample_workspace();
        let r = introspect_cargo(td.path());
        for node in &r.nodes {
            assert!(
                node.source_file.starts_with("crates/"),
                "expected relative path, got {}",
                node.source_file
            );
        }
    }

    #[test]
    fn root_package_is_included_alongside_members() {
        let td = TempDir::new().unwrap();
        let r = td.path();
        write(
            r,
            "Cargo.toml",
            "[package]\nname = \"top\"\n[workspace]\nmembers = [\"sub\"]\n[dependencies]\nkid = { path = \"sub\" }\n",
        );
        write(r, "sub/Cargo.toml", "[package]\nname = \"kid\"\n");

        let res = introspect_cargo(td.path());
        let labels: Vec<&str> = res.nodes.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["kid", "top"]);
        assert_eq!(res.edges.len(), 1);
        assert_eq!(res.edges[0].source, crate_id("top"));
    }

    #[test]
    fn missing_or_malformed_root_manifest_yields_nothing() {
        let td = TempDir::new().unwrap();
        assert!(introspect_cargo(td.path()).nodes.is_empty());

        write(td.path(), "Cargo.toml", "this is not = = toml");
        assert!(introspect_cargo(td.path()).nodes.is_empty());
    }

    #[test]
    fn deep_glob_matches_nested_members() {
        let td = TempDir::new().unwrap();
        let r = td.path();
        write(r, "Cargo.toml", "[workspace]\nmembers = [\"libs/**\"]\n");
        write(r, "libs/x/y/Cargo.toml", "[package]\nname = \"deep\"\n");

        let res = introspect_cargo(td.path());
        assert_eq!(res.nodes.len(), 1);
        assert_eq!(res.nodes[0].label, "deep");
    }

    #[test]
    fn wildcard_matcher_handles_common_shapes() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("graphify-*", "graphify-core"));
        assert!(!glob_match("graphify-*", "other-core"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "abbc"));
        assert!(glob_match("*-core", "graphify-core"));
        assert!(glob_match("a*c*e", "abcde"));
        assert!(!glob_match("a*c*f", "abcde"));
    }
}
