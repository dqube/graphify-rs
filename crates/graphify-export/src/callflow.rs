//! Call-flow architecture HTML export.
//!
//! Port of the Python `graphify export callflow-html` generator. Produces a
//! self-contained architecture document rather than a single diagram:
//!
//! - dark-themed CSS with a sticky navigation bar
//! - a section-level architecture overview flowchart
//! - one flowchart, call-detail table, and key-file cards per section
//! - sections derived from community clustering, matched against architecture
//!   archetypes (extraction pipeline, graph build, serving API, …)
//! - generated copy in English or Chinese, chosen from the graph's contents
//!
//! Mermaid is loaded from a CDN and diagrams get zoom/pan controls.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use graphify_core::confidence::Confidence;
use graphify_core::graph::KnowledgeGraph;
use graphify_core::model::{GraphEdge, GraphNode};
use sha2::{Digest, Sha256};
use tracing::info;

// ──────────────────────────────────────────────
// 1. CSS template (fixed, project-agnostic)
// ──────────────────────────────────────────────

const CSS: &str = r#":root {
  --bg: #0f172a; --surface: #1e293b; --border: #334155;
  --text: #e2e8f0; --muted: #94a3b8; --accent: #38bdf8;
  --warn: #fbbf24; --err: #f87171; --ok: #34d399;
}
* { box-sizing: border-box; margin: 0; padding: 0; }
body { font-family: 'Segoe UI', system-ui, -apple-system, sans-serif; background: var(--bg); color: var(--text); line-height: 1.7; }
.container { max-width: 1200px; margin: 0 auto; padding: 40px 24px; }
h1 { font-size: 2.4rem; margin-bottom: 8px; background: linear-gradient(135deg, var(--accent), #a78bfa); -webkit-background-clip: text; -webkit-text-fill-color: transparent; }
h2 { font-size: 1.7rem; margin: 48px 0 16px; padding-bottom: 8px; border-bottom: 2px solid var(--accent); }
h3 { font-size: 1.25rem; margin: 32px 0 12px; color: var(--accent); }
h4 { font-size: 1.05rem; margin: 20px 0 8px; color: var(--warn); }
p { margin: 8px 0; color: var(--muted); }
.subtitle { color: var(--muted); font-size: 1.1rem; margin-bottom: 32px; }
.mermaid { background: var(--surface); border: 1px solid var(--border); border-radius: 12px; padding: 24px; margin: 20px 0; overflow-x: auto; position: relative; }
.mermaid.is-enhanced { padding: 0; overflow: hidden; min-height: 260px; }
.mermaid-viewport { padding: 54px 24px 24px; overflow: hidden; cursor: grab; touch-action: none; min-height: 260px; }
.mermaid-viewport.is-dragging { cursor: grabbing; }
.mermaid-viewport svg { max-width: none !important; height: auto; transform-origin: 0 0; transition: transform 120ms ease; }
.mermaid-toolbar { position: absolute; top: 10px; right: 10px; z-index: 3; display: flex; align-items: center; gap: 6px; padding: 6px; background: rgba(15,23,42,0.92); border: 1px solid var(--border); border-radius: 8px; box-shadow: 0 8px 24px rgba(0,0,0,0.28); }
.mermaid-toolbar button, .mermaid-toolbar .zoom-level { height: 28px; min-width: 32px; border: 1px solid var(--border); border-radius: 6px; background: #1e293b; color: var(--text); font: 600 0.78rem system-ui, sans-serif; display: inline-flex; align-items: center; justify-content: center; }
.mermaid-toolbar button { cursor: pointer; }
.mermaid-toolbar button:hover { border-color: var(--accent); color: var(--accent); }
.mermaid-toolbar .zoom-level { min-width: 52px; color: var(--muted); background: transparent; }
.call-table { width: 100%; border-collapse: collapse; margin: 16px 0; font-size: 0.92rem; }
.call-table th { background: #1a2744; color: var(--accent); text-align: left; padding: 10px 14px; border: 1px solid var(--border); }
.call-table td { padding: 8px 14px; border: 1px solid var(--border); vertical-align: top; }
.call-table tr:nth-child(even) { background: rgba(255,255,255,0.02); }
.tag { display: inline-block; padding: 2px 8px; border-radius: 4px; font-size: 0.8rem; font-weight: 600; }
.tag-async { background: #7c3aed33; color: #a78bfa; }
.tag-class { background: #05966933; color: var(--ok); }
.tag-func { background: #2563eb33; color: var(--accent); }
.tag-cmd { background: #d9770633; color: var(--warn); }
.tag-endpoint { background: #dc262633; color: var(--err); }
.tag-hook { background: #db277733; color: #f472b6; }
.card { background: var(--surface); border: 1px solid var(--border); border-radius: 10px; padding: 20px; margin: 16px 0; }
.grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(340px, 1fr)); gap: 16px; margin: 16px 0; }
.arrow-chain { font-family: 'Fira Code', monospace; font-size: 0.85rem; color: var(--accent); padding: 10px; background: rgba(56,189,248,0.06); border-radius: 6px; }
code { font-family: 'Fira Code', 'Cascadia Code', monospace; background: rgba(255,255,255,0.06); padding: 1px 6px; border-radius: 3px; font-size: 0.88em; }
ul, ol { margin: 8px 0 8px 24px; color: var(--muted); }
li { margin: 4px 0; }
a { color: var(--accent); }
hr { border: none; border-top: 1px solid var(--border); margin: 40px 0; }
.nav { position: sticky; top: 0; background: var(--bg); z-index: 10; padding: 12px 0; border-bottom: 1px solid var(--border); display: flex; gap: 20px; flex-wrap: wrap; font-size: 0.9rem; }
.nav a { text-decoration: none; }
.nav a:hover { text-decoration: underline; }
@media (max-width: 768px) { .container { padding: 16px; } h1 { font-size: 1.8rem; } }
"#;

/// Tunables for diagram density, mirroring the Python CLI defaults.
#[derive(Debug, Clone)]
pub struct CallflowOptions {
    /// Maximum number of documented sections, including the overview.
    pub max_sections: usize,
    /// Mermaid font/spacing multiplier.
    pub diagram_scale: f64,
    /// Maximum nodes drawn in one section flowchart.
    pub max_diagram_nodes: usize,
    /// Maximum edges drawn in one section flowchart.
    pub max_diagram_edges: usize,
    /// `"auto"`, `"en"`, or a `zh…` locale for generated copy.
    pub lang: String,
    /// Display name for the project; inferred from the output path when empty.
    pub project_name: String,
}

impl Default for CallflowOptions {
    fn default() -> Self {
        Self {
            max_sections: 15,
            diagram_scale: 1.0,
            max_diagram_nodes: 18,
            max_diagram_edges: 24,
            lang: "auto".to_string(),
            project_name: String::new(),
        }
    }
}

// ──────────────────────────────────────────────
// 2. Text helpers
// ──────────────────────────────────────────────

/// HTML-escape `&`, `<`, `>` — Python's `escape(text, quote=False)`.
fn esc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
    out
}

/// HTML-escape including quotes, for attribute values.
fn esc_attr(text: &str) -> String {
    esc(text).replace('"', "&quot;").replace('\'', "&#x27;")
}

/// Collapse all runs of whitespace into single spaces.
fn collapse_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sanitize text for a Mermaid node label: strip the characters Mermaid reads
/// as syntax, then HTML-escape what is left.
fn safe_mermaid_text(text: &str) -> String {
    let mut t = text.replace('"', "'");
    t = t.replace('`', "");
    t = t.replace('#', "");
    t = t.replace('|', " ");
    t = t.replace(['{', '}'], "");
    t = t
        .replace("->>", " to ")
        .replace("-->", " to ")
        .replace("->", " to ");
    esc(&collapse_ws(&t))
}

/// Keep generated HTML comments well-formed.
fn html_comment_text(text: &str) -> String {
    text.replace("--", "- -").replace('\n', " ")
}

/// Truncate to `limit` characters with an ellipsis, after collapsing whitespace.
fn truncate_text(text: &str, limit: usize) -> String {
    let text = collapse_ws(text);
    if text.chars().count() <= limit {
        return text;
    }
    let mut s: String = text.chars().take(limit.saturating_sub(3)).collect();
    while s.ends_with(' ') {
        s.pop();
    }
    s.push_str("...");
    s
}

/// First 8 hex characters of the SHA-256 of `raw`, used to keep generated
/// identifiers unique after slugification.
fn short_digest(raw: &str) -> String {
    let digest = Sha256::digest(raw.as_bytes());
    let mut out = String::with_capacity(8);
    for byte in &digest[..4] {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Build a Mermaid-safe ASCII identifier with a hash suffix to avoid collisions.
///
/// Mermaid IDs must match `[a-zA-Z][a-zA-Z0-9_]*` — no dots, hyphens, slashes.
fn stable_ascii_id(raw: &str, prefix: &str, limit: usize) -> String {
    let digest = short_digest(raw);

    // Collapse every run of non-alphanumeric characters into a single '_'.
    let mut slug = String::with_capacity(raw.len());
    let mut pending_sep = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            if pending_sep && !slug.is_empty() {
                slug.push('_');
            }
            pending_sep = false;
            slug.push(c);
        } else {
            pending_sep = true;
        }
    }
    let mut slug = slug.trim_matches('_').to_string();
    if slug.is_empty() {
        slug = prefix.to_string();
    }
    if slug.starts_with(|c: char| c.is_ascii_digit()) {
        slug = format!("{prefix}_{slug}");
    }
    let truncated: String = slug.chars().take(limit).collect();
    format!("{}_{digest}", truncated.trim_end_matches('_'))
}

fn node_mermaid_id(node_id: &str) -> String {
    stable_ascii_id(node_id, "node", 48)
}

fn mermaid_section_id(section_id: &str) -> String {
    stable_ascii_id(section_id, "section", 48).to_uppercase()
}

/// Shorten a path for display to at most its final three components.
fn safe_file_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    if parts.len() > 3 {
        parts[parts.len() - 3..].join("/")
    } else {
        normalized
    }
}

/// Final path component of a path-like string.
fn base_name(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized
        .rsplit('/')
        .next()
        .unwrap_or(normalized.as_str())
        .to_string()
}

/// Generate a stable, unique HTML anchor ID.
fn html_anchor_id(raw: &str, fallback: &str, used: &mut HashSet<String>) -> String {
    let slugify = |value: &str| -> String {
        let mut out = String::with_capacity(value.len());
        let mut pending_sep = false;
        for c in value.chars().flat_map(char::to_lowercase) {
            if c.is_ascii_alphanumeric() {
                if pending_sep && !out.is_empty() {
                    out.push('-');
                }
                pending_sep = false;
                out.push(c);
            } else {
                pending_sep = true;
            }
        }
        out.trim_matches('-').to_string()
    };

    let mut base = slugify(raw);
    if base.is_empty() {
        base = slugify(fallback);
    }
    if base.is_empty() {
        base = "section".to_string();
    }
    base = base.chars().take(48).collect::<String>();
    base = base.trim_matches('-').to_string();
    if base.is_empty() {
        base = "section".to_string();
    }

    let mut candidate = base.clone();
    if used.contains(&candidate) {
        candidate = format!("{base}-{}", &short_digest(raw)[..6]);
    }
    let mut suffix = 2;
    while used.contains(&candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    used.insert(candidate.clone());
    candidate
}

// ──────────────────────────────────────────────
// 3. Localization
// ──────────────────────────────────────────────

fn is_zh(lang: &str) -> bool {
    lang.to_lowercase().starts_with("zh")
}

fn pick_text<'a>(lang: &str, zh: &'a str, en: &'a str) -> &'a str {
    if is_zh(lang) { zh } else { en }
}

/// Resolve `auto` language by looking for CJK characters in labels and paths.
fn detect_lang(lang: &str, nodes: &[&GraphNode], labels: &HashMap<usize, String>) -> String {
    if !lang.is_empty() && !lang.eq_ignore_ascii_case("auto") {
        return lang.to_string();
    }
    let has_cjk = |s: &str| s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));

    let mut label_values: Vec<&String> = labels.values().collect();
    label_values.sort();
    let cjk = label_values.iter().take(50).any(|v| has_cjk(v))
        || nodes.iter().take(200).any(|n| has_cjk(&n.label))
        || nodes.iter().take(100).any(|n| has_cjk(&n.source_file));
    if cjk {
        "zh-CN".to_string()
    } else {
        "en".to_string()
    }
}

// ──────────────────────────────────────────────
// 4. Node and edge classification
// ──────────────────────────────────────────────

/// Documents, rationale, and design references get different diagram styling
/// and table copy than code.
///
/// An explicit `file_type` recorded by an extractor wins; otherwise it is
/// inferred from the file extension.
fn file_type_of(node: &GraphNode) -> &'static str {
    if let Some(explicit) = node.extra.get("file_type").and_then(|v| v.as_str()) {
        return match explicit {
            "rationale" => "rationale",
            "doc_ref" => "doc_ref",
            "document" => "document",
            _ => "code",
        };
    }
    let lower = node.source_file.to_lowercase();
    if [".md", ".mdx", ".rst", ".txt"]
        .iter()
        .any(|ext| lower.ends_with(ext))
    {
        "document"
    } else {
        "code"
    }
}

/// Classify a graph node for Mermaid styling and table tags.
fn node_kind(node: &GraphNode) -> &'static str {
    let label = node.label.to_lowercase();
    let source_file = node.source_file.to_lowercase();
    let node_type = node.node_type.to_string().to_lowercase();

    match node_type.as_str() {
        "class" | "struct" | "interface" | "enum" | "trait" => return "klass",
        "module" | "file" | "package" | "namespace" => return "module",
        _ => {}
    }
    if node_type == "concept" || matches!(file_type_of(node), "document" | "rationale" | "doc_ref")
    {
        return "concept";
    }
    if source_file.contains("test") || label.starts_with("test_") || source_file.contains("spec") {
        return "test";
    }
    if ["endpoint", "router", "api", "route"]
        .iter()
        .any(|w| label.contains(w))
    {
        return "api";
    }
    if ["cli", "command", "click", "typer"]
        .iter()
        .any(|w| label.contains(w))
    {
        return "entry";
    }
    if ["async", "await", "stream", "sse"]
        .iter()
        .any(|w| label.contains(w))
    {
        return "async";
    }

    let raw = node.label.as_str();
    let hook_like = raw.starts_with("use")
        && raw.chars().count() > 3
        && raw
            .chars()
            .nth(3)
            .is_some_and(|c| c.is_uppercase() || c == '_' || c == '-');
    let ui_file = [".tsx", ".jsx", ".vue", ".svelte"]
        .iter()
        .any(|ext| source_file.ends_with(ext));
    if ["component", "props", "hook", "store"]
        .iter()
        .any(|w| label.contains(w))
        || hook_like
        || ui_file
    {
        return "ui";
    }
    if raw.chars().next().is_some_and(char::is_uppercase) && !raw.ends_with("()") {
        return "klass";
    }
    const MODULE_EXTS: &[&str] = &[
        ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".rs", ".java", ".kt", ".rb", ".php", ".cs",
        ".swift", ".vue", ".svelte",
    ];
    if MODULE_EXTS.iter().any(|ext| raw.ends_with(ext)) {
        return "module";
    }
    "function"
}

/// Convert graph labels into short labels people can scan in a diagram.
fn humanize_label(label: &str, source_file: &str) -> String {
    let label = label.trim();
    if label.is_empty() {
        return if source_file.is_empty() {
            "Unknown".to_string()
        } else {
            base_name(source_file)
        };
    }
    if label.starts_with('.') && label.ends_with("()") {
        return label[1..].to_string();
    }
    const SRC_EXTS: &[&str] = &[
        ".py", ".ts", ".tsx", ".js", ".jsx", ".go", ".rs", ".java", ".rb",
    ];
    if SRC_EXTS.iter().any(|ext| label.ends_with(ext)) {
        return base_name(label);
    }
    let mut label = label.to_string();
    if label.contains('_') && !label.contains(' ') && label.chars().count() > 28 {
        let parts: Vec<&str> = label.split('_').filter(|p| !p.is_empty()).collect();
        if !parts.is_empty() {
            label = parts[parts.len().saturating_sub(3)..].join(" ");
        }
    }
    truncate_text(&label, 42)
}

/// Map graph edge relation names to short diagram labels.
fn relation_label(relation: &str, lang: &str) -> String {
    let relation = relation.trim();
    let mapped = if is_zh(lang) {
        match relation {
            "calls" => "调用",
            "uses" => "使用",
            "imports" | "imports_from" => "导入",
            "method" => "方法",
            "contains" => "包含",
            "rationale_for" => "说明",
            "conceptually_related_to" => "相关",
            "participate_in" => "参与",
            "form" => "组成",
            "references" => "引用",
            _ => "",
        }
    } else {
        match relation {
            "calls" => "calls",
            "uses" => "uses",
            "imports" | "imports_from" => "imports",
            "method" => "method",
            "contains" => "contains",
            "rationale_for" => "explains",
            "conceptually_related_to" => "relates",
            "participate_in" => "joins",
            "form" => "forms",
            "references" => "references",
            _ => "",
        }
    };
    if mapped.is_empty() {
        safe_mermaid_text(&relation.replace('_', " "))
    } else {
        safe_mermaid_text(mapped)
    }
}

/// Decide whether to auto-include an edge in Mermaid output.
fn should_include_edge(edge: &GraphEdge) -> bool {
    match edge.confidence {
        Confidence::Extracted => true,
        Confidence::Inferred => edge.confidence_score >= 0.85,
        Confidence::Ambiguous => false,
    }
}

/// Filter to edges that make a readable call-flow diagram.
fn preferred_edges<'a>(edges: &[&'a GraphEdge], allow_structure: bool) -> Vec<&'a GraphEdge> {
    const PRIMARY: &[&str] = &["calls", "uses", "method", "imports", "imports_from"];
    const SECONDARY: &[&str] = &["contains", "rationale_for", "conceptually_related_to"];

    let selected: Vec<&GraphEdge> = edges
        .iter()
        .filter(|e| should_include_edge(e))
        .filter(|e| {
            PRIMARY.contains(&e.relation.as_str())
                || (allow_structure && SECONDARY.contains(&e.relation.as_str()))
        })
        .copied()
        .collect();
    if !selected.is_empty() {
        return selected;
    }
    edges
        .iter()
        .filter(|e| should_include_edge(e))
        .copied()
        .collect()
}

/// Rank edges by confidence and usefulness for diagrams.
fn edge_score(edge: &GraphEdge) -> f64 {
    let mut score = edge.confidence_score;
    if edge.confidence == Confidence::Extracted {
        score += 2.0;
    }
    match edge.relation.as_str() {
        "calls" | "uses" | "method" => score += 1.0,
        "imports" | "imports_from" => score += 0.6,
        "contains" => score -= 0.2,
        "rationale_for" => score -= 0.6,
        _ => {}
    }
    score
}

/// Descending edge-score order. Used with a stable sort, so equal scores keep
/// graph insertion order — the same tiebreak Python's `sorted` gives.
fn by_edge_score_desc(a: &&GraphEdge, b: &&GraphEdge) -> std::cmp::Ordering {
    edge_score(b)
        .partial_cmp(&edge_score(a))
        .unwrap_or(std::cmp::Ordering::Equal)
}

// ──────────────────────────────────────────────
// 5. Mermaid scaffolding
// ──────────────────────────────────────────────

/// Mermaid init directive that scales diagrams via Mermaid config.
fn mermaid_init(scale: f64, direction: &str) -> String {
    let scale = scale.clamp(0.65, 1.8);
    let config = serde_json::json!({
        "theme": "dark",
        "themeVariables": {
            // One decimal place, matching Python's `round(15 * scale, 1)`.
            "fontSize": format!("{:.1}px", ((15.0 * scale) * 10.0).round() / 10.0),
            "fontFamily": "Segoe UI, system-ui, sans-serif",
            "primaryColor": "#1e293b",
            "primaryTextColor": "#e2e8f0",
            "primaryBorderColor": "#38bdf8",
            "secondaryColor": "#0f172a",
            "tertiaryColor": "#334155",
            "lineColor": "#64748b",
            "textColor": "#e2e8f0",
        },
        "flowchart": {
            "htmlLabels": true,
            "curve": "basis",
            "nodeSpacing": (48.0 * scale).round() as i64,
            "rankSpacing": (64.0 * scale).round() as i64,
            "padding": (14.0 * scale).round() as i64,
            "diagramPadding": (10.0 * scale).round() as i64,
            "useMaxWidth": true,
        },
    });
    format!(
        "%%{{init: {}}}%%\nflowchart {direction}",
        serde_json::to_string(&config).unwrap_or_default()
    )
}

/// Shared Mermaid-native styles for readable diagrams.
const MERMAID_CLASS_DEFS: &[&str] = &[
    "    classDef entry fill:#422006,stroke:#fbbf24,color:#fde68a,stroke-width:1px;",
    "    classDef api fill:#450a0a,stroke:#f87171,color:#fee2e2,stroke-width:1px;",
    "    classDef async fill:#2e1065,stroke:#a78bfa,color:#ede9fe,stroke-width:1px;",
    "    classDef klass fill:#064e3b,stroke:#34d399,color:#d1fae5,stroke-width:1px;",
    "    classDef ui fill:#831843,stroke:#f472b6,color:#fce7f3,stroke-width:1px;",
    "    classDef module fill:#172554,stroke:#60a5fa,color:#dbeafe,stroke-width:1px;",
    "    classDef test fill:#3f3f46,stroke:#a1a1aa,color:#f4f4f5,stroke-width:1px;",
    "    classDef concept fill:#292524,stroke:#a8a29e,color:#fafaf9,stroke-dasharray:4 3;",
    "    classDef function fill:#0f172a,stroke:#38bdf8,color:#e0f2fe,stroke-width:1px;",
];

// ──────────────────────────────────────────────
// 6. Section derivation
// ──────────────────────────────────────────────

/// A documented section: a named group of one or more communities.
struct Section {
    id: String,
    name: String,
    communities: Vec<String>,
}

/// Architecture archetypes in priority order. A community joins the archetype
/// whose keywords it matches most strongly, provided the score clears 2.
const SECTION_ARCHETYPES: &[(&str, &str, &str, &[&str])] = &[
    (
        "extract-pipeline",
        "提取管线",
        "Extraction Pipeline",
        &[
            "extract",
            "extractor",
            "tree",
            "sitter",
            "parser",
            "language",
            "python",
            "javascript",
            "typescript",
            "rust",
            "java",
            "go",
            "ast",
            "calls",
            "imports",
            "multilang",
        ],
    ),
    (
        "build-graph",
        "图谱构建",
        "Graph Build",
        &[
            "build",
            "graph",
            "merge",
            "dedup",
            "node",
            "edge",
            "hyperedge",
            "json",
            "schema",
            "normalize",
            "confidence",
        ],
    ),
    (
        "analysis-clustering",
        "分析聚类",
        "Analysis & Clustering",
        &[
            "cluster",
            "community",
            "leiden",
            "cohesion",
            "analyze",
            "god",
            "surprise",
            "question",
            "query",
            "path",
            "explain",
            "benchmark",
        ],
    ),
    (
        "outputs-docs",
        "输出文档",
        "Outputs & Docs",
        &[
            "export",
            "html",
            "wiki",
            "obsidian",
            "canvas",
            "svg",
            "graphml",
            "report",
            "callflow",
            "mermaid",
            "tree",
            "documentation",
        ],
    ),
    (
        "cli-skills",
        "CLI 与技能安装",
        "CLI & Skill Installers",
        &[
            "main",
            "install",
            "uninstall",
            "skill",
            "agent",
            "claude",
            "codex",
            "opencode",
            "aider",
            "copilot",
            "kiro",
            "vscode",
            "hook",
            "command",
        ],
    ),
    (
        "ingest-cache-update",
        "摄取与增量更新",
        "Ingestion & Updates",
        &[
            "ingest",
            "fetch",
            "download",
            "url",
            "html",
            "markdown",
            "cache",
            "manifest",
            "watch",
            "update",
            "incremental",
            "transcribe",
            "video",
            "audio",
            "google",
        ],
    ),
    (
        "serve-api",
        "服务 API",
        "Serving API",
        &[
            "serve", "api", "request", "response", "endpoint", "router", "handle", "upload",
            "search", "delete", "enrich",
        ],
    ),
    (
        "security-global",
        "安全与全局图",
        "Security & Global Graph",
        &[
            "security",
            "safe",
            "ssrf",
            "xss",
            "path",
            "traversal",
            "global",
            "prefix",
            "prune",
            "repo",
            "clone",
        ],
    ),
    (
        "tests-fixtures",
        "测试与样例",
        "Tests & Fixtures",
        &[
            "test", "tests", "fixture", "fixtures", "sample", "assert", "pytest", "mock",
        ],
    ),
];

/// Concatenated lowercase text describing a community, for keyword scoring.
fn community_text(nodes: &[&GraphNode], label: &str) -> String {
    let mut parts = vec![label.to_string()];
    for node in nodes.iter().take(80) {
        parts.push(node.label.clone());
        parts.push(node.source_file.clone());
        parts.push(node.node_type.to_string());
        parts.push(file_type_of(node).to_string());
    }
    parts.join(" ").to_lowercase()
}

/// Count whole-word occurrences of each keyword in `text`.
///
/// A match must not be flanked by `[a-z0-9]`, mirroring the Python lookarounds
/// so that `path` does not match inside `pathlib`.
fn keyword_score(text: &str, keywords: &[&str]) -> usize {
    let bytes = text.as_bytes();
    let is_word = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let mut score = 0;
    for keyword in keywords {
        let klen = keyword.len();
        if klen == 0 {
            continue;
        }
        let mut start = 0;
        while start < text.len() {
            let Some(found) = text[start..].find(keyword) else {
                break;
            };
            let at = start + found;
            let before_ok = at == 0 || !is_word(bytes[at - 1]);
            let after = at + klen;
            let after_ok = after >= bytes.len() || !is_word(bytes[after]);
            if before_ok && after_ok {
                score += 1;
            }
            start = at + 1;
        }
    }
    score
}

/// Pick representative words from labels and file names.
fn section_keywords(nodes: &[&GraphNode], limit: usize) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "from", "this", "that", "class", "function", "method", "file",
        "src", "lib", "core", "index", "main", "init", "py", "ts", "tsx", "js", "jsx", "go", "rs",
        "java", "html", "css",
    ];
    // First-seen order breaks count ties, matching Counter.most_common.
    let mut counts: HashMap<String, usize> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for node in nodes {
        let text = format!("{} {}", node.label, node.source_file).replace(['/', '_', '-'], " ");
        for raw in text.split_whitespace() {
            let word: String = raw
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(char::to_lowercase)
                .collect();
            if word.chars().count() < 3 || STOPWORDS.contains(&word.as_str()) {
                continue;
            }
            let entry = counts.entry(word.clone()).or_insert(0);
            if *entry == 0 {
                order.push(word);
            }
            *entry += 1;
        }
    }
    let mut ranked: Vec<(usize, usize, &String)> = order
        .iter()
        .enumerate()
        .map(|(idx, word)| (counts[word], idx, word))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    ranked
        .into_iter()
        .take(limit)
        .map(|(_, _, w)| w.clone())
        .collect()
}

/// Capitalize the first character of a word.
fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Choose a readable section name for a community.
fn label_for_community(
    cid: &str,
    labels: &HashMap<usize, String>,
    nodes: &[&GraphNode],
    lang: &str,
) -> String {
    if let Ok(num) = cid.parse::<usize>()
        && let Some(label) = labels.get(&num)
        && !label.is_empty()
    {
        return label.clone();
    }
    let keywords = section_keywords(nodes, 3);
    if !keywords.is_empty() {
        return keywords
            .iter()
            .take(3)
            .map(|w| title_case(w))
            .collect::<Vec<_>>()
            .join(" ");
    }
    if is_zh(lang) {
        format!("社区 {cid}")
    } else {
        format!("Community {cid}")
    }
}

/// Map community id -> its nodes.
fn build_community_index<'a>(nodes: &[&'a GraphNode]) -> BTreeMap<String, Vec<&'a GraphNode>> {
    let mut idx: BTreeMap<String, Vec<&GraphNode>> = BTreeMap::new();
    for node in nodes {
        let cid = node
            .community
            .map_or_else(|| "unknown".to_string(), |c| c.to_string());
        idx.entry(cid).or_default().push(node);
    }
    idx
}

/// Derive architecture-oriented sections from community clustering.
fn derive_sections(
    comm_idx: &BTreeMap<String, Vec<&GraphNode>>,
    labels: &HashMap<usize, String>,
    lang: &str,
    max_sections: usize,
) -> Vec<Section> {
    struct Grouped {
        id: &'static str,
        name: String,
        communities: Vec<String>,
        node_count: usize,
        priority: usize,
    }

    let mut grouped: BTreeMap<&'static str, Grouped> = BTreeMap::new();
    let mut unassigned: Vec<(String, String)> = Vec::new();

    // Largest communities first, ties broken by id — matches the Python sort.
    let mut ordered: Vec<(&String, &Vec<&GraphNode>)> = comm_idx.iter().collect();
    ordered.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(b.0)));

    for (cid, community_nodes) in ordered {
        let label = label_for_community(cid, labels, community_nodes, lang);
        let text = community_text(community_nodes, &label);

        let mut best: Option<(usize, &'static str, &'static str, &'static str)> = None;
        let mut best_score = 0;
        for (priority, (sid, zh_name, en_name, keywords)) in SECTION_ARCHETYPES.iter().enumerate() {
            let score = keyword_score(&text, keywords);
            if score > best_score {
                best = Some((priority, sid, zh_name, en_name));
                best_score = score;
            }
        }

        match best {
            Some((priority, sid, zh_name, en_name)) if best_score >= 2 => {
                let entry = grouped.entry(sid).or_insert_with(|| Grouped {
                    id: sid,
                    name: pick_text(lang, zh_name, en_name).to_string(),
                    communities: Vec::new(),
                    node_count: 0,
                    priority,
                });
                entry.communities.push(cid.clone());
                entry.node_count += community_nodes.len();
            }
            _ => unassigned.push((cid.clone(), label)),
        }
    }

    let max_sections = max_sections.max(1);
    let cap = max_sections.saturating_sub(1).max(1);
    let mut ranked: Vec<Grouped> = grouped.into_values().collect();
    ranked.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then_with(|| b.node_count.cmp(&a.node_count))
            .then_with(|| a.id.cmp(b.id))
    });
    let overflow: Vec<Grouped> = ranked.split_off(ranked.len().min(cap));

    let mut sections: Vec<Section> = vec![Section {
        id: "overview".to_string(),
        name: pick_text(lang, "架构总览", "Architecture Overview").to_string(),
        communities: Vec::new(),
    }];
    for sec in ranked {
        sections.push(Section {
            id: sec.id.to_string(),
            name: sec.name,
            communities: sec.communities,
        });
    }

    let remaining_slots = max_sections
        .saturating_sub(sections.len() - 1)
        .saturating_sub(1);
    let taken = unassigned.len().min(remaining_slots);
    for (cid, label) in &unassigned[..taken] {
        let id = if label.is_empty() {
            format!("community-{cid}")
        } else {
            label.clone()
        };
        sections.push(Section {
            id,
            name: label.clone(),
            communities: vec![cid.clone()],
        });
    }

    let mut other: Vec<String> = overflow.into_iter().flat_map(|s| s.communities).collect();
    other.extend(unassigned[taken..].iter().map(|(cid, _)| cid.clone()));
    if !other.is_empty() {
        sections.push(Section {
            id: "other".to_string(),
            name: pick_text(lang, "其他", "Other").to_string(),
            communities: other,
        });
    }
    sections
}

/// Ensure sections have safe unique anchor IDs, with the overview first.
fn normalize_sections(sections: Vec<Section>, lang: &str) -> Vec<Section> {
    let overview_name = pick_text(lang, "架构总览", "Architecture Overview");
    let mut normalized = vec![Section {
        id: "overview".to_string(),
        name: overview_name.to_string(),
        communities: Vec::new(),
    }];
    let mut used: HashSet<String> = ["overview", "hyperedges", "stats"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();

    for (index, sec) in sections.into_iter().enumerate() {
        if sec.id.eq_ignore_ascii_case("overview") {
            if !sec.name.is_empty() {
                normalized[0].name = sec.name;
            }
            continue;
        }
        let fallback = format!("section-{}", index + 1);
        let sid = html_anchor_id(&sec.id, &fallback, &mut used);
        normalized.push(Section {
            id: sid,
            name: sec.name,
            communities: sec.communities,
        });
    }
    normalized
}

/// Map section id -> the nodes of every community it covers.
fn build_section_node_map<'a>(
    sections: &[Section],
    comm_idx: &BTreeMap<String, Vec<&'a GraphNode>>,
) -> HashMap<String, Vec<&'a GraphNode>> {
    let mut map: HashMap<String, Vec<&GraphNode>> = HashMap::new();
    for sec in sections {
        if sec.id == "overview" {
            map.insert(sec.id.clone(), Vec::new());
            continue;
        }
        let mut nodes: Vec<&GraphNode> = Vec::new();
        for cid in &sec.communities {
            if let Some(community_nodes) = comm_idx.get(cid) {
                nodes.extend(community_nodes.iter().copied());
            }
        }
        map.insert(sec.id.clone(), nodes);
    }
    map
}

// ──────────────────────────────────────────────
// 7. Edge classification by section
// ──────────────────────────────────────────────

/// Edges split into intra-section, inter-section, and dangling groups.
struct ClassifiedEdges<'a> {
    intra: HashMap<String, Vec<&'a GraphEdge>>,
    inter: Vec<&'a GraphEdge>,
    node_section: HashMap<&'a str, String>,
}

fn classify_edges<'a>(
    edges: &[&'a GraphEdge],
    section_nodes_map: &HashMap<String, Vec<&'a GraphNode>>,
) -> ClassifiedEdges<'a> {
    let mut node_section: HashMap<&str, String> = HashMap::new();
    for (sid, nodes) in section_nodes_map {
        for node in nodes {
            node_section.insert(node.id.as_str(), sid.clone());
        }
    }

    let mut intra: HashMap<String, Vec<&GraphEdge>> = HashMap::new();
    let mut inter: Vec<&GraphEdge> = Vec::new();
    for edge in edges {
        let src_sec = node_section.get(edge.source.as_str());
        let tgt_sec = node_section.get(edge.target.as_str());
        match (src_sec, tgt_sec) {
            (Some(s), Some(t)) if s == t => intra.entry(s.clone()).or_default().push(edge),
            (Some(_), Some(_)) => inter.push(edge),
            _ => {}
        }
    }
    ClassifiedEdges {
        intra,
        inter,
        node_section,
    }
}

/// Inter-section edge totals keyed by `(source section, target section)`, each
/// carrying a per-relation tally used to pick the arrow label.
type SectionEdgeSummary = BTreeMap<(String, String), (usize, BTreeMap<String, usize>)>;

/// Aggregate inter-section edge counts and their dominant relation.
fn section_edge_summary(classified: &ClassifiedEdges) -> SectionEdgeSummary {
    let mut summary: SectionEdgeSummary = BTreeMap::new();
    for edge in &classified.inter {
        if !should_include_edge(edge) {
            continue;
        }
        let (Some(src), Some(tgt)) = (
            classified.node_section.get(edge.source.as_str()),
            classified.node_section.get(edge.target.as_str()),
        ) else {
            continue;
        };
        if src == tgt {
            continue;
        }
        let entry = summary
            .entry((src.clone(), tgt.clone()))
            .or_insert_with(|| (0, BTreeMap::new()));
        entry.0 += 1;
        *entry.1.entry(edge.relation.clone()).or_insert(0) += 1;
    }
    summary
}

/// Most frequent relation in a counted map, ties broken alphabetically.
fn dominant_relation(relations: &BTreeMap<String, usize>) -> String {
    relations
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map_or_else(|| "relates".to_string(), |(rel, _)| rel.clone())
}

// ──────────────────────────────────────────────
// 8. Diagram node selection
// ──────────────────────────────────────────────

/// Use graphify centrality fields when the analysis pass recorded them.
fn node_importance(node: &GraphNode) -> f64 {
    for key in [
        "pagerank",
        "page_rank",
        "pageRank",
        "rank",
        "centrality",
        "score",
    ] {
        if let Some(value) = node.extra.get(key) {
            return value.as_f64().unwrap_or(0.0);
        }
    }
    0.0
}

/// Score nodes by useful edge participation.
fn node_degree_scores(edges: &[&GraphEdge]) -> HashMap<String, f64> {
    let mut scores: HashMap<String, f64> = HashMap::new();
    for edge in edges {
        let score = edge_score(edge);
        *scores.entry(edge.source.clone()).or_insert(0.0) += score;
        *scores.entry(edge.target.clone()).or_insert(0.0) += score;
    }
    scores
}

/// Select a compact, connected subset of nodes for a readable diagram.
fn select_diagram_nodes<'a>(
    nodes: &[&'a GraphNode],
    edges: &[&'a GraphEdge],
    max_nodes: usize,
) -> Vec<&'a GraphNode> {
    let node_by_id: HashMap<&str, &GraphNode> = nodes.iter().map(|n| (n.id.as_str(), *n)).collect();

    let mut usable = preferred_edges(edges, false);
    if usable.is_empty() {
        usable = preferred_edges(edges, true);
    }
    let scores = node_degree_scores(&usable);

    let mut outgoing: HashMap<&str, i64> = HashMap::new();
    let mut incoming: HashMap<&str, i64> = HashMap::new();
    for edge in &usable {
        *outgoing.entry(edge.source.as_str()).or_insert(0) += 1;
        *incoming.entry(edge.target.as_str()).or_insert(0) += 1;
    }

    let mut selected: Vec<&GraphNode> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let concept_cap = std::cmp::max(4, max_nodes / 3);

    // Returns true once the diagram is full.
    let add_node =
        |nid: &str, selected: &mut Vec<&'a GraphNode>, seen: &mut HashSet<&'a str>| -> bool {
            let Some(node) = node_by_id.get(nid) else {
                return false;
            };
            if seen.contains(node.id.as_str()) {
                return false;
            }
            if node_kind(node) == "concept" && selected.len() >= concept_cap {
                return false;
            }
            selected.push(node);
            seen.insert(node.id.as_str());
            selected.len() >= max_nodes
        };

    // Start with likely entry points: nodes that call out more than they are called.
    let mut entry_candidates: Vec<&str> = node_by_id.keys().copied().collect();
    entry_candidates.sort_by(|a, b| {
        let out_a = outgoing.get(a).copied().unwrap_or(0);
        let out_b = outgoing.get(b).copied().unwrap_or(0);
        let net_a = out_a - incoming.get(a).copied().unwrap_or(0);
        let net_b = out_b - incoming.get(b).copied().unwrap_or(0);
        net_b
            .cmp(&net_a)
            .then_with(|| out_b.cmp(&out_a))
            .then_with(|| a.cmp(b))
    });
    for nid in entry_candidates
        .iter()
        .take(std::cmp::max(3, max_nodes / 3))
    {
        if outgoing.get(nid).copied().unwrap_or(0) > 0 && add_node(nid, &mut selected, &mut seen) {
            return selected;
        }
    }

    // Then pull in the most useful neighbors from the strongest edges.
    let mut ranked = usable.clone();
    ranked.sort_by(by_edge_score_desc);
    for edge in &ranked {
        for nid in [edge.source.as_str(), edge.target.as_str()] {
            if add_node(nid, &mut selected, &mut seen) {
                return selected;
            }
        }
    }

    // Finally top up with whatever remains, most connected first.
    let mut fallback: Vec<&GraphNode> = nodes.to_vec();
    fallback.sort_by(|a, b| {
        let kind_penalty = |n: &GraphNode| usize::from(node_kind(n) == "concept");
        let score = |n: &GraphNode| scores.get(&n.id).copied().unwrap_or(0.0);
        kind_penalty(a)
            .cmp(&kind_penalty(b))
            .then_with(|| {
                score(b)
                    .partial_cmp(&score(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| {
                node_importance(b)
                    .partial_cmp(&node_importance(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| safe_file_path(&a.source_file).cmp(&safe_file_path(&b.source_file)))
            .then_with(|| {
                humanize_label(&a.label, &a.source_file)
                    .cmp(&humanize_label(&b.label, &b.source_file))
            })
    });
    for node in fallback {
        if !seen.contains(node.id.as_str()) {
            selected.push(node);
            seen.insert(node.id.as_str());
        }
        if selected.len() >= max_nodes {
            break;
        }
    }
    selected
}

/// Build a readable Mermaid node label, with the source file as a subtitle.
fn node_label(node: &GraphNode) -> String {
    let label = humanize_label(&node.label, &node.source_file);
    let source_file = safe_file_path(&node.source_file);
    if !source_file.is_empty() && !label.ends_with(&base_name(&source_file)) {
        format!(
            "{}<br/><small>{}</small>",
            safe_mermaid_text(&label),
            safe_mermaid_text(&source_file)
        )
    } else {
        safe_mermaid_text(&label)
    }
}

/// Group selected nodes by source file for Mermaid subgraphs.
fn group_nodes_by_file<'a>(nodes: &[&'a GraphNode]) -> Vec<(String, Vec<&'a GraphNode>)> {
    let mut groups: BTreeMap<String, Vec<&GraphNode>> = BTreeMap::new();
    for node in nodes {
        let key = {
            let path = safe_file_path(&node.source_file);
            if path.is_empty() {
                "External / generated".to_string()
            } else {
                path
            }
        };
        groups.entry(key).or_default().push(node);
    }
    let mut ordered: Vec<(String, Vec<&GraphNode>)> = groups.into_iter().collect();
    ordered.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    ordered
}

// ──────────────────────────────────────────────
// 9. Mermaid diagram generators
// ──────────────────────────────────────────────

/// Section-level architecture overview.
fn generate_overview_graph(
    sections: &[Section],
    section_nodes_map: &HashMap<String, Vec<&GraphNode>>,
    classified: &ClassifiedEdges,
    lang: &str,
    diagram_scale: f64,
) -> String {
    let mut lines = vec![mermaid_init(diagram_scale, "LR")];
    let section_defs: Vec<&Section> = sections.iter().filter(|s| s.id != "overview").collect();

    for sec in &section_defs {
        let sid = mermaid_section_id(&sec.id);
        let node_count = section_nodes_map.get(&sec.id).map_or(0, Vec::len);
        let label = format!(
            "{}<br/><small>{node_count} {}</small>",
            safe_mermaid_text(&sec.name),
            safe_mermaid_text("nodes")
        );
        lines.push(format!("    {sid}(\"{label}\")"));
        lines.push(format!("    class {sid} module;"));
    }

    let aggregated = section_edge_summary(classified);
    let mut ranked: Vec<_> = aggregated.iter().collect();
    ranked.sort_by_key(|(_, (count, _))| std::cmp::Reverse(*count));
    for ((src, tgt), (count, relations)) in ranked.into_iter().take(12) {
        let src_id = mermaid_section_id(src);
        let tgt_id = mermaid_section_id(tgt);
        let mut label = relation_label(&dominant_relation(relations), lang);
        if *count > 1 {
            label = format!("{label} x{count}");
        }
        lines.push(format!("    {src_id} -->|{label}| {tgt_id}"));
    }

    if aggregated.is_empty() && section_defs.len() > 1 {
        for pair in section_defs.windows(2) {
            lines.push(format!(
                "    {} -.-> {}",
                mermaid_section_id(&pair[0].id),
                mermaid_section_id(&pair[1].id)
            ));
        }
    }

    lines.extend(MERMAID_CLASS_DEFS.iter().map(|s| (*s).to_string()));
    lines.join("\n")
}

/// Compact, human-readable call-flow chart for one section.
fn generate_section_flowchart(
    section_id: &str,
    section_name: &str,
    nodes: &[&GraphNode],
    edges: &[&GraphEdge],
    lang: &str,
    options: &CallflowOptions,
) -> String {
    let (max_nodes, max_edges) = (options.max_diagram_nodes, options.max_diagram_edges);
    let mut lines = vec![mermaid_init(options.diagram_scale, "LR")];
    lines.push(format!(
        "    %% Section: {} ({} nodes, {} edges)",
        safe_mermaid_text(section_name),
        nodes.len(),
        edges.len()
    ));

    if nodes.is_empty() {
        let empty_label = if is_zh(lang) {
            format!("{section_name} - 无节点")
        } else {
            format!("{section_name} - no nodes")
        };
        lines.push(format!(
            "    empty(\"{}\")",
            safe_mermaid_text(&empty_label)
        ));
        lines.extend(MERMAID_CLASS_DEFS.iter().map(|s| (*s).to_string()));
        return lines.join("\n");
    }

    let selected_nodes = select_diagram_nodes(nodes, edges, max_nodes);
    let selected_ids: HashSet<&str> = selected_nodes.iter().map(|n| n.id.as_str()).collect();

    let visible_in = |allow_structure: bool| -> Vec<&GraphEdge> {
        preferred_edges(edges, allow_structure)
            .into_iter()
            .filter(|e| {
                selected_ids.contains(e.source.as_str()) && selected_ids.contains(e.target.as_str())
            })
            .collect()
    };
    let mut visible_edges = visible_in(false);
    if visible_edges.is_empty() {
        visible_edges = visible_in(true);
    }

    let groups = group_nodes_by_file(&selected_nodes);
    let mut class_lines: Vec<String> = Vec::new();
    for (source_file, group) in &groups {
        let use_subgraph = groups.len() > 1 && group.len() > 1;
        let indent = if use_subgraph { "        " } else { "    " };
        if use_subgraph {
            let group_id = node_mermaid_id(&format!("{section_id}_{source_file}"));
            lines.push(format!(
                "    subgraph {group_id}[\"{}\"]",
                safe_mermaid_text(source_file)
            ));
        }
        for node in group {
            let mid = node_mermaid_id(&node.id);
            lines.push(format!("{indent}{mid}(\"{}\")", node_label(node)));
            class_lines.push(format!("    class {mid} {};", node_kind(node)));
        }
        if use_subgraph {
            lines.push("    end".to_string());
        }
    }

    let mut ranked = visible_edges.clone();
    ranked.sort_by(by_edge_score_desc);
    let mut included = 0;
    for edge in &ranked {
        if included >= max_edges {
            break;
        }
        lines.push(format!(
            "    {} -->|{}| {}",
            node_mermaid_id(&edge.source),
            relation_label(&edge.relation, lang),
            node_mermaid_id(&edge.target)
        ));
        included += 1;
    }

    let omitted_nodes = nodes.len().saturating_sub(selected_nodes.len());
    let omitted_edges = visible_edges.len().saturating_sub(included);
    if omitted_nodes > 0 || omitted_edges > 0 {
        lines.push(format!(
            "    %% Omitted for readability: {omitted_nodes} nodes, {omitted_edges} edges"
        ));
    }
    lines.extend(class_lines);
    lines.extend(MERMAID_CLASS_DEFS.iter().map(|s| (*s).to_string()));
    lines.join("\n")
}

// ──────────────────────────────────────────────
// 10. HTML fragment generators
// ──────────────────────────────────────────────

/// Sticky navigation bar linking every section.
fn generate_nav(sections: &[Section]) -> String {
    let links: Vec<String> = sections
        .iter()
        .map(|sec| {
            format!(
                "    <a href=\"#{}\">{}</a>",
                esc_attr(&sec.id),
                esc(&sec.name)
            )
        })
        .collect();
    format!("<div class=\"nav\">\n{}\n</div>", links.join("\n"))
}

/// Readable node label for tables and summaries.
fn node_display_name(node: Option<&GraphNode>, fallback: &str) -> String {
    match node {
        Some(n) => {
            let label = if n.label.is_empty() {
                if n.id.is_empty() { fallback } else { &n.id }
            } else {
                &n.label
            };
            humanize_label(label, &n.source_file)
        }
        None => fallback.to_string(),
    }
}

/// Render node references as readable labels instead of internal IDs.
fn format_node_refs(
    node_ids: &HashSet<&str>,
    node_by_id: &HashMap<&str, &GraphNode>,
    lang: &str,
    empty_text: &str,
    limit: usize,
) -> String {
    if node_ids.is_empty() {
        return esc(empty_text);
    }
    let mut ordered: Vec<&str> = node_ids.iter().copied().collect();
    ordered.sort_by_key(|nid| node_display_name(node_by_id.get(nid).copied(), nid).to_lowercase());

    let mut parts: Vec<String> = Vec::new();
    for nid in ordered.iter().take(limit) {
        let node = node_by_id.get(nid).copied();
        let label = node_display_name(node, nid);
        let source = node.map_or_else(String::new, |n| safe_file_path(&n.source_file));
        if source.is_empty() {
            parts.push(format!("<code>{}</code>", esc(&label)));
        } else {
            parts.push(format!(
                "<code>{}</code><br><small style=\"color:var(--muted)\">{}</small>",
                esc(&label),
                esc(&source)
            ));
        }
    }
    if node_ids.len() > limit {
        let extra = node_ids.len() - limit;
        parts.push(esc(&if is_zh(lang) {
            format!("+{extra} 个更多")
        } else {
            format!("+{extra} more")
        }));
    }
    parts.join("<br>")
}

/// Heuristic tag suggestion based on node kind, label, and file type.
fn suggest_tag(label: &str, file_type: &str, lang: &str, kind: &str) -> String {
    let named = match kind {
        "concept" => Some(("概念", "Concept", "tag-func")),
        "entry" => Some(("入口", "Entry", "tag-cmd")),
        "api" => Some(("API", "API", "tag-endpoint")),
        "async" => Some(("异步", "Async", "tag-async")),
        "klass" => Some(("类", "Class", "tag-class")),
        "ui" => Some(("UI", "UI", "tag-hook")),
        "module" => Some(("模块", "Module", "tag-class")),
        "test" => Some(("测试", "Test", "tag-func")),
        "function" => Some(("函数", "Function", "tag-func")),
        _ => None,
    };
    if let Some((zh, en, class)) = named {
        return format!(
            "<span class=\"tag {class}\">{}</span>",
            pick_text(lang, zh, en)
        );
    }

    let lower = label.to_lowercase();
    if file_type == "rationale" {
        return format!(
            "<span class=\"tag tag-func\">{}</span>",
            pick_text(lang, "概念", "Concept")
        );
    }
    if ["cli", "command", "scan", "serve", "chat", "config"]
        .iter()
        .any(|kw| lower.contains(kw))
        && (lower.contains("group") || lower.contains("command"))
    {
        return format!(
            "<span class=\"tag tag-cmd\">{}</span>",
            pick_text(lang, "CLI命令", "CLI")
        );
    }
    if ["router", "endpoint", "api", "/api/"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        return format!(
            "<span class=\"tag tag-endpoint\">{}</span>",
            pick_text(lang, "API端点", "API")
        );
    }
    if ["async", "await", "stream"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        return format!(
            "<span class=\"tag tag-async\">{}</span>",
            pick_text(lang, "异步", "Async")
        );
    }
    if ["class", "model", "schema", "dataclass", "pydantic"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        return format!(
            "<span class=\"tag tag-class\">{}</span>",
            pick_text(lang, "类", "Class")
        );
    }
    if ["hook", "usestate", "useeffect", "store"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        return "<span class=\"tag tag-hook\">Hook</span>".to_string();
    }
    if ["component", "props", "tsx", "jsx", "render"]
        .iter()
        .any(|kw| lower.contains(kw))
    {
        return format!(
            "<span class=\"tag tag-class\">{}</span>",
            pick_text(lang, "组件", "Component")
        );
    }
    format!(
        "<span class=\"tag tag-func\">{}</span>",
        pick_text(lang, "函数", "Function")
    )
}

/// Compact human-readable description for a graph node.
fn describe_node(label: &str, source_file: &str, file_type: &str, lang: &str) -> String {
    let lower = label.to_lowercase();
    let source = if source_file.is_empty() {
        pick_text(lang, "项目", "project").to_string()
    } else {
        source_file.to_string()
    };

    if file_type == "rationale" {
        return if is_zh(lang) {
            format!("设计说明：{label}")
        } else {
            format!("Design note for {label}.")
        };
    }
    if file_type == "document" {
        return if is_zh(lang) {
            format!("文档入口，描述 {label} 相关能力。")
        } else {
            format!("Documentation node describing {label}.")
        };
    }
    if [".py", ".tsx", ".ts"]
        .iter()
        .any(|ext| label.ends_with(ext))
    {
        return if is_zh(lang) {
            format!("{source} 中的模块文件，承载该层主要实现。")
        } else {
            format!("Module file in {source}.")
        };
    }

    const RULES: &[(&str, &str, &str)] = &[
        (
            "config",
            "读取、解析或持久化项目配置。",
            "Reads, resolves, or persists project configuration.",
        ),
        (
            "scan",
            "触发项目扫描或处理扫描状态。",
            "Starts scanning or handles scan status.",
        ),
        (
            "ingest",
            "把本地目录或远程仓库转换为分析上下文。",
            "Turns a local path or remote repository into analysis context.",
        ),
        (
            "clone",
            "把本地目录或远程仓库转换为分析上下文。",
            "Turns a local path or remote repository into analysis context.",
        ),
        (
            "git",
            "把本地目录或远程仓库转换为分析上下文。",
            "Turns a local path or remote repository into analysis context.",
        ),
        (
            "prompt",
            "构造发送给 LLM 的结构化提示。",
            "Builds structured prompts for model calls.",
        ),
        (
            "analy",
            "编排分析流程并产出结构化文档数据。",
            "Orchestrates analysis and returns structured documentation data.",
        ),
        (
            "graph",
            "构建依赖关系并提供排序或图形化数据。",
            "Builds dependency relationships and graph data.",
        ),
        (
            "dependency",
            "构建依赖关系并提供排序或图形化数据。",
            "Builds dependency relationships and graph data.",
        ),
        (
            "export",
            "将文档数据导出为目标格式。",
            "Exports documentation data to a target format.",
        ),
        (
            "markdown",
            "将文档数据导出为目标格式。",
            "Exports documentation data to a target format.",
        ),
        (
            "html",
            "将文档数据导出为目标格式。",
            "Exports documentation data to a target format.",
        ),
        (
            "chat",
            "支撑检索增强问答或流式聊天。",
            "Supports retrieval-augmented Q&A or streaming chat.",
        ),
        (
            "rag",
            "支撑检索增强问答或流式聊天。",
            "Supports retrieval-augmented Q&A or streaming chat.",
        ),
        (
            "retrieve",
            "支撑检索增强问答或流式聊天。",
            "Supports retrieval-augmented Q&A or streaming chat.",
        ),
        (
            "wiki",
            "组织文档页面、侧边栏或内容读取。",
            "Organizes documentation pages, navigation, or content lookup.",
        ),
        (
            "page",
            "组织文档页面、侧边栏或内容读取。",
            "Organizes documentation pages, navigation, or content lookup.",
        ),
        (
            "sidebar",
            "组织文档页面、侧边栏或内容读取。",
            "Organizes documentation pages, navigation, or content lookup.",
        ),
        (
            "cache",
            "缓存分析结果或生成缓存键。",
            "Caches analysis results or computes cache keys.",
        ),
        (
            "hash",
            "缓存分析结果或生成缓存键。",
            "Caches analysis results or computes cache keys.",
        ),
        (
            "test",
            "验证导入、入口点或版本等基础行为。",
            "Verifies imports, entry points, or version behavior.",
        ),
    ];
    for (needle, zh, en) in RULES {
        if lower.contains(needle) {
            return pick_text(lang, zh, en).to_string();
        }
    }
    if is_zh(lang) {
        format!("{source} 中的 {label} 节点。")
    } else {
        format!("{label} node in {source}.")
    }
}

/// Call-detail table rows for a section, capped at 30 nodes.
fn generate_call_table_rows(
    nodes: &[&GraphNode],
    section_edges: &[&GraphEdge],
    lang: &str,
) -> String {
    if nodes.is_empty() {
        return String::new();
    }
    let node_by_id: HashMap<&str, &GraphNode> = nodes.iter().map(|n| (n.id.as_str(), *n)).collect();

    const CALL_RELATIONS: &[&str] = &["calls", "imports", "imports_from", "uses", "method"];
    let mut callers: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut callees: HashMap<&str, HashSet<&str>> = HashMap::new();
    for edge in section_edges {
        if CALL_RELATIONS.contains(&edge.relation.as_str()) {
            callers
                .entry(edge.target.as_str())
                .or_default()
                .insert(edge.source.as_str());
            callees
                .entry(edge.source.as_str())
                .or_default()
                .insert(edge.target.as_str());
        }
    }

    let empty = HashSet::new();
    let mut rows = String::new();
    for (i, node) in nodes.iter().take(30).enumerate() {
        let source_file = safe_file_path(&node.source_file);
        let file_type = file_type_of(node);
        let tag = suggest_tag(&node.label, file_type, lang, node_kind(node));
        let caller_text = format_node_refs(
            callers.get(node.id.as_str()).unwrap_or(&empty),
            &node_by_id,
            lang,
            pick_text(
                lang,
                "外部入口 / 无直接入边",
                "External entry / no inbound edge",
            ),
            3,
        );
        let callee_text = format_node_refs(
            callees.get(node.id.as_str()).unwrap_or(&empty),
            &node_by_id,
            lang,
            pick_text(lang, "无直接出边", "No direct outbound edge"),
            3,
        );
        let _ = write!(
            rows,
            "<tr>
  <td>{}</td>
  <td><code>{}</code><br><small style=\"color:var(--muted)\">{}</small></td>
  <td>{tag}</td>
  <td>{caller_text}</td>
  <td>{callee_text}</td>
  <td>{}</td>
</tr>\n",
            i + 1,
            esc(&node.label),
            esc(&source_file),
            esc(&describe_node(&node.label, &source_file, file_type, lang))
        );
    }
    rows.trim_end().to_string()
}

/// Derive a readable section-to-section flow from inter-section edges.
fn derive_flow_chain(sections: &[Section], classified: &ClassifiedEdges) -> String {
    let order: Vec<&str> = sections
        .iter()
        .filter(|s| s.id != "overview")
        .map(|s| s.id.as_str())
        .collect();
    if order.is_empty() {
        return "Graph nodes -> documentation".to_string();
    }
    let names: HashMap<&str, &str> = sections
        .iter()
        .map(|s| (s.id.as_str(), s.name.as_str()))
        .collect();

    let mut outgoing: HashMap<String, BTreeMap<String, usize>> = HashMap::new();
    let mut incoming: HashMap<String, usize> = HashMap::new();
    for ((src, tgt), (count, _)) in section_edge_summary(classified) {
        *outgoing
            .entry(src)
            .or_default()
            .entry(tgt.clone())
            .or_insert(0) += count;
        *incoming.entry(tgt).or_insert(0) += count;
    }

    let start = order
        .iter()
        .enumerate()
        .min_by_key(|(idx, sid)| (incoming.get(**sid).copied().unwrap_or(0), *idx))
        .map_or(order[0], |(_, sid)| *sid);

    let mut chain: Vec<&str> = vec![start];
    let mut seen: HashSet<&str> = [start].into_iter().collect();
    let mut current = start;
    let limit = std::cmp::min(7, order.len());
    while chain.len() < limit {
        // Highest edge count wins; ties go to the lexicographically largest id,
        // matching Python's `max` over (count, target) tuples.
        let next_owned = outgoing.get(current).and_then(|targets| {
            targets
                .iter()
                .filter(|(tgt, _)| !seen.contains(tgt.as_str()))
                .max_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)))
                .map(|(tgt, _)| tgt.clone())
        });
        let next = match next_owned.and_then(|t| order.iter().find(|s| **s == t).copied()) {
            Some(n) => n,
            None => match order.iter().find(|sid| !seen.contains(*sid)) {
                Some(n) => *n,
                None => break,
            },
        };
        chain.push(next);
        seen.insert(next);
        current = next;
    }
    chain
        .iter()
        .map(|sid| names.get(sid).copied().unwrap_or(sid))
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// Architecture-layer and core-flow cards for the overview section.
fn generate_overview_cards(
    sections: &[Section],
    section_nodes_map: &HashMap<String, Vec<&GraphNode>>,
    classified: &ClassifiedEdges,
    lang: &str,
) -> String {
    let mut rows = String::new();
    for sec in sections.iter().filter(|s| s.id != "overview") {
        let communities = sec.communities.join(", ");
        let node_count = section_nodes_map.get(&sec.id).map_or(0, Vec::len);
        let _ = write!(
            rows,
            "<tr><td>{}</td><td>{node_count}</td><td><code>{}</code></td></tr>",
            esc(&sec.name),
            esc(&communities)
        );
    }
    let flow = derive_flow_chain(sections, classified);
    let layer_title = pick_text(lang, "架构层次", "Architecture Layers");
    let layer_cols = pick_text(
        lang,
        "<tr><th>层</th><th>节点</th><th>社区</th></tr>",
        "<tr><th>Layer</th><th>Nodes</th><th>Communities</th></tr>",
    );
    let flow_title = pick_text(lang, "核心数据流", "Core Flow");
    format!(
        r#"<div class="grid">
  <div class="card">
    <h4>{layer_title}</h4>
    <table style="width:100%;font-size:0.85rem;">
      {layer_cols}
      {rows}
    </table>
  </div>
  <div class="card">
    <h4>{flow_title}</h4>
    <div class="arrow-chain">{}</div>
  </div>
</div>"#,
        esc(&flow)
    )
}

/// Introductory paragraph for a section.
fn generate_section_intro(
    sec: &Section,
    nodes: &[&GraphNode],
    edge_count: usize,
    lang: &str,
) -> String {
    // First-seen order breaks count ties, matching Counter.most_common.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for node in nodes {
        if node.source_file.is_empty() {
            continue;
        }
        let entry = counts.entry(node.source_file.as_str()).or_insert(0);
        if *entry == 0 {
            order.push(node.source_file.as_str());
        }
        *entry += 1;
    }
    let mut ranked: Vec<(usize, usize, &str)> = order
        .iter()
        .enumerate()
        .map(|(idx, path)| (counts[path], idx, *path))
        .collect();
    ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let files: Vec<String> = ranked
        .iter()
        .take(3)
        .map(|(_, _, path)| safe_file_path(path))
        .collect();
    let keywords = section_keywords(nodes, 4);

    let text = if is_zh(lang) {
        let file_text = if files.is_empty() {
            "未标注源文件".to_string()
        } else {
            files.join("、")
        };
        let keyword_text = if keywords.is_empty() {
            sec.name.clone()
        } else {
            keywords.join("、")
        };
        format!(
            "{} 汇集了与 {keyword_text} 相关的实现，主要分布在 {file_text}。\
             本节覆盖 {} 个节点、{edge_count} 条内部边，图中只展示最有代表性的调用关系以保持可读性。",
            sec.name,
            nodes.len()
        )
    } else {
        let file_text = if files.is_empty() {
            "unmapped files".to_string()
        } else {
            files.join(", ")
        };
        let keyword_text = if keywords.is_empty() {
            sec.name.clone()
        } else {
            keywords.join(", ")
        };
        format!(
            "{} groups implementation around {keyword_text}, mostly in {file_text}. \
             This section covers {} nodes and {edge_count} internal edges; \
             the diagram shows only representative relationships to stay readable.",
            sec.name,
            nodes.len()
        )
    };
    format!("<p>{}</p>", esc(&text))
}

/// Key-file and design-note cards for a section.
fn generate_section_cards(
    nodes: &[&GraphNode],
    section_edges: &[&GraphEdge],
    lang: &str,
) -> String {
    let mut file_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for node in nodes {
        if !node.source_file.is_empty() {
            *file_counts.entry(node.source_file.as_str()).or_insert(0) += 1;
        }
    }
    let mut top_files: Vec<(&str, usize)> = file_counts.into_iter().collect();
    top_files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let file_rows = if top_files.is_empty() {
        format!(
            "<tr><td colspan=\"2\">{}</td></tr>",
            esc(pick_text(lang, "无源文件映射", "No source file mapping"))
        )
    } else {
        top_files
            .iter()
            .take(8)
            .map(|(path, count)| {
                format!(
                    "<tr><td><code>{}</code></td><td>{count} {}</td></tr>",
                    esc(&safe_file_path(path)),
                    esc(pick_text(lang, "个节点", "nodes"))
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut relation_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for edge in section_edges.iter().filter(|e| should_include_edge(e)) {
        *relation_counts.entry(edge.relation.as_str()).or_insert(0) += 1;
    }
    let mut ranked: Vec<(&str, usize)> = relation_counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let relation_text = if ranked.is_empty() {
        pick_text(
            lang,
            "未检测到高置信调用边",
            "No high-confidence call edges detected",
        )
        .to_string()
    } else {
        ranked
            .iter()
            .take(4)
            .map(|(rel, count)| format!("{} x{count}", relation_label(rel, lang)))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let note = if is_zh(lang) {
        format!(
            "本节由 graphify 社区聚类生成。关系概况：{relation_text}。\
             图表优先展示高置信、跨节点调用或使用关系，完整节点清单位于表格中。"
        )
    } else {
        format!(
            "This section comes from graphify community clustering. \
             Relationship summary: {relation_text}. The diagram prioritizes high-confidence \
             calls or usage relationships; the table keeps the broader node inventory."
        )
    };

    format!(
        r#"<div class="grid">
  <div class="card">
    <h4>{}</h4>
    <table style="width:100%;font-size:0.85rem;">
      <tr><th>File</th><th>{}</th></tr>
      {file_rows}
    </table>
  </div>
  <div class="card">
    <h4>{}</h4>
    <p>{}</p>
  </div>
</div>"#,
        pick_text(lang, "关键文件", "Key Files"),
        pick_text(lang, "覆盖节点", "Coverage"),
        pick_text(lang, "设计备注", "Design Notes"),
        esc(&note)
    )
}

/// Compact highlights card pulled from `GRAPH_REPORT.md`.
fn report_highlights(report_text: &str, lang: &str) -> String {
    if report_text.trim().is_empty() {
        return String::new();
    }
    let mut keep: Vec<&str> = Vec::new();
    let mut in_gods = false;
    let mut in_summary = false;
    for line in report_text.lines() {
        let stripped = line.trim();
        if stripped.starts_with("## ") {
            in_summary = stripped == "## Summary";
            in_gods = stripped.starts_with("## God Nodes");
            continue;
        }
        if in_summary && let Some(rest) = stripped.strip_prefix("- ") {
            keep.push(rest);
        } else if in_gods
            && stripped.split_once('.').is_some_and(|(head, _)| {
                !head.is_empty() && head.bytes().all(|b| b.is_ascii_digit())
            })
        {
            keep.push(stripped);
        }
        if keep.len() >= 6 {
            break;
        }
    }
    if keep.is_empty() {
        return String::new();
    }
    let title = pick_text(lang, "图谱报告摘要", "Graph Report Highlights");
    let items = keep
        .iter()
        .map(|item| format!("      <li>{}</li>", esc(item)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"<div class="card">
    <h4>{title}</h4>
    <ul>
{items}
    </ul>
  </div>"#
    )
}

/// Current UTC time formatted as `YYYY-MM-DD HH:MM UTC`.
fn utc_timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let days = (secs / 86_400) as i64;
    let time_of_day = secs % 86_400;

    // Civil-from-days (Howard Hinnant's algorithm), epoch shifted to 0000-03-01.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!(
        "{year:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        time_of_day / 3600,
        (time_of_day % 3600) / 60
    )
}

/// Mermaid bootstrap plus the zoom/pan toolbar attached to every diagram.
const INTERACTIVE_JS: &str = r#"<script>
(function () {
  const mermaidConfig = {
    startOnLoad: false,
    theme: 'dark',
    securityLevel: 'loose',
    flowchart: { htmlLabels: true, useMaxWidth: true },
    themeVariables: {
      primaryColor: '#1e293b',
      primaryTextColor: '#e2e8f0',
      primaryBorderColor: '#38bdf8',
      secondaryColor: '#0f172a',
      tertiaryColor: '#334155',
      lineColor: '#64748b',
      textColor: '#e2e8f0',
    }
  };

  mermaid.initialize(mermaidConfig);

  function clamp(value, min, max) {
    return Math.min(max, Math.max(min, value));
  }

  function enhanceMermaidDiagrams() {
    document.querySelectorAll('.mermaid').forEach((container) => {
      if (container.dataset.zoomReady === 'true') return;
      const svg = container.querySelector('svg');
      if (!svg) return;

      container.dataset.zoomReady = 'true';
      container.classList.add('is-enhanced');

      const viewport = document.createElement('div');
      viewport.className = 'mermaid-viewport';
      svg.parentNode.insertBefore(viewport, svg);
      viewport.appendChild(svg);

      const toolbar = document.createElement('div');
      toolbar.className = 'mermaid-toolbar';
      toolbar.innerHTML = [
        '<button type="button" data-action="zoom-out" title="Zoom out">-</button>',
        '<span class="zoom-level" data-role="level">100%</span>',
        '<button type="button" data-action="zoom-in" title="Zoom in">+</button>',
        '<button type="button" data-action="fit" title="Fit width">Fit</button>',
        '<button type="button" data-action="reset" title="Reset view">Reset</button>'
      ].join('');
      container.insertBefore(toolbar, viewport);

      const state = { scale: 1, x: 0, y: 0, dragging: false, startX: 0, startY: 0, originX: 0, originY: 0 };
      const level = toolbar.querySelector('[data-role="level"]');

      function applyTransform() {
        svg.style.transform = `translate(${state.x}px, ${state.y}px) scale(${state.scale})`;
        level.textContent = `${Math.round(state.scale * 100)}%`;
      }

      function zoomBy(delta) {
        state.scale = clamp(state.scale + delta, 0.25, 3);
        applyTransform();
      }

      function reset() {
        state.scale = 1;
        state.x = 0;
        state.y = 0;
        applyTransform();
      }

      function fitWidth() {
        const rawWidth = svg.viewBox && svg.viewBox.baseVal && svg.viewBox.baseVal.width
          ? svg.viewBox.baseVal.width
          : svg.getBoundingClientRect().width / state.scale;
        if (!rawWidth) {
          reset();
          return;
        }
        state.scale = clamp((viewport.clientWidth - 48) / rawWidth, 0.25, 1.4);
        state.x = 0;
        state.y = 0;
        applyTransform();
      }

      toolbar.addEventListener('click', (event) => {
        const button = event.target.closest('button[data-action]');
        if (!button) return;
        const action = button.dataset.action;
        if (action === 'zoom-in') zoomBy(0.15);
        if (action === 'zoom-out') zoomBy(-0.15);
        if (action === 'fit') fitWidth();
        if (action === 'reset') reset();
      });

      viewport.addEventListener('wheel', (event) => {
        if (!event.ctrlKey && !event.metaKey) return;
        event.preventDefault();
        zoomBy(event.deltaY < 0 ? 0.1 : -0.1);
      }, { passive: false });

      viewport.addEventListener('pointerdown', (event) => {
        if (event.button !== 0) return;
        state.dragging = true;
        state.startX = event.clientX;
        state.startY = event.clientY;
        state.originX = state.x;
        state.originY = state.y;
        viewport.classList.add('is-dragging');
        viewport.setPointerCapture(event.pointerId);
      });

      viewport.addEventListener('pointermove', (event) => {
        if (!state.dragging) return;
        state.x = state.originX + event.clientX - state.startX;
        state.y = state.originY + event.clientY - state.startY;
        applyTransform();
      });

      function endDrag(event) {
        if (!state.dragging) return;
        state.dragging = false;
        viewport.classList.remove('is-dragging');
        if (viewport.hasPointerCapture(event.pointerId)) {
          viewport.releasePointerCapture(event.pointerId);
        }
      }

      viewport.addEventListener('pointerup', endDrag);
      viewport.addEventListener('pointercancel', endDrag);
      applyTransform();
    });
  }

  function renderMermaid() {
    const result = mermaid.run
      ? mermaid.run({ querySelector: '.mermaid' })
      : Promise.resolve();
    Promise.resolve(result)
      .then(enhanceMermaidDiagrams)
      .catch((error) => {
        console.error('Mermaid render failed:', error);
        enhanceMermaidDiagrams();
      });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', renderMermaid);
  } else {
    renderMermaid();
  }
})();
</script>"#;

// ──────────────────────────────────────────────
// 11. Entry point
// ──────────────────────────────────────────────

/// Export the graph as a call-flow architecture document (`callflow.html`).
///
/// `community_labels` names the clusters that become sections. A
/// `GRAPH_REPORT.md` already present in `output_dir` contributes a highlights
/// card; it is optional.
pub fn export_callflow_html(
    graph: &KnowledgeGraph,
    community_labels: &HashMap<usize, String>,
    output_dir: &Path,
    options: &CallflowOptions,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(output_dir)?;
    let path = output_dir.join("callflow.html");

    let nodes = graph.nodes();
    let edges = graph.edges();
    let lang = detect_lang(&options.lang, &nodes, community_labels);

    let project_name = if options.project_name.is_empty() {
        "Project".to_string()
    } else {
        options.project_name.clone()
    };

    let comm_idx = build_community_index(&nodes);
    let sections = normalize_sections(
        derive_sections(&comm_idx, community_labels, &lang, options.max_sections),
        &lang,
    );
    let section_nodes_map = build_section_node_map(&sections, &comm_idx);
    let classified = classify_edges(&edges, &section_nodes_map);
    let report_text = fs::read_to_string(output_dir.join("GRAPH_REPORT.md")).unwrap_or_default();

    let doc_title = if is_zh(&lang) {
        format!("{project_name} — 完整调用流程与架构文档")
    } else {
        format!("{project_name} — Complete Call Flow & Architecture Documentation")
    };
    let subtitle = if is_zh(&lang) {
        format!(
            "由 graphify 知识图谱生成：{} 个节点、{} 条边、{} 个社区。",
            nodes.len(),
            edges.len(),
            comm_idx.len()
        )
    } else {
        format!(
            "Generated from graphify knowledge graph: {} nodes, {} edges, {} communities.",
            nodes.len(),
            edges.len(),
            comm_idx.len()
        )
    };

    let mut html = String::with_capacity(64 * 1024);
    let _ = write!(
        html,
        r#"<!DOCTYPE html>
<html lang="{}">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{}</title>
<script src="https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.min.js"></script>
<style>
{CSS}
</style>
</head>
<body>
<div class="container">

<h1>{}</h1>
<p class="subtitle">{}</p>

{}
"#,
        esc_attr(&lang),
        esc(&doc_title),
        esc(&doc_title),
        esc(&subtitle),
        generate_nav(&sections)
    );

    // ── Architecture overview ──
    let overview_name = sections
        .first()
        .map_or("Architecture Overview", |s| s.name.as_str());
    let _ = write!(
        html,
        r#"<!-- ====== Architecture Overview ====== -->
<h2 id="overview">1. {}</h2>

<div class="mermaid">
{}
</div>
{}
"#,
        esc(overview_name),
        generate_overview_graph(
            &sections,
            &section_nodes_map,
            &classified,
            &lang,
            options.diagram_scale
        ),
        generate_overview_cards(&sections, &section_nodes_map, &classified, &lang)
    );
    let report_card = report_highlights(&report_text, &lang);
    if !report_card.is_empty() {
        let _ = write!(html, "<div class=\"grid\">\n  {report_card}\n</div>\n");
    }
    html.push_str("<hr>\n");

    // ── Per-section content ──
    let empty_edges: Vec<&GraphEdge> = Vec::new();
    let empty_nodes: Vec<&GraphNode> = Vec::new();
    let mut section_num = 1;
    for sec in sections.iter().filter(|s| s.id != "overview") {
        section_num += 1;
        let sec_nodes = section_nodes_map.get(&sec.id).unwrap_or(&empty_nodes);
        let sec_edges = classified.intra.get(&sec.id).unwrap_or(&empty_edges);

        let _ = write!(
            html,
            r#"<!-- ====== {}. {} ====== -->
<h2 id="{}">{section_num}. {}</h2>
{}

<div class="mermaid">
{}
</div>

<h3>{}</h3>
<table class="call-table">
<tr>
  <th style="width:5%">#</th>
  <th style="width:28%">{}</th>
  <th style="width:10%">{}</th>
  <th style="width:17%">{}</th>
  <th style="width:20%">{}</th>
  <th style="width:20%">{}</th>
</tr>
{}
</table>

{}
<hr>
"#,
            section_num,
            html_comment_text(&sec.name),
            esc_attr(&sec.id),
            esc(&sec.name),
            generate_section_intro(sec, sec_nodes, sec_edges.len(), &lang),
            generate_section_flowchart(&sec.id, &sec.name, sec_nodes, sec_edges, &lang, options),
            pick_text(&lang, "调用明细", "Call Details"),
            pick_text(&lang, "节点", "Node"),
            pick_text(&lang, "类型", "Type"),
            pick_text(&lang, "调用方", "Caller"),
            pick_text(&lang, "被调用/依赖", "Callees"),
            pick_text(&lang, "说明", "Description"),
            generate_call_table_rows(sec_nodes, sec_edges, &lang),
            generate_section_cards(sec_nodes, sec_edges, &lang)
        );
    }

    // ── Hyperedges ──
    if !graph.hyperedges.is_empty() {
        html.push_str(
            "<h2 id=\"hyperedges\">Group Relationships (Hyperedges)</h2>\n<div class=\"grid\">\n",
        );
        for he in graph.hyperedges.iter().take(9) {
            let _ = write!(
                html,
                "  <div class=\"card\">\n    <h4>{}</h4>\n    <p><code>{}</code> — {} participants</p>\n    <ul>",
                esc(&he.label),
                esc(&he.relation),
                he.nodes.len()
            );
            for hn in he.nodes.iter().take(5) {
                let _ = write!(html, "\n      <li><code>{}</code></li>", esc(hn));
            }
            if he.nodes.len() > 5 {
                let _ = write!(html, "\n      <li>... and {} more</li>", he.nodes.len() - 5);
            }
            html.push_str("\n    </ul>\n  </div>\n");
        }
        html.push_str("</div>\n<hr>\n");
    }

    // ── Statistics ──
    let count_conf = |c: Confidence| edges.iter().filter(|e| e.confidence == c).count();
    let _ = write!(
        html,
        r#"<h2 id="stats">Project Statistics</h2>

<div class="grid">
  <div class="card">
    <h4>Graph</h4>
    <table style="width:100%;font-size:0.85rem;">
      <tr><td>Nodes</td><td>{}</td></tr>
      <tr><td>Edges</td><td>{}</td></tr>
      <tr><td>Hyperedges</td><td>{}</td></tr>
      <tr><td>Communities</td><td>{}</td></tr>
      <tr><td>Documented Sections</td><td>{}</td></tr>
    </table>
  </div>
  <div class="card">
    <h4>Edge Confidence</h4>
    <table style="width:100%;font-size:0.85rem;">
      <tr><td>EXTRACTED</td><td>{}</td></tr>
      <tr><td>INFERRED</td><td>{}</td></tr>
      <tr><td>AMBIGUOUS</td><td>{}</td></tr>
    </table>
  </div>
</div>

<div style="text-align:center; padding:40px 0; color: var(--muted); font-size:0.9rem;">
  <p>{} — Architecture Documentation</p>
  <p>Generated: {} · graphify-rs callflow-html</p>
</div>

</div><!-- .container -->

{INTERACTIVE_JS}

</body>
</html>
"#,
        nodes.len(),
        edges.len(),
        graph.hyperedges.len(),
        comm_idx.len(),
        sections.len() - 1,
        count_conf(Confidence::Extracted),
        count_conf(Confidence::Inferred),
        count_conf(Confidence::Ambiguous),
        esc(&project_name),
        utc_timestamp()
    );

    fs::write(&path, &html)?;
    info!(
        path = %path.display(),
        sections = sections.len() - 1,
        "exported call-flow architecture HTML"
    );
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::model::{Hyperedge, NodeType};

    fn node(id: &str, label: &str, file: &str, kind: NodeType, community: usize) -> GraphNode {
        GraphNode {
            id: id.into(),
            label: label.into(),
            source_file: file.into(),
            source_location: None,
            node_type: kind,
            community: Some(community),
            extra: HashMap::new(),
        }
    }

    fn edge(src: &str, tgt: &str, relation: &str) -> GraphEdge {
        GraphEdge {
            source: src.into(),
            target: tgt.into(),
            relation: relation.into(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: "src/main.rs".into(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: HashMap::new(),
        }
    }

    /// Two communities whose labels steer them into different archetypes.
    fn sample_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        for n in [
            node(
                "extract::parse_ast",
                "parse_ast",
                "src/extract/parser.rs",
                NodeType::Function,
                0,
            ),
            node(
                "extract::extract_calls",
                "extract_calls",
                "src/extract/calls.rs",
                NodeType::Function,
                0,
            ),
            node(
                "export::export_html",
                "export_html",
                "src/export/html.rs",
                NodeType::Function,
                1,
            ),
            node(
                "export::export_wiki",
                "export_wiki",
                "src/export/wiki.rs",
                NodeType::Function,
                1,
            ),
        ] {
            kg.add_node(n).unwrap();
        }
        for e in [
            edge("extract::parse_ast", "extract::extract_calls", "calls"),
            edge("export::export_html", "export::export_wiki", "calls"),
            edge("extract::extract_calls", "export::export_html", "calls"),
        ] {
            kg.add_edge(e).unwrap();
        }
        kg
    }

    fn labels() -> HashMap<usize, String> {
        HashMap::from([
            (0, "Extraction Parser AST".to_string()),
            (1, "Export Html Wiki Report".to_string()),
        ])
    }

    #[test]
    fn writes_full_architecture_document() {
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_graph();
        let opts = CallflowOptions {
            project_name: "demo".to_string(),
            ..Default::default()
        };

        let path = export_callflow_html(&kg, &labels(), dir.path(), &opts).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();

        // The title is HTML-escaped, so `&` arrives as `&amp;`.
        assert!(content.contains("Complete Call Flow &amp; Architecture Documentation"));
        assert!(content.contains(r#"<div class="nav">"#));
        assert!(content.contains(r#"<h2 id="overview">1. Architecture Overview</h2>"#));
        assert!(content.contains(r#"<table class="call-table">"#));
        assert!(content.contains(r#"<h2 id="stats">Project Statistics</h2>"#));
        assert!(content.contains("classDef function"));
        assert!(content.contains("mermaid-toolbar"));
        // Overview plus at least one derived section.
        assert!(content.matches(r#"<div class="mermaid">"#).count() >= 2);
    }

    #[test]
    fn derives_sections_from_archetypes() {
        let kg = sample_graph();
        let nodes = kg.nodes();
        let comm_idx = build_community_index(&nodes);
        let sections = derive_sections(&comm_idx, &labels(), "en", 15);
        let ids: Vec<&str> = sections.iter().map(|s| s.id.as_str()).collect();

        assert_eq!(ids[0], "overview");
        assert!(ids.contains(&"extract-pipeline"), "got {ids:?}");
        assert!(ids.contains(&"outputs-docs"), "got {ids:?}");
    }

    #[test]
    fn empty_graph_still_writes_a_document() {
        let dir = tempfile::tempdir().unwrap();
        let kg = KnowledgeGraph::new();

        let path = export_callflow_html(
            &kg,
            &HashMap::new(),
            dir.path(),
            &CallflowOptions::default(),
        )
        .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();

        assert!(content.contains("Project Statistics"));
        assert!(content.contains("<tr><td>Nodes</td><td>0</td></tr>"));
    }

    #[test]
    fn report_highlights_pulls_summary_and_god_nodes() {
        let report = "# Report\n\n## Summary\n- 100 nodes\n- 42 edges\n\n## God Nodes\n1. main()\n";
        let card = report_highlights(report, "en");
        assert!(card.contains("100 nodes"));
        assert!(card.contains("42 edges"));
        assert!(card.contains("main()"));
    }

    #[test]
    fn hyperedges_render_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let mut kg = sample_graph();
        kg.set_hyperedges(vec![Hyperedge {
            nodes: vec!["extract::parse_ast".into(), "export::export_html".into()],
            relation: "participate_in".into(),
            label: "Build pipeline".into(),
        }]);

        let path =
            export_callflow_html(&kg, &labels(), dir.path(), &CallflowOptions::default()).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();

        assert!(content.contains("Group Relationships (Hyperedges)"));
        assert!(content.contains("Build pipeline"));
        assert!(content.contains("2 participants"));
    }

    #[test]
    fn keyword_score_respects_word_boundaries() {
        assert_eq!(keyword_score("path traversal", &["path"]), 1);
        assert_eq!(keyword_score("pathlib import", &["path"]), 0);
        assert_eq!(keyword_score("src/path/path.rs", &["path"]), 2);
    }

    #[test]
    fn mermaid_ids_are_syntax_safe_and_stable() {
        let id = node_mermaid_id("crates/graphify-export/src/lib.rs::export_html");
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        assert!(!id.starts_with(|c: char| c.is_ascii_digit()));
        assert_eq!(
            id,
            node_mermaid_id("crates/graphify-export/src/lib.rs::export_html")
        );
        assert_ne!(id, node_mermaid_id("other::export_html"));
    }

    #[test]
    fn safe_mermaid_text_strips_syntax_characters() {
        assert_eq!(safe_mermaid_text("a\"b"), "a'b");
        assert_eq!(safe_mermaid_text("x --> y"), "x to y");
        assert_eq!(safe_mermaid_text("f{x}|z"), "fx z");
        assert_eq!(safe_mermaid_text("a<b>c"), "a&lt;b&gt;c");
    }

    #[test]
    fn detects_chinese_from_labels() {
        let kg = sample_graph();
        let nodes = kg.nodes();
        let zh_labels = HashMap::from([(0, "提取管线".to_string())]);
        assert_eq!(detect_lang("auto", &nodes, &zh_labels), "zh-CN");
        assert_eq!(detect_lang("auto", &nodes, &labels()), "en");
        assert_eq!(detect_lang("en", &nodes, &zh_labels), "en");
    }

    #[test]
    fn chinese_document_uses_localized_copy() {
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_graph();
        let opts = CallflowOptions {
            lang: "zh-CN".to_string(),
            project_name: "demo".to_string(),
            ..Default::default()
        };

        let path = export_callflow_html(&kg, &labels(), dir.path(), &opts).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();

        assert!(content.contains("完整调用流程与架构文档"));
        assert!(content.contains("架构总览"));
        assert!(content.contains("调用明细"));
    }

    #[test]
    fn low_confidence_edges_are_excluded() {
        let mut weak = edge("a", "b", "calls");
        weak.confidence = Confidence::Inferred;
        weak.confidence_score = 0.5;
        assert!(!should_include_edge(&weak));

        weak.confidence_score = 0.9;
        assert!(should_include_edge(&weak));

        let mut ambiguous = edge("a", "b", "calls");
        ambiguous.confidence = Confidence::Ambiguous;
        assert!(!should_include_edge(&ambiguous));
    }

    #[test]
    fn node_kind_covers_every_tag_class() {
        let kind_of =
            |label: &str, file: &str, ty: NodeType| node_kind(&node("id", label, file, ty, 0));
        assert_eq!(
            kind_of("build_router", "src/serve.rs", NodeType::Function),
            "api"
        );
        assert_eq!(
            kind_of("cli_main", "src/main.rs", NodeType::Function),
            "entry"
        );
        assert_eq!(
            kind_of("async_run", "src/run.rs", NodeType::Function),
            "async"
        );
        assert_eq!(kind_of("Parser", "src/p.rs", NodeType::Struct), "klass");
        assert_eq!(kind_of("mod.rs", "src/mod.rs", NodeType::Module), "module");
        assert_eq!(kind_of("useState", "src/a.rs", NodeType::Function), "ui");
        assert_eq!(
            kind_of("check", "tests/thing.rs", NodeType::Function),
            "test"
        );
        assert_eq!(
            kind_of("Overview", "docs/guide.md", NodeType::Concept),
            "concept"
        );
        assert_eq!(
            kind_of("run_once", "src/run.rs", NodeType::Function),
            "function"
        );

        // Every kind must map to a real tag class, or table tags fall back wrongly.
        for kind in [
            "api", "entry", "async", "klass", "module", "ui", "test", "concept", "function",
        ] {
            let tag = suggest_tag("x", "code", "en", kind);
            assert!(tag.contains("class=\"tag tag-"), "{kind} produced {tag}");
        }
    }

    #[test]
    fn mermaid_init_matches_python_font_formatting() {
        assert!(mermaid_init(1.0, "LR").contains(r#""fontSize":"15.0px""#));
        assert!(mermaid_init(0.7, "LR").contains(r#""fontSize":"10.5px""#));
        // Scale is clamped to [0.65, 1.8].
        assert!(mermaid_init(9.0, "LR").contains(r#""fontSize":"27.0px""#));
    }

    #[test]
    fn humanize_label_shortens_long_snake_case() {
        assert_eq!(humanize_label("main.rs", ""), "main.rs");
        assert_eq!(humanize_label(".render()", ""), "render()");
        assert_eq!(
            humanize_label("a_very_long_snake_case_function_name_here", ""),
            "function name here"
        );
        assert_eq!(humanize_label("", "src/lib.rs"), "lib.rs");
    }
}
