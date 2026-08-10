//! SCIP JSON ingestion (simplified subset).
//!
//! Reads a SCIP-style JSON index and converts it into symbol nodes and
//! relationship edges. This is **not** a full SCIP protobuf implementation; it
//! consumes the flattened JSON shape that SCIP-style dumps and LLM-generated
//! indexes commonly produce, where occurrences hang off each symbol rather than
//! off the document.
//!
//! Expected shape:
//! ```text
//! documents[]:     { relative_path, language, symbols[] }
//! symbols[]:       { symbol, kind, display_name, documentation[],
//!                    relationships[], occurrences[] }
//! relationships[]: { symbol, is_reference, is_implementation,
//!                    is_type_definition, is_definition }
//! occurrences[]:   { range[], symbol, symbol_roles }
//! ```
//!
//! Every emitted edge is endpoint-safe: a first pass indexes every symbol, and
//! a target that resolves to nothing gets a stub node rather than a dangling
//! edge that graph assembly would silently drop.
//!
//! The index describes *other* files, so no node is emitted for the index file
//! itself.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::{make_edge, path_str};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use graphify_security::sanitize_label;
use serde_json::Value;

/// A symbol collected in pass 1, kept so pass 2 need not re-walk the tree.
struct SymbolRecord<'a> {
    node_id: String,
    symbol: String,
    doc_path: String,
    raw: &'a Value,
}

/// Node id for a symbol, scoped to the document that declares it.
///
/// Scoping by document is what lets two files declare the same local symbol
/// name without collapsing into one last-writer-wins node.
fn scip_node_id(symbol: &str, doc_path: &str) -> String {
    make_id(&["scip", doc_path, symbol])
}

/// The readable part of a SCIP symbol: everything after the final `#`.
///
/// Type symbols conventionally *end* in `#` (`…/Greeter#`), which leaves an
/// empty suffix, so the full symbol is the fallback.
fn symbol_suffix(symbol: &str) -> &str {
    let suffix = symbol.rsplit('#').next().unwrap_or("");
    if suffix.is_empty() { symbol } else { suffix }
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// SCIP relationship flags are only honoured when the JSON value is literally
/// `true`. External indexes routinely carry the *string* `"false"`, which is
/// truthy in many languages and must not read as a set flag.
fn is_true(v: &Value, key: &str) -> bool {
    v.get(key) == Some(&Value::Bool(true))
}

fn relation_for(rel: &Value) -> &'static str {
    if is_true(rel, "is_implementation") {
        "scip_impl"
    } else if is_true(rel, "is_type_definition") {
        "scip_typed"
    } else if is_true(rel, "is_definition") {
        "scip_def"
    } else {
        "scip_ref"
    }
}

/// 1-based line from the first occurrence's range, if it is a sane integer.
///
/// A JSON `true` cannot reach `as_u64` here, so a malformed `range: [true]`
/// cannot turn into a bogus line number.
fn first_occurrence_line(raw: &Value) -> Option<u64> {
    raw.get("occurrences")?
        .as_array()?
        .first()?
        .get("range")?
        .as_array()?
        .first()?
        .as_u64()
        .filter(|n| *n > 0)
}

/// Map a SCIP symbol kind onto a graph node type.
fn node_type_for(kind: &str) -> NodeType {
    match kind.to_ascii_lowercase().as_str() {
        "class" => NodeType::Class,
        "struct" => NodeType::Struct,
        "interface" | "protocol" => NodeType::Interface,
        "enum" => NodeType::Enum,
        "trait" => NodeType::Trait,
        "method" => NodeType::Method,
        "function" => NodeType::Function,
        "namespace" => NodeType::Namespace,
        "variable" | "field" | "property" | "parameter" => NodeType::Variable,
        "constant" => NodeType::Constant,
        "package" => NodeType::Package,
        _ => NodeType::Module,
    }
}

pub(crate) fn extract_scip(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let Ok(doc) = serde_json::from_str::<Value>(source) else {
        return result;
    };
    let Some(documents) = doc.get("documents").and_then(Value::as_array) else {
        return result;
    };

    let fallback_path = path_str(path);

    // ── pass 1: index every symbol ───────────────────────────────────────────
    // Two indices, so a relationship can prefer a same-document target:
    //   per_doc — (symbol, doc_path) -> node id
    //   global  — symbol -> node ids, used only when unambiguous
    let mut per_doc: HashMap<(String, String), String> = HashMap::new();
    let mut global: HashMap<String, Vec<String>> = HashMap::new();
    let mut records: Vec<SymbolRecord<'_>> = Vec::new();

    for document in documents {
        let doc_path = str_field(document, "relative_path")
            .unwrap_or(&fallback_path)
            .to_string();
        let Some(symbols) = document.get("symbols").and_then(Value::as_array) else {
            continue;
        };
        for raw in symbols {
            let Some(symbol) = str_field(raw, "symbol").filter(|s| !s.is_empty()) else {
                continue;
            };
            let node_id = scip_node_id(symbol, &doc_path);
            per_doc
                .entry((symbol.to_string(), doc_path.clone()))
                .or_insert_with(|| node_id.clone());
            // Duplicate records inside one document share a node id; that is
            // not cross-document ambiguity, so keep the candidate list unique.
            let candidates = global.entry(symbol.to_string()).or_default();
            if !candidates.contains(&node_id) {
                candidates.push(node_id.clone());
            }
            records.push(SymbolRecord {
                node_id,
                symbol: symbol.to_string(),
                doc_path: doc_path.clone(),
                raw,
            });
        }
    }

    // ── pass 2: emit nodes, then relationship edges ──────────────────────────
    let mut seen_nodes: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<(String, String, String)> = HashSet::new();

    for record in &records {
        emit_symbol_node(&mut result, &mut seen_nodes, record);
    }
    for record in &records {
        emit_relationships(
            &mut result,
            &mut seen_nodes,
            &mut seen_edges,
            record,
            &per_doc,
            &global,
            path,
        );
    }

    result
}

fn emit_symbol_node(
    result: &mut ExtractionResult,
    seen: &mut HashSet<String>,
    record: &SymbolRecord<'_>,
) {
    if !seen.insert(record.node_id.clone()) {
        return;
    }
    let kind = str_field(record.raw, "kind").unwrap_or("unknown");
    let label = str_field(record.raw, "display_name")
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| symbol_suffix(&record.symbol));

    let mut extra: HashMap<String, Value> = HashMap::new();
    extra.insert(
        "scip_symbol".to_string(),
        Value::String(record.symbol.clone()),
    );
    extra.insert("scip_kind".to_string(), Value::String(kind.to_string()));
    if let Some(doc) = record
        .raw
        .get("documentation")
        .and_then(Value::as_array)
        .and_then(|d| d.first())
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        extra.insert(
            "scip_description".to_string(),
            Value::String(doc.to_string()),
        );
    }

    result.nodes.push(GraphNode {
        id: record.node_id.clone(),
        label: sanitize_label(label),
        source_file: record.doc_path.clone(),
        source_location: first_occurrence_line(record.raw).map(|l| format!("L{l}")),
        node_type: node_type_for(kind),
        community: None,
        extra,
    });
}

fn emit_relationships(
    result: &mut ExtractionResult,
    seen_nodes: &mut HashSet<String>,
    seen_edges: &mut HashSet<(String, String, String)>,
    record: &SymbolRecord<'_>,
    per_doc: &HashMap<(String, String), String>,
    global: &HashMap<String, Vec<String>>,
    path: &Path,
) {
    let Some(relationships) = record.raw.get("relationships").and_then(Value::as_array) else {
        return;
    };
    let line = first_occurrence_line(record.raw).map(|l| format!("L{l}"));

    for rel in relationships {
        let Some(target_symbol) = str_field(rel, "symbol").filter(|s| !s.is_empty()) else {
            continue;
        };

        let target_id = resolve_target(target_symbol, &record.doc_path, per_doc, global)
            .unwrap_or_else(|| {
                // Unresolved or ambiguous: emit a stub rather than guessing,
                // scoped to the document that referenced it.
                let stub_id = scip_node_id(target_symbol, &record.doc_path);
                if seen_nodes.insert(stub_id.clone()) {
                    let mut extra: HashMap<String, Value> = HashMap::new();
                    extra.insert(
                        "scip_symbol".to_string(),
                        Value::String(target_symbol.to_string()),
                    );
                    extra.insert(
                        "scip_kind".to_string(),
                        Value::String("external".to_string()),
                    );
                    result.nodes.push(GraphNode {
                        id: stub_id.clone(),
                        label: sanitize_label(symbol_suffix(target_symbol)),
                        source_file: record.doc_path.clone(),
                        source_location: None,
                        node_type: NodeType::Module,
                        community: None,
                        extra,
                    });
                }
                stub_id
            });

        let relation = relation_for(rel);
        let key = (
            record.node_id.clone(),
            target_id.clone(),
            relation.to_string(),
        );
        if !seen_edges.insert(key) {
            continue;
        }
        let mut edge = make_edge(
            &record.node_id,
            &target_id,
            relation,
            path,
            Confidence::Extracted,
        );
        edge.source_file = record.doc_path.clone();
        edge.source_location = line.clone();
        edge.extra
            .insert("context".to_string(), Value::String("scip".to_string()));
        result.edges.push(edge);
    }
}

/// Same document wins; then a unique cross-document match; otherwise `None`.
///
/// Ambiguity deliberately resolves to `None` — with the symbol declared in
/// several documents there is no principled way to pick one, and a wrong edge
/// is worse than an explicit external stub.
fn resolve_target(
    target_symbol: &str,
    source_doc: &str,
    per_doc: &HashMap<(String, String), String>,
    global: &HashMap<String, Vec<String>>,
) -> Option<String> {
    if let Some(id) = per_doc.get(&(target_symbol.to_string(), source_doc.to_string())) {
        return Some(id.clone());
    }
    match global.get(target_symbol) {
        Some(candidates) if candidates.len() == 1 => Some(candidates[0].clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn run(json: &str) -> ExtractionResult {
        extract_scip(&PathBuf::from("/repo/index.scip.json"), json)
    }

    fn labels(r: &ExtractionResult) -> Vec<&str> {
        r.nodes.iter().map(|n| n.label.as_str()).collect()
    }

    #[test]
    fn emits_symbol_nodes_with_kind_and_line() {
        let r = run(
            r#"{"documents": [{"relative_path": "src/app.py", "symbols": [
                {"symbol": "scip-python mypkg app/Greeter#", "kind": "class",
                 "display_name": "Greeter", "documentation": ["Greets people."],
                 "occurrences": [{"range": [12, 0, 12, 7]}]}
            ]}]}"#,
        );
        assert_eq!(r.nodes.len(), 1);
        let n = &r.nodes[0];
        assert_eq!(n.label, "Greeter");
        assert_eq!(n.node_type, NodeType::Class);
        assert_eq!(n.source_file, "src/app.py");
        assert_eq!(n.source_location.as_deref(), Some("L12"));
        assert_eq!(n.extra["scip_description"], "Greets people.");
    }

    #[test]
    fn label_falls_back_to_the_symbol_suffix() {
        let r = run(r#"{"documents": [{"relative_path": "a.py", "symbols": [
                {"symbol": "scip-python pkg a/helper().", "kind": "function"}
            ]}]}"#);
        assert_eq!(labels(&r), vec!["scip-python pkg a/helper()."]);
    }

    #[test]
    fn relationship_flags_pick_the_relation() {
        for (flag, expected) in [
            ("is_implementation", "scip_impl"),
            ("is_type_definition", "scip_typed"),
            ("is_definition", "scip_def"),
        ] {
            let json = format!(
                r#"{{"documents": [{{"relative_path": "a.py", "symbols": [
                    {{"symbol": "A#", "relationships": [{{"symbol": "B#", "{flag}": true}}]}}
                ]}}]}}"#
            );
            let r = run(&json);
            assert_eq!(r.edges[0].relation, expected);
        }
    }

    #[test]
    fn a_string_false_does_not_set_a_flag() {
        // External JSON routinely carries "false" as a string; treating it as
        // truthy would mislabel a plain reference as an implementation.
        let r = run(r#"{"documents": [{"relative_path": "a.py", "symbols": [
                {"symbol": "A#", "relationships": [{"symbol": "B#", "is_implementation": "false"}]}
            ]}]}"#);
        assert_eq!(r.edges[0].relation, "scip_ref");
    }

    #[test]
    fn same_document_target_wins_over_another_document() {
        let r = run(r#"{"documents": [
                {"relative_path": "a.py", "symbols": [
                    {"symbol": "Dup#"},
                    {"symbol": "User#", "relationships": [{"symbol": "Dup#"}]}
                ]},
                {"relative_path": "b.py", "symbols": [{"symbol": "Dup#"}]}
            ]}"#);
        let edge = r.edges.iter().find(|e| e.relation == "scip_ref").unwrap();
        assert_eq!(edge.target, scip_node_id("Dup#", "a.py"));
    }

    #[test]
    fn unique_cross_document_target_resolves() {
        let r = run(r#"{"documents": [
                {"relative_path": "a.py", "symbols": [
                    {"symbol": "User#", "relationships": [{"symbol": "Other#"}]}
                ]},
                {"relative_path": "b.py", "symbols": [{"symbol": "Other#"}]}
            ]}"#);
        let edge = r.edges.iter().find(|e| e.relation == "scip_ref").unwrap();
        assert_eq!(edge.target, scip_node_id("Other#", "b.py"));
    }

    #[test]
    fn ambiguous_target_gets_a_stub_instead_of_a_guess() {
        let r = run(r#"{"documents": [
                {"relative_path": "a.py", "symbols": [{"symbol": "Amb#"}]},
                {"relative_path": "b.py", "symbols": [{"symbol": "Amb#"}]},
                {"relative_path": "c.py", "symbols": [
                    {"symbol": "User#", "relationships": [{"symbol": "Amb#"}]}
                ]}
            ]}"#);
        let edge = r.edges.iter().find(|e| e.relation == "scip_ref").unwrap();
        // Scoped to the referencing document, not either declaration.
        assert_eq!(edge.target, scip_node_id("Amb#", "c.py"));
        let stub = r.nodes.iter().find(|n| n.id == edge.target).unwrap();
        assert_eq!(stub.extra["scip_kind"], "external");
    }

    #[test]
    fn every_edge_endpoint_exists_as_a_node() {
        let r = run(r#"{"documents": [{"relative_path": "a.py", "symbols": [
                {"symbol": "A#", "relationships": [
                    {"symbol": "Nowhere#"}, {"symbol": "AlsoMissing#", "is_definition": true}
                ]}
            ]}]}"#);
        let ids: HashSet<&str> = r.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(!r.edges.is_empty());
        for e in &r.edges {
            assert!(ids.contains(e.source.as_str()), "dangling source");
            assert!(ids.contains(e.target.as_str()), "dangling target");
        }
    }

    #[test]
    fn duplicate_relationships_collapse() {
        let r = run(r#"{"documents": [{"relative_path": "a.py", "symbols": [
                {"symbol": "A#", "relationships": [{"symbol": "B#"}, {"symbol": "B#"}]}
            ]}]}"#);
        assert_eq!(r.edges.len(), 1);
    }

    #[test]
    fn malformed_range_cannot_become_a_line_number() {
        let r = run(r#"{"documents": [{"relative_path": "a.py", "symbols": [
                {"symbol": "A#", "occurrences": [{"range": [true, 0]}]}
            ]}]}"#);
        assert_eq!(r.nodes[0].source_location, None);
    }

    #[test]
    fn junk_input_yields_nothing() {
        for src in ["not json", "[]", r#"{"documents": "nope"}"#, "{}"] {
            let r = run(src);
            assert!(r.nodes.is_empty(), "unexpected nodes for {src}");
            assert!(r.edges.is_empty());
        }
    }

    #[test]
    fn non_object_entries_are_skipped_without_aborting() {
        let r = run(r#"{"documents": [
                "garbage",
                {"relative_path": "a.py", "symbols": ["junk", {"symbol": ""}, {"symbol": "Good#"}]}
            ]}"#);
        assert_eq!(labels(&r), vec!["Good#"]);
    }
}
