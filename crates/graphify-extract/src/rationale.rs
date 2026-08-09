//! Deterministic rationale and design-reference extraction.
//!
//! A second, LLM-free pass over source files that captures the *why* the AST
//! pass throws away:
//!
//! - **Rationale comments** — `# NOTE:`, `// WHY:`, `-- HACK:`, and friends
//!   become `Concept` nodes joined to their file by a `rationale_for` edge.
//! - **Python docstrings** — module, class, and function docstrings attach to
//!   the node the AST pass created for that definition.
//! - **Design references** — `ADR-0011` / `RFC 793` cited inside comments
//!   become shared nodes, so every file citing the same decision record
//!   converges on one node via `cites` edges.
//!
//! Comment syntax is chosen per language, so this works across the whole
//! extractor fleet rather than just Python and JS/TS.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphEdge, GraphNode, NodeType};

use crate::ast_extract::path_str;

/// Comment openers that mark a rationale tag, e.g. `# NOTE:` or `// WHY:`.
///
/// `*` covers JSDoc/rustdoc continuation lines inside block comments.
fn comment_markers(lang: &str) -> &'static [&'static str] {
    match lang {
        "python" | "ruby" | "shell" | "powershell" | "elixir" | "julia" | "hcl" => &["#"],
        "sql" | "lua" => &["--"],
        "fortran" => &["!"],
        // Data formats carry no comments worth mining.
        "json" | "dotnet_proj" => &[],
        _ => &["//", "*"],
    }
}

/// Tags that mark a comment as design rationale rather than description.
const RATIONALE_TAGS: &[&str] = &[
    "NOTE",
    "IMPORTANT",
    "HACK",
    "WHY",
    "RATIONALE",
    "TODO",
    "FIXME",
];

/// Design references worth first-classing. Deliberately conservative:
/// `ADR-NNNN` (any zero padding) and `RFC NNNN` / `RFC-NNNN`.
static DOC_REF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b(ADR|RFC)[- ]?(\d{1,5})\b").unwrap());

/// Labels are kept short enough to read in a diagram node.
const MAX_LABEL_LEN: usize = 80;

/// Collapse a comment body into a single-line label.
fn label_from(text: &str) -> String {
    let flattened = text.replace(['\r', '\n'], " ");
    let collapsed = flattened.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= MAX_LABEL_LEN {
        collapsed
    } else {
        collapsed.chars().take(MAX_LABEL_LEN).collect()
    }
}

/// Strip a leading comment marker, returning the comment body.
fn strip_marker<'a>(line: &'a str, markers: &[&str]) -> Option<&'a str> {
    let trimmed = line.trim_start();
    for marker in markers {
        if let Some(rest) = trimmed.strip_prefix(marker) {
            return Some(rest.trim_start());
        }
    }
    None
}

/// Return the tag when a comment body opens with `TAG:`.
fn rationale_tag(body: &str) -> Option<&'static str> {
    let upper: String = body.chars().take(12).flat_map(char::to_uppercase).collect();
    RATIONALE_TAGS
        .iter()
        .find(|tag| {
            upper
                .strip_prefix(**tag)
                .is_some_and(|rest| rest.starts_with(':'))
        })
        .copied()
}

/// Canonical label for a design reference, so `adr 11` and `ADR-0011` collapse
/// onto one node.
fn normalize_doc_ref(kind: &str, number: &str) -> String {
    let kind = kind.to_uppercase();
    if kind == "ADR" {
        format!("ADR-{number:0>4}")
    } else {
        format!("{kind}-{number}")
    }
}

/// Accumulates nodes and edges while keeping ids unique within one file.
struct Collector<'a> {
    path_string: String,
    file_id: String,
    /// Node ids the AST pass already produced for this file, used to attach
    /// docstrings to the definition they document.
    existing: &'a HashSet<&'a str>,
    result: ExtractionResult,
    seen_nodes: HashSet<String>,
    seen_doc_refs: HashSet<String>,
}

impl<'a> Collector<'a> {
    fn new(path: &Path, existing: &'a HashSet<&'a str>) -> Self {
        let path_string = path_str(path);
        let file_id = make_id(&[&path_string]);
        Self {
            path_string,
            file_id,
            existing,
            result: ExtractionResult::default(),
            seen_nodes: HashSet::new(),
            seen_doc_refs: HashSet::new(),
        }
    }

    fn edge(&mut self, source: String, target: String, relation: &str, line: usize) {
        self.result.edges.push(GraphEdge {
            source,
            target,
            relation: relation.to_string(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: self.path_string.clone(),
            source_location: Some(format!("L{line}")),
            weight: 1.0,
            provenance: Some("rationale".to_string()),
            extra: HashMap::new(),
        });
    }

    /// Add a rationale node for `text` and join it to `parent_id`.
    fn add_rationale(&mut self, text: &str, line: usize, parent_id: &str) {
        let label = label_from(text);
        if label.is_empty() {
            return;
        }
        let id = make_id(&[&self.path_string, "rationale", &line.to_string()]);
        if self.seen_nodes.insert(id.clone()) {
            self.result.nodes.push(GraphNode {
                id: id.clone(),
                label,
                source_file: self.path_string.clone(),
                source_location: Some(format!("L{line}")),
                node_type: NodeType::Concept,
                community: None,
                extra: HashMap::from([(
                    "file_type".to_string(),
                    serde_json::Value::String("rationale".to_string()),
                )]),
            });
        }
        self.edge(id, parent_id.to_string(), "rationale_for", line);
    }

    /// Add a shared design-reference node and cite it from this file.
    fn add_doc_ref(&mut self, label: &str, line: usize) {
        if !self.seen_doc_refs.insert(label.to_string()) {
            return;
        }
        let id = make_id(&["docref", label]);
        if self.seen_nodes.insert(id.clone()) {
            self.result.nodes.push(GraphNode {
                id: id.clone(),
                label: label.to_string(),
                source_file: self.path_string.clone(),
                source_location: Some(format!("L{line}")),
                node_type: NodeType::Concept,
                community: None,
                extra: HashMap::from([(
                    "file_type".to_string(),
                    serde_json::Value::String("doc_ref".to_string()),
                )]),
            });
        }
        let file_id = self.file_id.clone();
        self.edge(file_id, id, "cites", line);
    }

    /// Node id the AST pass gave a definition in this file.
    ///
    /// Extractors disagree on how methods are named — tree-sitter qualifies
    /// them as `Class.method` while the regex fallback uses the bare name — so
    /// try each spelling and take the one that exists. A definition the AST
    /// pass never produced falls back to the file node, which keeps the
    /// rationale in the graph instead of dangling into nothing.
    fn definition_id(&self, class: Option<&str>, name: &str) -> String {
        let mut candidates = Vec::with_capacity(2);
        if let Some(class) = class {
            candidates.push(make_id(&[&self.path_string, &format!("{class}.{name}")]));
        }
        candidates.push(make_id(&[&self.path_string, name]));
        candidates
            .into_iter()
            .find(|id| self.existing.contains(id.as_str()))
            .unwrap_or_else(|| self.file_id.clone())
    }
}

/// Auto-generated Python whose module docstring is a revision annotation or
/// codegen boilerplate rather than architectural rationale.
fn is_autogenerated_python(source: &str) -> bool {
    let head: String = source.chars().take(2048).collect();
    if [
        "DO NOT EDIT",
        "@generated",
        "Generated by the protocol buffer",
    ]
    .iter()
    .any(|marker| head.contains(marker))
    {
        return true;
    }
    // Alembic / Flask-Migrate revision files.
    let has_revision = head.lines().any(|l| {
        let t = l.trim_start();
        t.starts_with("revision")
            && t.trim_start_matches("revision")
                .trim_start()
                .starts_with([':', '='])
    });
    if has_revision && head.contains("def upgrade(") && head.contains("down_revision") {
        return true;
    }
    // Django migrations.
    head.contains("class Migration(migrations.Migration)") && head.contains("operations")
}

/// Read a `"""…"""` / `'''…'''` docstring starting at `start`.
///
/// Returns the docstring body and its 1-based line. Python only treats a
/// string long enough to say something as rationale, so short ones are
/// skipped, matching the reference implementation's 20-character floor.
fn read_docstring(lines: &[&str], start: usize) -> Option<(String, usize)> {
    let first = lines.get(start)?.trim_start();
    let quote = if first.starts_with("\"\"\"") {
        "\"\"\""
    } else if first.starts_with("'''") {
        "'''"
    } else {
        return None;
    };

    let after_open = &first[quote.len()..];
    let mut body = String::new();
    if let Some(end) = after_open.find(quote) {
        body.push_str(&after_open[..end]);
    } else {
        body.push_str(after_open);
        let mut closed = false;
        for line in lines.iter().skip(start + 1) {
            if let Some(end) = line.find(quote) {
                body.push(' ');
                body.push_str(&line[..end]);
                closed = true;
                break;
            }
            body.push(' ');
            body.push_str(line);
        }
        if !closed {
            return None;
        }
    }

    let text = body.trim().to_string();
    if text.chars().count() > 20 {
        Some((text, start + 1))
    } else {
        None
    }
}

/// Index of the first line of a definition's body: the line after the one that
/// closes the signature. Handles signatures wrapped over several lines.
fn body_start(lines: &[&str], def_line: usize) -> Option<usize> {
    for (offset, line) in lines.iter().skip(def_line).take(12).enumerate() {
        if line.trim_end().ends_with(':') {
            return Some(def_line + offset + 1);
        }
    }
    None
}

/// First identifier after a `def`/`class` keyword.
fn definition_name(trimmed: &str, keyword: &str) -> Option<String> {
    let rest = trimmed.strip_prefix(keyword)?.trim_start();
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() { None } else { Some(name) }
}

/// Module, class, and function docstrings, attached to the matching AST node.
fn extract_python_docstrings(lines: &[&str], source: &str, collector: &mut Collector) {
    // Module docstring: the first statement in the file.
    if !is_autogenerated_python(source) {
        let first_code = lines
            .iter()
            .position(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'));
        if let Some(idx) = first_code
            && let Some((text, line)) = read_docstring(lines, idx)
        {
            let file_id = collector.file_id.clone();
            collector.add_rationale(&text, line, &file_id);
        }
    }

    // Innermost enclosing class and its indent, so methods can be qualified.
    let mut class_context: Option<(String, usize)> = None;

    for (idx, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim_start();
        let indent = raw.len() - trimmed.len();

        // Leaving the class body clears the context.
        if let Some((_, class_indent)) = &class_context
            && !trimmed.is_empty()
            && indent <= *class_indent
            && !trimmed.starts_with("class ")
        {
            class_context = None;
        }

        let (keyword, is_class) = if trimmed.starts_with("class ") {
            ("class ", true)
        } else if trimmed.starts_with("def ") {
            ("def ", false)
        } else if trimmed.starts_with("async def ") {
            ("async def ", false)
        } else {
            continue;
        };
        let Some(name) = definition_name(trimmed, keyword) else {
            continue;
        };

        let parent = if is_class {
            collector.definition_id(None, &name)
        } else {
            let class = class_context
                .as_ref()
                .filter(|(_, class_indent)| indent > *class_indent)
                .map(|(class_name, _)| class_name.as_str());
            collector.definition_id(class, &name)
        };

        if let Some(body) = body_start(lines, idx)
            && let Some((text, line)) = read_docstring(lines, body)
        {
            collector.add_rationale(&text, line, &parent);
        }

        if is_class {
            class_context = Some((name, indent));
        }
    }
}

/// Extract rationale comments, Python docstrings, and design references.
///
/// `existing` holds the node ids the AST pass already produced for this file,
/// so docstrings can attach to the definition they document. Returns only the
/// added nodes and edges; the caller merges them into the file's result.
pub fn extract_rationale(
    path: &Path,
    source: &str,
    lang: &str,
    existing: &HashSet<&str>,
) -> ExtractionResult {
    let markers = comment_markers(lang);
    if markers.is_empty() {
        return ExtractionResult::default();
    }

    let lines: Vec<&str> = source.lines().collect();
    let mut collector = Collector::new(path, existing);

    for (idx, raw) in lines.iter().enumerate() {
        let line = idx + 1;
        let Some(body) = strip_marker(raw, markers) else {
            continue;
        };
        if rationale_tag(body).is_some() {
            let file_id = collector.file_id.clone();
            collector.add_rationale(body, line, &file_id);
        }
        // Design references are only trusted inside comments, never in code
        // or string literals.
        for cap in DOC_REF_RE.captures_iter(body) {
            let label = normalize_doc_ref(&cap[1], &cap[2]);
            collector.add_doc_ref(&label, line);
        }
    }

    if lang == "python" {
        extract_python_docstrings(&lines, source, &mut collector);
    }

    collector.result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Run with no pre-existing AST nodes: every parent falls back to the file.
    fn run(name: &str, source: &str, lang: &str) -> ExtractionResult {
        extract_rationale(&PathBuf::from(name), source, lang, &HashSet::new())
    }

    /// Run against the node ids an AST pass would have produced for `names`.
    fn run_with_defs(name: &str, source: &str, lang: &str, names: &[&str]) -> ExtractionResult {
        let path = PathBuf::from(name);
        let ps = path_str(&path);
        let ids: Vec<String> = names.iter().map(|n| make_id(&[&ps, n])).collect();
        let existing: HashSet<&str> = ids.iter().map(String::as_str).collect();
        extract_rationale(&path, source, lang, &existing)
    }

    fn labels(result: &ExtractionResult) -> Vec<&str> {
        result.nodes.iter().map(|n| n.label.as_str()).collect()
    }

    #[test]
    fn finds_hash_rationale_comments() {
        let src = "x = 1\n# NOTE: this is deliberate for perf\n# just a comment\n";
        let result = run("a.py", src, "python");
        assert_eq!(labels(&result), vec!["NOTE: this is deliberate for perf"]);
        assert_eq!(result.edges[0].relation, "rationale_for");
        assert_eq!(result.edges[0].source_location.as_deref(), Some("L2"));
    }

    #[test]
    fn finds_slash_and_star_rationale_comments() {
        let src = "// WHY: avoids a lock\nlet x = 1;\n/*\n * HACK: upstream bug\n */\n";
        let result = run("a.rs", src, "rust");
        assert_eq!(
            labels(&result),
            vec!["WHY: avoids a lock", "HACK: upstream bug"]
        );
    }

    #[test]
    fn recognizes_every_tag() {
        for tag in RATIONALE_TAGS {
            let src = format!("// {tag}: something\n");
            let result = run("a.rs", &src, "rust");
            assert_eq!(result.nodes.len(), 1, "tag {tag} not recognized");
        }
    }

    #[test]
    fn respects_language_comment_syntax() {
        // `--` is a comment in SQL but not in Rust.
        assert_eq!(run("a.sql", "-- NOTE: index hint\n", "sql").nodes.len(), 1);
        assert_eq!(run("a.rs", "-- NOTE: index hint\n", "rust").nodes.len(), 0);
        assert_eq!(run("a.json", "// NOTE: x\n", "json").nodes.len(), 0);
    }

    #[test]
    fn ignores_tags_outside_comments() {
        let src = "let msg = \"NOTE: not a comment\";\n";
        assert_eq!(run("a.rs", src, "rust").nodes.len(), 0);
    }

    #[test]
    fn collects_and_normalizes_design_references() {
        let src = "// see ADR-11 and RFC 793\n// also adr 0011 again\n";
        let result = run("a.rs", src, "rust");
        let mut found: Vec<&str> = result
            .nodes
            .iter()
            .filter(|n| n.extra.get("file_type").and_then(|v| v.as_str()) == Some("doc_ref"))
            .map(|n| n.label.as_str())
            .collect();
        found.sort_unstable();
        // "ADR-11" and "adr 0011" collapse onto one canonical node.
        assert_eq!(found, vec!["ADR-0011", "RFC-793"]);
        assert!(result.edges.iter().any(|e| e.relation == "cites"));
    }

    #[test]
    fn doc_references_are_ignored_in_code() {
        let src = "let adr = \"ADR-0011\";\n";
        assert_eq!(run("a.rs", src, "rust").nodes.len(), 0);
    }

    const PY_SRC: &str = "\"\"\"Module level rationale that is long enough.\"\"\"\n\
                          \n\
                          class Widget:\n\
                          \x20   \"\"\"A widget that does something useful here.\"\"\"\n\
                          \n\
                          \x20   def render(self):\n\
                          \x20       \"\"\"Renders the widget onto the target surface.\"\"\"\n\
                          \x20       return None\n\
                          \n\
                          def build(a,\n\
                          \x20        b):\n\
                          \x20   \"\"\"Builds the widget from its two parts.\"\"\"\n\
                          \x20   return None\n";

    #[test]
    fn extracts_python_docstrings_onto_definitions() {
        let result = run_with_defs(
            "w.py",
            PY_SRC,
            "python",
            &["Widget", "Widget.render", "build"],
        );
        let found = labels(&result);
        assert!(found.contains(&"Module level rationale that is long enough."));
        assert!(found.contains(&"A widget that does something useful here."));
        assert!(found.contains(&"Renders the widget onto the target surface."));
        // Multi-line signature still resolves to the body.
        assert!(found.contains(&"Builds the widget from its two parts."));

        let ps = path_str(&PathBuf::from("w.py"));
        let target_of = |label: &str| -> String {
            let id = &result
                .nodes
                .iter()
                .find(|n| n.label == label)
                .expect(label)
                .id;
            result
                .edges
                .iter()
                .find(|e| &e.source == id)
                .expect("edge")
                .target
                .clone()
        };
        // Each docstring lands on the definition it documents, not the file.
        assert_eq!(
            target_of("A widget that does something useful here."),
            make_id(&[&ps, "Widget"])
        );
        assert_eq!(
            target_of("Renders the widget onto the target surface."),
            make_id(&[&ps, "Widget.render"])
        );
        assert_eq!(
            target_of("Builds the widget from its two parts."),
            make_id(&[&ps, "build"])
        );
        assert_eq!(
            target_of("Module level rationale that is long enough."),
            make_id(&[&ps])
        );
    }

    #[test]
    fn method_docstrings_fall_back_to_the_bare_name() {
        // The regex extractor names methods without the class prefix.
        let result = run_with_defs("w.py", PY_SRC, "python", &["Widget", "render", "build"]);
        let node = result
            .nodes
            .iter()
            .find(|n| n.label.starts_with("Renders the widget"))
            .expect("render docstring");
        let target = &result
            .edges
            .iter()
            .find(|e| e.source == node.id)
            .expect("edge")
            .target;
        assert_eq!(
            target,
            &make_id(&[&path_str(&PathBuf::from("w.py")), "render"])
        );
    }

    #[test]
    fn unknown_definitions_anchor_to_the_file() {
        // No AST nodes at all: docstrings still land in the graph, on the file.
        let result = run("w.py", PY_SRC, "python");
        let ps = path_str(&PathBuf::from("w.py"));
        let file_id = make_id(&[&ps]);
        assert!(result.edges.iter().all(|e| e.target == file_id));
        assert!(!result.nodes.is_empty());
    }

    #[test]
    fn skips_short_docstrings() {
        let src = "\"\"\"Short.\"\"\"\n";
        assert_eq!(run("a.py", src, "python").nodes.len(), 0);
    }

    #[test]
    fn skips_autogenerated_module_docstrings() {
        let src = "\"\"\"Revision ID: abc123 with plenty of text here.\"\"\"\n\
                   revision = 'abc123'\n\
                   down_revision = None\n\
                   def upgrade():\n\
                   \x20   pass\n";
        let result = run("m.py", src, "python");
        assert!(
            !labels(&result).iter().any(|l| l.contains("Revision ID")),
            "auto-generated module docstring should be skipped"
        );
    }

    #[test]
    fn docstrings_only_run_for_python() {
        let src = "\"\"\"This looks like a docstring but is not Python.\"\"\"\n";
        assert_eq!(run("a.rs", src, "rust").nodes.len(), 0);
    }

    #[test]
    fn labels_are_capped_and_single_line() {
        let src = format!("// NOTE: {}\n", "x".repeat(200));
        let result = run("a.rs", &src, "rust");
        assert_eq!(result.nodes[0].label.chars().count(), MAX_LABEL_LEN);
        assert!(!result.nodes[0].label.contains('\n'));
    }

    #[test]
    fn rationale_nodes_are_tagged_for_downstream_styling() {
        let result = run("a.rs", "// NOTE: keep this\n", "rust");
        assert_eq!(result.nodes[0].node_type, NodeType::Concept);
        assert_eq!(
            result.nodes[0]
                .extra
                .get("file_type")
                .and_then(|v| v.as_str()),
            Some("rationale")
        );
        assert_eq!(result.edges[0].provenance.as_deref(), Some("rationale"));
    }
}
