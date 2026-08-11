//! Python graphify JSON schema compatibility helpers.
//!
//! The reference Python implementation (`graphify.export.to_json`) writes a
//! very specific NetworkX `node_link_data` shape:
//!
//! - Every node carries `_origin`, `file_type`, `norm_label`, `community`,
//!   `community_name` (optional). No `node_type`. No provenance.
//! - Every edge carries `relation`, `confidence`, `confidence_score`,
//!   `weight`, `source_file`, `source_location`. No `provenance`,
//!   no `imported_symbols`.
//! - Top-level has `hyperedges` (always present) and `built_at_commit`
//!   (from `git rev-parse HEAD`).
//! - Relation vocabulary: `contains` (file → symbol), `imports_from`
//!   (module → import). The rust extractors internally use `defines` and
//!   `imports`; this module translates on write so callers can keep the
//!   internal names.
//! - `source_file` paths are stored without a leading `./`.
//!
//! This module owns the translation. Anything that writes `graph.json` for
//! external consumption (Python parity, downstream tools) should route
//! through [`write_python_compat_json`].

use std::io::Write;

use serde_json::{Value, json};

use crate::model::{GraphEdge, GraphNode, Hyperedge, NodeType};

/// Translate an internal relation name to the Python-compat vocabulary.
///
/// Keeps every unrelated relation unchanged so extractor-specific relations
/// (`queries`, `writes`, `binds`, `rationale_for`, `cites`, `references`)
/// pass through verbatim.
pub fn translate_relation(rel: &str) -> &str {
    match rel {
        "defines" => "contains",
        "imports" => "imports_from",
        other => other,
    }
}

/// Derive the Python-style `file_type` for a node.
///
/// Priority:
/// 1. An explicit `file_type` inside `extra` wins (rationale/doc_ref/etc.
///    already set this).
/// 2. Otherwise, derive from `NodeType`: `Paper → paper`, `Image → image`,
///    `Concept → concept`, and everything else (file/function/class/…)
///    becomes `code` — matching the way Python groups them.
pub fn file_type_for(node: &GraphNode) -> String {
    if let Some(v) = node.extra.get("file_type")
        && let Some(s) = v.as_str()
    {
        return s.to_string();
    }
    match node.node_type {
        NodeType::Paper => "paper".to_string(),
        NodeType::Image => "image".to_string(),
        NodeType::Concept => "concept".to_string(),
        _ => "code".to_string(),
    }
}

/// Lower-case + strip Unicode combining marks. Matches Python's
/// `_strip_diacritics(label).lower()` used to build the search-index key.
pub fn norm_label(label: &str) -> String {
    // Ordinary ASCII fast-path — the majority of code identifiers.
    if label.is_ascii() {
        return label.to_ascii_lowercase();
    }
    let lower = label.to_lowercase();
    // Combining marks are Unicode general category Mn/Mc/Me. We can spot them
    // by decomposition heuristic: NFD would split them, but pulling in
    // unicode-normalization is heavy. Instead skip combining code points in
    // the standard block U+0300–U+036F, which covers Latin diacritics.
    lower
        .chars()
        .filter(|c| {
            let cp = *c as u32;
            !(0x0300..=0x036F).contains(&cp)
        })
        .collect()
}

/// Strip a leading `./` from a stored source path so the on-disk JSON
/// matches Python (which stores paths relative to the project root without
/// the current-directory prefix).
pub fn normalize_source_path(path: &str) -> String {
    path.strip_prefix("./")
        .or_else(|| path.strip_prefix(".\\"))
        .unwrap_or(path)
        .to_string()
}

/// Build the JSON object for a single node in the Python-compat shape.
///
/// Fields written, in insertion order (matches Python by convention):
/// `id`, `label`, `source_file`, `source_location`, `community`,
/// `community_name` (optional), `_origin`, `file_type`, `norm_label`, plus
/// any `extra` entries other than the ones this function already handled.
pub fn python_node(
    node: &GraphNode,
    community_labels: Option<&std::collections::HashMap<usize, String>>,
) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("id".into(), Value::String(node.id.clone()));
    obj.insert("label".into(), Value::String(node.label.clone()));
    obj.insert(
        "source_file".into(),
        Value::String(normalize_source_path(&node.source_file)),
    );
    if let Some(loc) = &node.source_location {
        obj.insert("source_location".into(), Value::String(loc.clone()));
    }
    obj.insert(
        "community".into(),
        node.community.map(|c| json!(c)).unwrap_or(Value::Null),
    );
    if let (Some(cid), Some(labels)) = (node.community, community_labels)
        && let Some(name) = labels.get(&cid)
    {
        obj.insert("community_name".into(), Value::String(name.clone()));
    }
    obj.insert("_origin".into(), Value::String("ast".into()));
    obj.insert("file_type".into(), Value::String(file_type_for(node)));
    obj.insert("norm_label".into(), Value::String(norm_label(&node.label)));
    // Additive to Python's field set rather than a divergence from it: Python
    // consumers ignore the key, while dropping it costs us real behaviour.
    // Without it every node reloads as the `NodeType` default, so `query`
    // reported "type: File" for functions and structs alike, and seed ranking
    // had no way to prefer a definition over an import that re-exports it.
    obj.insert(
        "node_type".into(),
        serde_json::to_value(&node.node_type).unwrap_or(Value::Null),
    );

    // Pass through extra entries that don't conflict with the fields above.
    // `file_type` already handled; skip `node_type` if it leaked into extra.
    for (k, v) in &node.extra {
        if !matches!(
            k.as_str(),
            "file_type" | "node_type" | "_origin" | "norm_label"
        ) {
            obj.entry(k.clone()).or_insert(v.clone());
        }
    }
    Value::Object(obj)
}

/// Build the JSON object for a single edge in the Python-compat shape.
///
/// Emits: `source`, `target`, `relation` (translated), `confidence`,
/// `confidence_score`, `source_file` (normalized), `source_location`
/// (optional), `weight`, plus a `context` entry when the internal edge had
/// import-context metadata.
pub fn python_edge(edge: &GraphEdge) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("source".into(), Value::String(edge.source.clone()));
    obj.insert("target".into(), Value::String(edge.target.clone()));
    obj.insert(
        "relation".into(),
        Value::String(translate_relation(&edge.relation).to_string()),
    );
    obj.insert(
        "confidence".into(),
        serde_json::to_value(&edge.confidence)
            .unwrap_or_else(|_| Value::String("EXTRACTED".into())),
    );
    obj.insert("confidence_score".into(), json!(edge.confidence_score));
    obj.insert(
        "source_file".into(),
        Value::String(normalize_source_path(&edge.source_file)),
    );
    if let Some(loc) = &edge.source_location {
        obj.insert("source_location".into(), Value::String(loc.clone()));
    }
    obj.insert("weight".into(), json!(edge.weight));

    // Python collapses import symbol lists into a single `context` string
    // when present; rust stashes them under `extra.imported_symbols`.
    if let Some(syms) = edge.extra.get("imported_symbols")
        && let Some(arr) = syms.as_array()
    {
        let joined = arr
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(",");
        if !joined.is_empty() {
            obj.insert("context".into(), Value::String(joined));
        }
    }
    // Pass through any other extras verbatim (rare — most extra fields are
    // internal metadata we deliberately drop for parity).
    for (k, v) in &edge.extra {
        if !matches!(k.as_str(), "imported_symbols" | "provenance") {
            obj.entry(k.clone()).or_insert(v.clone());
        }
    }
    Value::Object(obj)
}

/// Best-effort `git rev-parse HEAD` for the `built_at_commit` field. Returns
/// `None` when the current directory is not a git repo or git is missing.
fn git_head() -> Option<String> {
    use std::process::Command;
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Write a full graph in Python-compat JSON to `writer`.
///
/// Streams directly rather than materializing a `Value` tree so this stays
/// usable for large graphs; the per-node/per-edge shape is built by
/// [`python_node`] and [`python_edge`].
///
/// `community_labels` is optional; when supplied it feeds the per-node
/// `community_name` field (mirrors Python's `community_labels` parameter).
pub fn write_python_compat_json<W: Write>(
    writer: W,
    nodes: &[&GraphNode],
    edges: &[&GraphEdge],
    hyperedges: &[Hyperedge],
    community_labels: Option<&std::collections::HashMap<usize, String>>,
) -> serde_json::Result<()> {
    use serde::ser::SerializeMap;
    use serde_json::ser::{PrettyFormatter, Serializer};

    let formatter = PrettyFormatter::with_indent(b"  ");
    let mut ser = Serializer::with_formatter(writer, formatter);
    let mut map = serde::Serializer::serialize_map(&mut ser, Some(7))?;

    map.serialize_entry("directed", &false)?;
    map.serialize_entry("multigraph", &false)?;
    map.serialize_entry("graph", &serde_json::Map::new())?;

    let node_values: Vec<Value> = nodes
        .iter()
        .map(|n| python_node(n, community_labels))
        .collect();
    map.serialize_entry("nodes", &node_values)?;

    let edge_values: Vec<Value> = edges.iter().map(|e| python_edge(e)).collect();
    map.serialize_entry("links", &edge_values)?;

    map.serialize_entry("hyperedges", hyperedges)?;

    if let Some(commit) = git_head() {
        map.serialize_entry("built_at_commit", &commit)?;
    }

    map.end()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confidence::Confidence;
    use crate::model::NodeType;
    use std::collections::HashMap;

    fn node(id: &str, label: &str, ft: Option<&str>, nt: NodeType) -> GraphNode {
        let mut extra = HashMap::new();
        if let Some(ft) = ft {
            extra.insert("file_type".into(), Value::String(ft.into()));
        }
        GraphNode {
            id: id.into(),
            label: label.into(),
            source_file: "./src/foo.rs".into(),
            source_location: Some("L1".into()),
            node_type: nt,
            community: Some(3),
            extra,
        }
    }

    fn edge(src: &str, tgt: &str, rel: &str) -> GraphEdge {
        GraphEdge {
            source: src.into(),
            target: tgt.into(),
            relation: rel.into(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: "./src/foo.rs".into(),
            source_location: Some("L2".into()),
            weight: 1.0,
            provenance: Some("test".into()),
            extra: HashMap::new(),
        }
    }

    #[test]
    fn translate_relation_renames_defines_and_imports() {
        assert_eq!(translate_relation("defines"), "contains");
        assert_eq!(translate_relation("imports"), "imports_from");
        assert_eq!(translate_relation("calls"), "calls");
        assert_eq!(translate_relation("references"), "references");
    }

    #[test]
    fn file_type_prefers_extra_then_node_type() {
        let n = node("a", "A", Some("document"), NodeType::File);
        assert_eq!(file_type_for(&n), "document");
        let n = node("b", "B", None, NodeType::Concept);
        assert_eq!(file_type_for(&n), "concept");
        let n = node("c", "C", None, NodeType::Function);
        assert_eq!(file_type_for(&n), "code");
    }

    #[test]
    fn norm_label_lowercases_and_strips_combining_marks() {
        // NFD-decomposed input: 'e' + combining acute (U+0301) → 'e'
        assert_eq!(norm_label("Cafe\u{0301}"), "cafe");
        assert_eq!(norm_label("Hello_World"), "hello_world");
    }

    #[test]
    fn normalize_source_path_strips_dot_slash() {
        assert_eq!(normalize_source_path("./src/foo.rs"), "src/foo.rs");
        assert_eq!(normalize_source_path("src/foo.rs"), "src/foo.rs");
    }

    #[test]
    fn python_node_emits_expected_schema() {
        let n = node("a", "Foo", Some("code"), NodeType::File);
        let v = python_node(&n, None);
        let obj = v.as_object().unwrap();
        for k in [
            "id",
            "label",
            "source_file",
            "source_location",
            "community",
            "_origin",
            "file_type",
            "norm_label",
        ] {
            assert!(obj.contains_key(k), "missing key {k}");
        }
        // `node_type` is emitted on top of Python's field set rather than in
        // place of any of it. Dropping it made every node reload as the
        // default type, which cost `query` its type display and left seed
        // ranking unable to tell a definition from an import of it.
        assert_eq!(obj["node_type"], Value::String("file".into()));
        assert_eq!(obj["_origin"], Value::String("ast".into()));
        assert_eq!(obj["file_type"], Value::String("code".into()));
        assert_eq!(obj["source_file"], Value::String("src/foo.rs".into()));
    }

    #[test]
    fn python_node_includes_community_name_when_labels_provided() {
        let n = node("a", "Foo", Some("code"), NodeType::File);
        let mut labels = HashMap::new();
        labels.insert(3, "cluster-3".to_string());
        let v = python_node(&n, Some(&labels));
        assert_eq!(v["community_name"], Value::String("cluster-3".into()));
    }

    #[test]
    fn python_edge_translates_and_drops_provenance() {
        let e = edge("a", "b", "defines");
        let v = python_edge(&e);
        let obj = v.as_object().unwrap();
        assert_eq!(obj["relation"], Value::String("contains".into()));
        assert!(
            !obj.contains_key("provenance"),
            "provenance must be dropped"
        );
        assert_eq!(obj["source_file"], Value::String("src/foo.rs".into()));
    }

    #[test]
    fn python_edge_folds_imported_symbols_into_context() {
        let mut e = edge("a", "b", "imports");
        e.extra
            .insert("imported_symbols".into(), json!(["Alpha", "Beta"]));
        let v = python_edge(&e);
        assert_eq!(v["relation"], Value::String("imports_from".into()));
        assert_eq!(v["context"], Value::String("Alpha,Beta".into()));
    }

    #[test]
    fn write_python_compat_json_has_all_top_level_keys() {
        let n1 = node("a", "A", Some("code"), NodeType::File);
        let n2 = node("b", "B", Some("code"), NodeType::Function);
        let e = edge("a", "b", "defines");
        let nodes = vec![&n1, &n2];
        let edges = vec![&e];
        let mut buf: Vec<u8> = Vec::new();
        write_python_compat_json(&mut buf, &nodes, &edges, &[], None).unwrap();
        let v: Value = serde_json::from_slice(&buf).unwrap();
        for k in [
            "directed",
            "multigraph",
            "graph",
            "nodes",
            "links",
            "hyperedges",
        ] {
            assert!(v.get(k).is_some(), "missing top-level key {k}");
        }
        assert_eq!(v["links"][0]["relation"], Value::String("contains".into()));
    }
}
