//! FalkorDB load-script export.
//!
//! Port of the Python `graphify export falkordb` path. FalkorDB is
//! OpenCypher-compatible but speaks the Redis protocol: statements arrive one
//! at a time through `GRAPH.QUERY <graph> "<cypher>"` rather than as a bulk
//! Cypher script, so a plain `graph.cypher` file (see [`crate::cypher`]) is not
//! loadable as-is.
//!
//! What this emits instead is a redis-cli command script:
//!
//! ```text
//! redis-cli -h localhost -p 6379 < graphify-out/graph.falkordb.cypher
//! ```
//!
//! Every statement uses `MERGE`, so re-running the script upserts rather than
//! duplicating — matching the Python push path's guarantee.
//!
//! Two deliberate choices are worth knowing about:
//!
//! - **No `#` comment lines.** redis-cli has no comment syntax; a `#` line is
//!   parsed as a command and answers `ERR unknown command`. The header is
//!   emitted as `ECHO` commands instead, which are valid, harmless, and print
//!   the banner while the script loads.
//! - **No index creation.** `CREATE INDEX` has no `IF NOT EXISTS` form in
//!   FalkorDB and errors on the second run, which would cost the script its
//!   clean re-runnability. Add `CREATE INDEX FOR (n:Label) ON (n.id)` by hand
//!   before the first load of a very large graph.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use graphify_core::graph::KnowledgeGraph;
use tracing::info;

/// FalkorDB keys every graph in the instance by name; this is the key the
/// Python exporter defaults to, so both implementations load into one place.
const GRAPH_KEY: &str = "graphify";

/// Export the graph as a runnable FalkorDB load script (`graph.falkordb.cypher`).
pub fn export_falkordb(graph: &KnowledgeGraph, output_dir: &Path) -> anyhow::Result<PathBuf> {
    let mut script = String::with_capacity(4096);

    writeln!(
        script,
        "ECHO \"graphify -> FalkorDB | graph: {GRAPH_KEY} | {} nodes | {} edges\"",
        graph.node_count(),
        graph.edge_count(),
    )?;
    writeln!(
        script,
        "ECHO \"MERGE-based, safe to re-run: redis-cli -h HOST -p PORT < graph.falkordb.cypher\""
    )?;

    for node in graph.nodes() {
        let label = safe_label(&node.node_type.to_string());
        let mut props: Vec<String> = vec![
            format!("n.label = '{}'", cypher_escape(&node.label)),
            format!("n.node_type = '{}'", cypher_escape(&label)),
            format!("n.source_file = '{}'", cypher_escape(&node.source_file)),
        ];
        if let Some(loc) = &node.source_location
            && !loc.is_empty()
        {
            props.push(format!("n.source_location = '{}'", cypher_escape(loc)));
        }
        if let Some(cid) = node.community {
            props.push(format!("n.community = {cid}"));
        }
        let cypher = format!(
            "MERGE (n:{label} {{id: '{}'}}) SET {}",
            cypher_escape(&node.id),
            props.join(", "),
        );
        writeln!(script, "GRAPH.QUERY {GRAPH_KEY} \"{}\"", redis_arg(&cypher))?;
    }

    for edge in graph.edges() {
        let rel_type = safe_relation(&edge.relation);
        let cypher = format!(
            "MATCH (a {{id: '{src}'}}), (b {{id: '{tgt}'}}) MERGE (a)-[r:{rel}]->(b) \
             SET r.relation = '{relation}', r.confidence = '{confidence}', \
             r.confidence_score = {score:.2}, r.source_file = '{file}', r.weight = {weight:.2}",
            src = cypher_escape(&edge.source),
            tgt = cypher_escape(&edge.target),
            rel = rel_type,
            relation = cypher_escape(&edge.relation),
            confidence = edge.confidence,
            score = edge.confidence_score,
            file = cypher_escape(&edge.source_file),
            weight = edge.weight,
        );
        writeln!(script, "GRAPH.QUERY {GRAPH_KEY} \"{}\"", redis_arg(&cypher))?;
    }

    fs::create_dir_all(output_dir)?;
    let path = output_dir.join("graph.falkordb.cypher");
    fs::write(&path, &script)?;
    info!(
        path = %path.display(),
        nodes = graph.node_count(),
        edges = graph.edge_count(),
        "exported FalkorDB load script"
    );
    Ok(path)
}

/// Escape a value for a single-quoted Cypher string literal.
///
/// Mirrors the private helper in [`crate::cypher`]; kept local because that one
/// is module-private and the two formats must be able to diverge (FalkorDB
/// rejects some Neo4j-only escapes).
fn cypher_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Escape an already-escaped Cypher statement for a double-quoted redis-cli
/// argument.
///
/// This is the *second* escaping layer and must run after [`cypher_escape`]:
/// redis-cli unquotes the argument first, so a Cypher `\n` (backslash + n) has
/// to reach it as `\\n`. Only backslash and double quote need handling —
/// [`cypher_escape`] has already removed every raw newline.
fn redis_arg(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Sanitize a Cypher node label, which cannot be parameterized and is therefore
/// an injection point. Falls back to `Entity`, like the Python exporter.
fn safe_label(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(char::is_ascii_alphanumeric).collect();
    if cleaned.is_empty() {
        "Entity".to_string()
    } else {
        cleaned
    }
}

/// Sanitize a relationship type: uppercase, `[A-Z0-9_]` only, `RELATED_TO` when
/// nothing survives.
fn safe_relation(relation: &str) -> String {
    let cleaned: String = relation
        .to_uppercase()
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() || c.is_ascii_digit() {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.chars().all(|c| c == '_') {
        "RELATED_TO".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::model::{GraphEdge, GraphNode, NodeType};
    use std::collections::HashMap;

    fn sample_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(GraphNode {
            id: "my_class".into(),
            label: "MyClass".into(),
            source_file: "src/main.rs".into(),
            source_location: Some("L42".into()),
            node_type: NodeType::Class,
            community: Some(3),
            extra: HashMap::new(),
        })
        .unwrap();
        kg.add_node(GraphNode {
            id: "helper".into(),
            label: "Helper".into(),
            source_file: "src/util.rs".into(),
            source_location: None,
            node_type: NodeType::Function,
            community: None,
            extra: HashMap::new(),
        })
        .unwrap();
        kg.add_edge(GraphEdge {
            source: "my_class".into(),
            target: "helper".into(),
            relation: "imports-from".into(),
            confidence: Confidence::Extracted,
            confidence_score: 0.9,
            source_file: "src/main.rs".into(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: HashMap::new(),
        })
        .unwrap();
        kg
    }

    #[test]
    fn export_falkordb_emits_graph_query_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = export_falkordb(&sample_graph(), dir.path()).unwrap();
        assert_eq!(path.file_name().unwrap(), "graph.falkordb.cypher");
        let script = fs::read_to_string(&path).unwrap();

        assert!(script.starts_with("ECHO \"graphify -> FalkorDB | graph: graphify | 2 nodes"));
        assert!(script.contains(
            "GRAPH.QUERY graphify \"MERGE (n:Class {id: 'my_class'}) SET n.label = 'MyClass'"
        ));
        assert!(script.contains("n.community = 3"));
        assert!(script.contains("n.source_location = 'L42'"));
        // A node without a community or location omits those properties.
        assert!(script.contains("MERGE (n:Function {id: 'helper'})"));
        assert!(script.contains(
            "MATCH (a {id: 'my_class'}), (b {id: 'helper'}) MERGE (a)-[r:IMPORTS_FROM]->(b)"
        ));
        assert!(script.contains("r.confidence_score = 0.90"));
    }

    #[test]
    fn every_line_is_a_redis_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = export_falkordb(&sample_graph(), dir.path()).unwrap();
        let script = fs::read_to_string(&path).unwrap();

        // A '#' comment would be parsed as a command by redis-cli and fail, so
        // the script must contain nothing but ECHO / GRAPH.QUERY lines.
        for line in script.lines() {
            assert!(
                line.starts_with("ECHO ") || line.starts_with("GRAPH.QUERY graphify \""),
                "not a redis command: {line}"
            );
            assert!(line.ends_with('"'), "unterminated argument: {line}");
        }
    }

    #[test]
    fn export_falkordb_empty_graph() {
        let dir = tempfile::tempdir().unwrap();
        let kg = KnowledgeGraph::new();
        let path = export_falkordb(&kg, dir.path()).unwrap();
        let script = fs::read_to_string(&path).unwrap();

        assert!(script.contains("| 0 nodes | 0 edges"));
        assert!(!script.contains("GRAPH.QUERY"));
        assert_eq!(script.lines().count(), 2);
    }

    #[test]
    fn quotes_and_newlines_survive_both_escaping_layers() {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(GraphNode {
            id: "weird".into(),
            // Contains every character that breaks one of the two layers.
            label: "it's a \"quote\"\nand a \\slash".into(),
            source_file: "src/a.rs".into(),
            source_location: None,
            node_type: NodeType::Function,
            community: None,
            extra: HashMap::new(),
        })
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = export_falkordb(&kg, dir.path()).unwrap();
        let script = fs::read_to_string(&path).unwrap();

        // Single output line: the raw newline never reaches the file.
        assert_eq!(script.lines().count(), 3);
        // redis-cli unescapes \\' -> \' (Cypher escape) and \\\\ -> \\.
        assert!(script.contains(r#"n.label = 'it\\'s a \"quote\"\\nand a \\\\slash'"#));
    }

    #[test]
    fn safe_relation_sanitizes() {
        assert_eq!(safe_relation("calls"), "CALLS");
        assert_eq!(safe_relation("imports from"), "IMPORTS_FROM");
        assert_eq!(safe_relation("!!!"), "RELATED_TO");
        assert_eq!(safe_relation(""), "RELATED_TO");
    }

    #[test]
    fn safe_label_strips_injection_attempts() {
        assert_eq!(safe_label("Class"), "Class");
        assert_eq!(safe_label("Cl}) DETACH DELETE (n"), "ClDETACHDELETEn");
        assert_eq!(safe_label("--"), "Entity");
    }
}
