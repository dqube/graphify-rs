//! Deterministic markdown cross-reference extraction.
//!
//! Scans `.md` documents for links to other files in the project and emits
//! `references` edges between the corresponding File nodes. Runs without an
//! LLM, so documentation wiring shows up in every build:
//!
//! - inline links: `[text](./other.md)`, `[text](../src/main.rs#L10)`
//! - reference-style definitions: `[ref]: ./other.md`
//! - wiki links: `[[other-doc]]`, `[[other-doc|alias]]`
//!
//! External URLs (`https://…`, `mailto:…`, …) and pure `#anchor` links are
//! skipped. Link targets are matched against the set of files detected in the
//! project; unresolved targets are ignored.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphEdge};

use crate::ast_extract::{make_file_node, path_str};

/// `[text](target)` / `[text](<target with spaces>)` — target in group 1 or 2.
fn inline_link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[[^\]]*\]\(\s*(?:<([^>]+)>|([^\s)]+))").unwrap())
}

/// `[ref]: target` reference-style definitions — target in group 1 or 2.
fn ref_def_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^[ \t]{0,3}\[[^\]]+\]:\s*(?:<([^>]+)>|(\S+))").unwrap())
}

/// `[[target]]` / `[[target|alias]]` — target in group 1.
fn wikilink_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[\[([^\]|]+)(?:\|[^\]]*)?\]\]").unwrap())
}

/// URI scheme prefix like `https:` or `mailto:` (2–16 chars after the first letter).
fn scheme_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9+.-]{1,15}:").unwrap())
}

/// All raw link targets found in one markdown document.
fn link_targets(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for re in [inline_link_re(), ref_def_re(), wikilink_re()] {
        for cap in re.captures_iter(content) {
            let target = cap
                .get(1)
                .or_else(|| cap.get(2))
                .map(|m| m.as_str().trim().to_string());
            if let Some(t) = target
                && !t.is_empty()
            {
                out.push(t);
            }
        }
    }
    out
}

/// Lexically normalize a path: resolve `.` and `..` components without
/// touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Clean a raw markdown link target: URL-decode the common cases and strip
/// any `#anchor` / `?query` suffix. Returns `None` for links that can never
/// point into the project (external URLs, pure anchors, protocol-relative).
fn clean_target(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.starts_with('#') || t.starts_with("//") || scheme_re().is_match(t) {
        return None;
    }
    let t = t.replace("%20", " ");
    let end = t.find(['#', '?']).unwrap_or(t.len());
    let t = t[..end].trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// Resolve one cleaned target to a known project file.
///
/// `known` maps normalized absolute paths to the original path (used for node
/// identity); `stems` indexes lowercase file stems for wiki-style links.
fn resolve_target(
    source_file: &Path,
    root: &Path,
    target: &str,
    known: &HashMap<PathBuf, PathBuf>,
    stems: &HashMap<String, Vec<PathBuf>>,
) -> Option<PathBuf> {
    let candidate = if let Some(stripped) = target.strip_prefix('/') {
        // Root-relative: "/docs/guide.md" from the project root.
        normalize(&root.join(stripped))
    } else {
        let parent = source_file.parent().unwrap_or(root);
        normalize(&parent.join(target))
    };

    if let Some(orig) = known.get(&candidate) {
        return Some(orig.clone());
    }

    // Extensionless link (`[[guide]]` or `[x](./guide)`): try `.md`.
    if Path::new(target).extension().is_none() {
        let with_md = candidate.with_extension("md");
        if let Some(orig) = known.get(&with_md) {
            return Some(orig.clone());
        }
        // Wiki-style stem match, accepted only when unambiguous.
        let stem = Path::new(target)
            .file_stem()
            .or_else(|| Path::new(target).file_name())?
            .to_str()?
            .to_lowercase();
        if let Some(matches) = stems.get(&stem)
            && matches.len() == 1
        {
            return Some(matches[0].clone());
        }
    }
    None
}

/// Extract `references` edges between markdown documents and the files they
/// link to.
///
/// `md_files` are the absolute paths of the detected `.md` documents;
/// `known_files` are the absolute paths of every detected file in the project
/// (any type). File nodes use the same identity convention as the AST pass
/// (`make_id` of the path string), so links to code files attach to the File
/// nodes those extractors already create.
pub fn extract_markdown_links(
    root: &Path,
    md_files: &[PathBuf],
    known_files: &[PathBuf],
) -> ExtractionResult {
    let mut known: HashMap<PathBuf, PathBuf> = HashMap::new();
    for f in known_files {
        known.insert(normalize(f), f.clone());
    }
    let mut stems: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for f in known_files {
        if let Some(stem) = f.file_stem().and_then(|s| s.to_str()) {
            stems
                .entry(stem.to_lowercase())
                .or_default()
                .push(f.clone());
        }
    }

    let mut result = ExtractionResult::default();
    let mut seen_nodes: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<(String, String)> = HashSet::new();

    for md in md_files {
        let content = match std::fs::read_to_string(md) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let src_id = make_id(&[&path_str(md)]);

        for raw in link_targets(&content) {
            let Some(target) = clean_target(&raw) else {
                continue;
            };
            let Some(dst) = resolve_target(md, root, &target, &known, &stems) else {
                continue;
            };
            let dst_id = make_id(&[&path_str(&dst)]);
            if dst_id == src_id || !seen_edges.insert((src_id.clone(), dst_id.clone())) {
                continue;
            }
            if seen_nodes.insert(src_id.clone()) {
                result.nodes.push(make_file_node(md));
            }
            if seen_nodes.insert(dst_id.clone()) {
                result.nodes.push(make_file_node(&dst));
            }
            result.edges.push(GraphEdge {
                source: src_id.clone(),
                target: dst_id,
                relation: "references".to_string(),
                confidence: Confidence::Extracted,
                confidence_score: 1.0,
                source_file: path_str(md),
                source_location: None,
                weight: 1.0,
                provenance: Some("markdown-links".to_string()),
                extra: HashMap::new(),
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_inline_links() {
        let md = "See [the guide](./docs/guide.md) and [code](../src/main.rs#L12).";
        let targets = link_targets(md);
        assert!(targets.contains(&"./docs/guide.md".to_string()));
        assert!(targets.contains(&"../src/main.rs#L12".to_string()));
    }

    #[test]
    fn finds_angle_bracket_and_title_variants() {
        let md = "[a](<./my doc.md>) [b](./other.md \"A title\")";
        let targets = link_targets(md);
        assert!(targets.contains(&"./my doc.md".to_string()));
        assert!(targets.contains(&"./other.md".to_string()));
    }

    #[test]
    fn finds_reference_definitions() {
        let md = "Text with [a link][g].\n\n[g]: ./guide.md \"Guide\"\n";
        let targets = link_targets(md);
        assert!(targets.contains(&"./guide.md".to_string()));
    }

    #[test]
    fn finds_wikilinks() {
        let md = "See [[Architecture]] and [[notes/todo|TODO list]].";
        let targets = link_targets(md);
        assert!(targets.contains(&"Architecture".to_string()));
        assert!(targets.contains(&"notes/todo".to_string()));
    }

    #[test]
    fn clean_rejects_external_and_anchor_links() {
        assert_eq!(clean_target("https://example.com/x.md"), None);
        assert_eq!(clean_target("mailto:a@b.c"), None);
        assert_eq!(clean_target("#section"), None);
        assert_eq!(clean_target("//cdn.example.com/x"), None);
    }

    #[test]
    fn clean_strips_anchor_and_decodes_spaces() {
        assert_eq!(
            clean_target("./guide.md#install"),
            Some("./guide.md".to_string())
        );
        assert_eq!(
            clean_target("./my%20doc.md"),
            Some("./my doc.md".to_string())
        );
    }

    #[test]
    fn normalize_resolves_dot_components() {
        let p = normalize(Path::new("/a/b/../c/./d.md"));
        assert_eq!(p, PathBuf::from("/a/c/d.md"));
    }

    #[test]
    fn resolve_relative_link_to_known_file() {
        let root = Path::new("/proj");
        let src = PathBuf::from("/proj/docs/a.md");
        let known_file = PathBuf::from("/proj/docs/guide.md");
        let known: HashMap<PathBuf, PathBuf> = [(normalize(&known_file), known_file.clone())]
            .into_iter()
            .collect();
        let stems = HashMap::new();
        let resolved = resolve_target(&src, root, "./guide.md", &known, &stems);
        assert_eq!(resolved, Some(known_file));
    }

    #[test]
    fn resolve_wikilink_by_unique_stem() {
        let root = Path::new("/proj");
        let src = PathBuf::from("/proj/a.md");
        let arch = PathBuf::from("/proj/docs/Architecture.md");
        let known: HashMap<PathBuf, PathBuf> =
            [(normalize(&arch), arch.clone())].into_iter().collect();
        let mut stems: HashMap<String, Vec<PathBuf>> = HashMap::new();
        stems.insert("architecture".to_string(), vec![arch.clone()]);
        let resolved = resolve_target(&src, root, "Architecture", &known, &stems);
        assert_eq!(resolved, Some(arch));
    }

    #[test]
    fn ambiguous_stem_not_resolved() {
        let root = Path::new("/proj");
        let src = PathBuf::from("/proj/a.md");
        let known = HashMap::new();
        let mut stems: HashMap<String, Vec<PathBuf>> = HashMap::new();
        stems.insert(
            "main".to_string(),
            vec![
                PathBuf::from("/proj/src/main.rs"),
                PathBuf::from("/proj/x/main.py"),
            ],
        );
        assert_eq!(resolve_target(&src, root, "main", &known, &stems), None);
    }
}
