//! Package manifest extractor: `pyproject.toml`, `go.mod`, `pom.xml`, `apm.yml`.
//!
//! Each manifest contributes one **canonical package node** keyed by package
//! *name* (`make_id(["pkg", name])`), so a package's own manifest and every
//! dependent's dependency line collapse onto a single node. That is what turns
//! a monorepo's manifests into a dependency hub rather than a set of isolated
//! per-file islands.
//!
//! Dependencies deliberately get **no stub node**. The `depends_on` edge points
//! at the dependency's canonical id; if that package's own manifest is in the
//! corpus the edge resolves, and if the dependency is external
//! `build_from_extraction` prunes the dangling edge. A stub with an empty
//! `source_file` would risk clobbering the real node under id-dedup.
//!
//! `apm.yml` is parsed by a small line scanner rather than a full YAML parser —
//! the manifest shape we need (`name:` plus a flat `dependencies:` block) does
//! not justify a YAML dependency.

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use super::{make_edge, path_str};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::manifests::manifest_ecosystem;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use graphify_security::sanitize_label;
use regex::Regex;
use serde_json::Value;

/// Manifests are small; this rejects junk that merely shares the name.
const MAX_MANIFEST_BYTES: usize = 2_000_000;

/// Parsed manifest: the package it declares and what it depends on.
struct ManifestInfo {
    name: String,
    version: Option<String>,
    deps: Vec<String>,
}

/// Canonical package node id, keyed by package NAME.
fn pkg_id(name: &str) -> String {
    make_id(&["pkg", name])
}

pub(crate) fn extract_package_manifest(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    if source.len() > MAX_MANIFEST_BYTES {
        return result;
    }
    let Some(eco) = manifest_ecosystem(path) else {
        return result;
    };

    // A malformed manifest must not abort extraction — it just yields nothing.
    let info = match eco {
        "python" => parse_pyproject(source),
        "go" => parse_gomod(source),
        "maven" => parse_pom(source),
        "apm" => parse_apm(source),
        _ => None,
    };
    let Some(info) = info.filter(|i| !i.name.is_empty()) else {
        return result;
    };

    let ps = path_str(path);
    let self_id = pkg_id(&info.name);
    let mut extra = std::collections::HashMap::new();
    extra.insert("type".to_string(), Value::String("package".to_string()));
    extra.insert("ecosystem".to_string(), Value::String(eco.to_string()));
    if let Some(v) = info.version.as_deref().filter(|v| !v.is_empty()) {
        extra.insert("version".to_string(), Value::String(v.to_string()));
    }
    result.nodes.push(GraphNode {
        id: self_id.clone(),
        label: sanitize_label(&info.name),
        source_file: ps,
        source_location: Some("L1".to_string()),
        node_type: NodeType::Package,
        community: None,
        extra,
    });

    let mut seen: HashSet<String> = HashSet::new();
    for dep in info.deps {
        if dep.is_empty() {
            continue;
        }
        let dep_id = pkg_id(&dep);
        if dep_id == self_id || dep_id.is_empty() || !seen.insert(dep_id.clone()) {
            continue;
        }
        let mut edge = make_edge(
            &self_id,
            &dep_id,
            "depends_on",
            path,
            Confidence::Extracted,
        );
        edge.source_location = Some("L1".to_string());
        edge.extra.insert(
            "context".to_string(),
            Value::String("dependency".to_string()),
        );
        result.edges.push(edge);
    }

    result
}

// ── per-ecosystem parsers ────────────────────────────────────────────────────

/// `requests>=2.0` -> `requests`; `pkg[extra]==1; python_version<'3.9'` -> `pkg`.
fn pep508_name(spec: &str) -> String {
    let spec = spec.trim();
    let end = spec
        .find(|c: char| c.is_whitespace() || "<>=!~;[(".contains(c))
        .unwrap_or(spec.len());
    spec[..end].to_string()
}

fn parse_pyproject(text: &str) -> Option<ManifestInfo> {
    let data: toml::Value = toml::from_str(text).ok()?;
    let project = data.get("project").and_then(|v| v.as_table());
    let poetry = data
        .get("tool")
        .and_then(|v| v.as_table())
        .and_then(|t| t.get("poetry"))
        .and_then(|v| v.as_table());

    let str_field = |t: Option<&toml::map::Map<String, toml::Value>>, key: &str| {
        t.and_then(|t| t.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };

    let name = str_field(project, "name").or_else(|| str_field(poetry, "name"))?;
    let version = str_field(project, "version").or_else(|| str_field(poetry, "version"));

    let mut deps: Vec<String> = project
        .and_then(|p| p.get("dependencies"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(pep508_name)
                .collect()
        })
        .unwrap_or_default();

    // Poetry keys the dependency table by name; `python` is the interpreter
    // constraint, not a package.
    if let Some(table) = poetry
        .and_then(|p| p.get("dependencies"))
        .and_then(|v| v.as_table())
    {
        deps.extend(
            table
                .keys()
                .filter(|k| !k.eq_ignore_ascii_case("python"))
                .cloned(),
        );
    }

    Some(ManifestInfo {
        name,
        version,
        deps,
    })
}

static RE_GO_MODULE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^module\s+(\S+)").unwrap());
static RE_GO_REQUIRE_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^require\s*\(").unwrap());
static RE_GO_BLOCK_DEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\S+)\s+v\S+").unwrap());
static RE_GO_SINGLE_DEP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^require\s+(\S+)\s+v\S+").unwrap());

fn parse_gomod(text: &str) -> Option<ManifestInfo> {
    let mut name: Option<String> = None;
    let mut deps = Vec::new();
    let mut in_block = false;

    for line in text.lines() {
        let s = line.trim();
        if name.is_none()
            && let Some(c) = RE_GO_MODULE.captures(s)
        {
            name = Some(c[1].to_string());
            continue;
        }
        if RE_GO_REQUIRE_OPEN.is_match(s) {
            in_block = true;
            continue;
        }
        if in_block {
            if s.starts_with(')') {
                in_block = false;
                continue;
            }
            if let Some(c) = RE_GO_BLOCK_DEP.captures(s) {
                deps.push(c[1].to_string());
            }
        } else if let Some(c) = RE_GO_SINGLE_DEP.captures(s) {
            deps.push(c[1].to_string());
        }
    }

    name.map(|name| ManifestInfo {
        name,
        version: None,
        deps,
    })
}

static RE_APM_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^name:\s*["']?([^"'\s#]+)"#).unwrap());
static RE_APM_DEPS_OPEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^dependencies:\s*$").unwrap());
static RE_APM_DEP_ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^\s*-\s*["']?([^"'\s#:]+)"#).unwrap());
static RE_APM_DEP_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{2,}([A-Za-z0-9._/@-]+)\s*:").unwrap());

fn parse_apm(text: &str) -> Option<ManifestInfo> {
    let mut name: Option<String> = None;
    let mut deps = Vec::new();
    let mut in_deps = false;

    for line in text.lines() {
        if !in_deps
            && let Some(c) = RE_APM_NAME.captures(line)
        {
            name = Some(c[1].to_string());
            continue;
        }
        if RE_APM_DEPS_OPEN.is_match(line) {
            in_deps = true;
            continue;
        }
        if in_deps {
            if let Some(c) = RE_APM_DEP_ITEM
                .captures(line)
                .or_else(|| RE_APM_DEP_KEY.captures(line))
            {
                deps.push(c[1].to_string());
            } else if line.starts_with(|c: char| !c.is_whitespace()) {
                // The next top-level key ends the block.
                in_deps = false;
            }
        }
    }

    name.map(|name| ManifestInfo {
        name,
        version: None,
        deps,
    })
}

// ── minimal XML element scan for pom.xml ─────────────────────────────────────
//
// A pom needs *direct child* semantics: `<project><artifactId>` is the module's
// own id, while the `<artifactId>` inside `<parent>` or `<dependency>` is not.
// A depth-tracking scan gives that without pulling in an XML parser, and stays
// consistent with the regex-based .NET project extractor next door.

static RE_XML_COMMENT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());
static RE_XML_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(/?)([A-Za-z_][\w.:-]*)([^>]*?)(/?)>").unwrap());

struct XmlElement {
    name: String,
    depth: usize,
    inner: std::ops::Range<usize>,
}

/// Every closed element in `xml`, with its nesting depth (root = 1) and the
/// byte range of its inner content.
fn scan_elements(xml: &str) -> Vec<XmlElement> {
    let mut out = Vec::new();
    let mut stack: Vec<(String, usize)> = Vec::new();

    for cap in RE_XML_TAG.captures_iter(xml) {
        let whole = cap.get(0).unwrap();
        let is_close = !cap[1].is_empty();
        let self_closing = !cap[4].is_empty();
        let name = cap[2].to_string();

        if is_close {
            // Unwind to the matching open tag; malformed markup just loses the
            // unmatched frames rather than derailing the scan.
            if let Some(pos) = stack.iter().rposition(|(n, _)| *n == name) {
                let (_, inner_start) = stack[pos].clone();
                out.push(XmlElement {
                    name,
                    depth: pos + 1,
                    inner: inner_start..whole.start(),
                });
                stack.truncate(pos);
            }
        } else if !self_closing {
            stack.push((name, whole.end()));
        }
    }
    out
}

/// Text of the first element named `tag` at exactly `depth`, within `range`.
fn element_text<'a>(
    xml: &'a str,
    elements: &[XmlElement],
    tag: &str,
    depth: usize,
    range: &std::ops::Range<usize>,
) -> Option<&'a str> {
    elements
        .iter()
        .find(|e| {
            e.name == tag
                && e.depth == depth
                && e.inner.start >= range.start
                && e.inner.end <= range.end
        })
        .map(|e| xml[e.inner.clone()].trim())
        .filter(|t| !t.is_empty() && !t.contains('<'))
}

fn parse_pom(text: &str) -> Option<ManifestInfo> {
    let xml = RE_XML_COMMENT.replace_all(text, "");
    let elements = scan_elements(&xml);
    let root = elements.iter().find(|e| e.depth == 1)?;

    let artifact = element_text(&xml, &elements, "artifactId", 2, &root.inner)?;
    let group = element_text(&xml, &elements, "groupId", 2, &root.inner);
    let name = match group {
        Some(g) => format!("{g}:{artifact}"),
        None => artifact.to_string(),
    };
    let version = element_text(&xml, &elements, "version", 2, &root.inner).map(str::to_string);

    let mut deps = Vec::new();
    for dep in elements.iter().filter(|e| e.name == "dependency") {
        let child_depth = dep.depth + 1;
        let Some(da) = element_text(&xml, &elements, "artifactId", child_depth, &dep.inner) else {
            continue;
        };
        let dg = element_text(&xml, &elements, "groupId", child_depth, &dep.inner);
        deps.push(match dg {
            Some(g) => format!("{g}:{da}"),
            None => da.to_string(),
        });
    }

    Some(ManifestInfo {
        name,
        version,
        deps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn run(name: &str, src: &str) -> ExtractionResult {
        extract_package_manifest(&PathBuf::from(format!("/repo/{name}")), src)
    }

    fn dep_labels(r: &ExtractionResult) -> Vec<String> {
        r.edges.iter().map(|e| e.target.clone()).collect()
    }

    #[test]
    fn pyproject_pep621_name_version_and_deps() {
        let r = run(
            "pyproject.toml",
            r#"
[project]
name = "mypkg"
version = "1.2.3"
dependencies = ["requests>=2.0", "pkg[extra]==1; python_version<'3.9'"]
"#,
        );
        assert_eq!(r.nodes.len(), 1);
        assert_eq!(r.nodes[0].label, "mypkg");
        assert_eq!(r.nodes[0].extra["version"], "1.2.3");
        assert_eq!(r.nodes[0].extra["ecosystem"], "python");
        assert!(dep_labels(&r).contains(&pkg_id("requests")));
        assert!(dep_labels(&r).contains(&pkg_id("pkg")));
    }

    #[test]
    fn pyproject_poetry_table_drops_python_constraint() {
        let r = run(
            "pyproject.toml",
            r#"
[tool.poetry]
name = "poetrypkg"
version = "0.1.0"
[tool.poetry.dependencies]
python = "^3.11"
httpx = "^0.27"
"#,
        );
        assert_eq!(r.nodes[0].label, "poetrypkg");
        assert!(dep_labels(&r).contains(&pkg_id("httpx")));
        assert!(!dep_labels(&r).contains(&pkg_id("python")));
    }

    #[test]
    fn gomod_block_and_single_require() {
        let r = run(
            "go.mod",
            r#"module github.com/me/proj

go 1.22

require github.com/pkg/errors v0.9.1

require (
	github.com/spf13/cobra v1.8.0
	golang.org/x/sync v0.7.0
)
"#,
        );
        assert_eq!(r.nodes[0].label, "github.com/me/proj");
        let deps = dep_labels(&r);
        assert!(deps.contains(&pkg_id("github.com/pkg/errors")));
        assert!(deps.contains(&pkg_id("github.com/spf13/cobra")));
        assert!(deps.contains(&pkg_id("golang.org/x/sync")));
    }

    #[test]
    fn pom_uses_direct_children_not_parent_or_dependency_ids() {
        let r = run(
            "pom.xml",
            r#"<?xml version="1.0"?>
<project xmlns="http://maven.apache.org/POM/4.0.0">
  <parent>
    <groupId>org.parent</groupId>
    <artifactId>parent-pom</artifactId>
  </parent>
  <groupId>com.example</groupId>
  <artifactId>my-app</artifactId>
  <version>2.0.0</version>
  <dependencies>
    <dependency>
      <groupId>junit</groupId>
      <artifactId>junit</artifactId>
    </dependency>
  </dependencies>
</project>
"#,
        );
        assert_eq!(r.nodes[0].label, "com.example:my-app");
        assert_eq!(r.nodes[0].extra["version"], "2.0.0");
        assert_eq!(dep_labels(&r), vec![pkg_id("junit:junit")]);
    }

    #[test]
    fn pom_comments_do_not_confuse_the_scan() {
        let r = run(
            "pom.xml",
            r#"<project>
  <!-- <artifactId>commented-out</artifactId> -->
  <artifactId>real-app</artifactId>
</project>"#,
        );
        assert_eq!(r.nodes[0].label, "real-app");
    }

    #[test]
    fn apm_yaml_line_scan() {
        let r = run(
            "apm.yml",
            r#"name: my-atom-pkg
dependencies:
  - underscore
  - jquery
version: 1.0.0
"#,
        );
        assert_eq!(r.nodes[0].label, "my-atom-pkg");
        let deps = dep_labels(&r);
        assert!(deps.contains(&pkg_id("underscore")));
        assert!(deps.contains(&pkg_id("jquery")));
    }

    #[test]
    fn dependency_ids_are_canonical_across_manifests() {
        // A monorepo: one package's manifest and another's dependency line must
        // agree on the id, so the edge resolves instead of being pruned.
        let lib = run("pyproject.toml", "[project]\nname = \"shared-lib\"\n");
        let app = run(
            "pyproject.toml",
            "[project]\nname = \"app\"\ndependencies = [\"shared-lib\"]\n",
        );
        assert_eq!(app.edges[0].target, lib.nodes[0].id);
    }

    #[test]
    fn no_stub_nodes_for_dependencies() {
        let r = run(
            "pyproject.toml",
            "[project]\nname = \"solo\"\ndependencies = [\"external-thing\"]\n",
        );
        assert_eq!(r.nodes.len(), 1, "only the manifest's own package node");
        assert_eq!(r.edges.len(), 1);
    }

    #[test]
    fn self_referential_and_duplicate_deps_are_dropped() {
        let r = run(
            "pyproject.toml",
            "[project]\nname = \"loop\"\ndependencies = [\"loop\", \"a\", \"a\"]\n",
        );
        assert_eq!(r.edges.len(), 1);
    }

    #[test]
    fn malformed_manifests_yield_nothing() {
        assert!(run("pyproject.toml", "not = = toml").nodes.is_empty());
        assert!(run("pyproject.toml", "[project]\nversion = \"1\"\n").nodes.is_empty());
        assert!(run("go.mod", "go 1.22\n").nodes.is_empty());
        assert!(run("pom.xml", "<project></project>").nodes.is_empty());
    }
}
