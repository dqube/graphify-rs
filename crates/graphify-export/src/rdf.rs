//! RDF 1.1 Turtle export.
//!
//! There is no Python counterpart for this format — it exists so a graphify
//! graph can be loaded into a triplestore (Jena, Blazegraph, Oxigraph, GraphDB)
//! and queried with SPARQL alongside other RDF datasets.
//!
//! Shape of the emitted document:
//!
//! - one `gfy:KnowledgeGraph` resource carrying the node/edge counts,
//! - one IRI-identified resource per graph node with `rdf:type`, `rdfs:label`,
//!   and the source-file provenance triples,
//! - one direct triple per edge, using a predicate minted from the relation
//!   name (`calls` -> `gfy:calls`).
//!
//! Edge attributes (confidence, weight) are deliberately *not* emitted. Plain
//! RDF cannot annotate a triple without reification, and reifying every edge
//! would roughly quadruple the file for metadata that SPARQL consumers of a
//! code graph rarely filter on. Callers that need edge attributes should use
//! the Cypher or GraphML exports instead.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use graphify_core::graph::KnowledgeGraph;
use graphify_core::model::GraphNode;
use tracing::info;

/// Vocabulary namespace for graphify's own terms (classes and predicates).
///
/// Minted under the project repository rather than a bare `urn:` so the IRIs
/// stay globally unique, dereferenceable-looking, and stable across releases.
const VOCAB_NS: &str = "https://github.com/TtTRz/graphify-rs/ns#";

/// Instance namespace: every graph node gets `NODE_NS + percent-encoded id`.
const NODE_NS: &str = "https://github.com/TtTRz/graphify-rs/id/node/";

/// Subject for the graph-level metadata resource.
const GRAPH_IRI: &str = "https://github.com/TtTRz/graphify-rs/id/graph";

/// Export the graph as an RDF 1.1 Turtle document (`graph.ttl`).
///
/// The document is always syntactically complete, including for an empty
/// graph, so downstream tooling never has to special-case a missing file.
pub fn export_rdf(graph: &KnowledgeGraph, output_dir: &Path) -> anyhow::Result<PathBuf> {
    let mut ttl = String::with_capacity(4096);

    writeln!(ttl, "# graphify knowledge graph — RDF 1.1 Turtle export")?;
    writeln!(
        ttl,
        "# {} nodes, {} edges",
        graph.node_count(),
        graph.edge_count()
    )?;
    writeln!(ttl)?;
    writeln!(
        ttl,
        "@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> ."
    )?;
    writeln!(
        ttl,
        "@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> ."
    )?;
    writeln!(ttl, "@prefix gfy: <{VOCAB_NS}> .")?;
    writeln!(ttl)?;

    writeln!(ttl, "<{GRAPH_IRI}>")?;
    writeln!(ttl, "    a gfy:KnowledgeGraph ;")?;
    writeln!(ttl, "    gfy:nodeCount {} ;", graph.node_count())?;
    writeln!(ttl, "    gfy:edgeCount {} .", graph.edge_count())?;

    for node in graph.nodes() {
        writeln!(ttl)?;
        write_node(&mut ttl, node)?;
    }

    // RDF triples are set semantics, but parallel edges in the source graph
    // would still emit byte-identical lines; drop them so the file stays small.
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut edge_block = String::new();
    for edge in graph.edges() {
        let predicate = relation_predicate(&edge.relation);
        let key = (edge.source.clone(), predicate.clone(), edge.target.clone());
        if !seen.insert(key) {
            continue;
        }
        writeln!(
            edge_block,
            "<{}> gfy:{predicate} <{}> .",
            node_iri(&edge.source),
            node_iri(&edge.target),
        )?;
    }
    if !edge_block.is_empty() {
        writeln!(ttl)?;
        ttl.push_str(&edge_block);
    }

    fs::create_dir_all(output_dir)?;
    let path = output_dir.join("graph.ttl");
    fs::write(&path, &ttl)?;
    info!(path = %path.display(), nodes = graph.node_count(), "exported RDF Turtle");
    Ok(path)
}

/// Emit the predicate-object list for one node as a single Turtle statement.
fn write_node(out: &mut String, node: &GraphNode) -> std::fmt::Result {
    let class = local_name(&node.node_type.to_string(), "Node");
    writeln!(out, "<{}>", node_iri(&node.id))?;
    writeln!(out, "    a gfy:Node, gfy:{class} ;")?;
    writeln!(out, "    gfy:nodeId \"{}\" ;", escape_literal(&node.id))?;
    writeln!(out, "    rdfs:label \"{}\" ;", escape_literal(&node.label))?;
    if let Some(loc) = &node.source_location
        && !loc.is_empty()
    {
        writeln!(out, "    gfy:sourceLocation \"{}\" ;", escape_literal(loc))?;
    }
    if let Some(cid) = node.community {
        writeln!(out, "    gfy:community {cid} ;")?;
    }
    writeln!(out, "    gfy:inGraph <{GRAPH_IRI}> ;")?;
    // Last predicate closes the statement, so it must always be present.
    writeln!(
        out,
        "    gfy:sourceFile \"{}\" .",
        escape_literal(&node.source_file)
    )
}

/// Percent-encode a node id into an absolute IRI.
///
/// Node ids come straight from source files and routinely contain spaces,
/// angle brackets, and non-ASCII text — all of which terminate or invalidate an
/// `<...>` IRI reference. Everything outside the RFC 3987 unreserved set is
/// percent-encoded byte-wise; `/`, `:`, `.` and `-` are kept so paths stay
/// readable in a SPARQL result table.
fn node_iri(id: &str) -> String {
    let mut out = String::with_capacity(NODE_NS.len() + id.len());
    out.push_str(NODE_NS);
    for &byte in id.as_bytes() {
        let c = char::from(byte);
        if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '~' | '/' | ':') {
            out.push(c);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// Escape a string for use inside a Turtle `"..."` literal.
///
/// Covers the four escapes that actually break parsers (backslash, quote,
/// newline, carriage return) plus tab/backspace/form-feed, and falls back to
/// `\uXXXX` for any remaining control character.
fn escape_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Reduce arbitrary text to a Turtle `PN_LOCAL` that needs no escaping.
///
/// Turtle allows a much wider local-name grammar, but restricting to
/// `[A-Za-z0-9_]` sidesteps every reserved-character edge case (`.` may not end
/// a local name, `-` may not start one) for a negligible loss of fidelity.
fn local_name(raw: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Mint the predicate local name for an edge relation (`calls` -> `calls`).
fn relation_predicate(relation: &str) -> String {
    local_name(relation, "relatedTo")
}

#[cfg(test)]
mod tests {
    use super::*;
    use graphify_core::confidence::Confidence;
    use graphify_core::model::{GraphEdge, GraphNode, NodeType};
    use std::collections::HashMap;

    fn node(id: &str, label: &str, file: &str, ty: NodeType) -> GraphNode {
        GraphNode {
            id: id.into(),
            label: label.into(),
            source_file: file.into(),
            source_location: Some("L42".into()),
            node_type: ty,
            community: Some(0),
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

    fn sample_graph() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(node("my_class", "MyClass", "src/main.rs", NodeType::Class))
            .unwrap();
        kg.add_node(node("helper", "Helper", "src/util.rs", NodeType::Function))
            .unwrap();
        kg.add_edge(edge("my_class", "helper", "calls")).unwrap();
        kg
    }

    #[test]
    fn export_rdf_emits_prefixes_and_triples() {
        let dir = tempfile::tempdir().unwrap();
        let path = export_rdf(&sample_graph(), dir.path()).unwrap();
        assert_eq!(path.file_name().unwrap(), "graph.ttl");
        let ttl = fs::read_to_string(&path).unwrap();

        assert!(ttl.contains("@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> ."));
        assert!(ttl.contains("@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> ."));
        assert!(ttl.contains(&format!("@prefix gfy: <{VOCAB_NS}> .")));

        assert!(ttl.contains("a gfy:Node, gfy:Class ;"));
        assert!(ttl.contains("a gfy:Node, gfy:Function ;"));
        assert!(ttl.contains("rdfs:label \"MyClass\" ;"));
        assert!(ttl.contains("gfy:sourceFile \"src/main.rs\" ."));
        assert!(ttl.contains("gfy:sourceLocation \"L42\" ;"));
        assert!(ttl.contains("gfy:community 0 ;"));
        assert!(ttl.contains(&format!(
            "<{NODE_NS}my_class> gfy:calls <{NODE_NS}helper> ."
        )));
    }

    #[test]
    fn export_rdf_empty_graph_is_still_valid_turtle() {
        let dir = tempfile::tempdir().unwrap();
        let kg = KnowledgeGraph::new();
        let path = export_rdf(&kg, dir.path()).unwrap();
        let ttl = fs::read_to_string(&path).unwrap();

        assert!(ttl.contains("@prefix gfy:"));
        assert!(ttl.contains("gfy:nodeCount 0 ;"));
        assert!(ttl.contains("gfy:edgeCount 0 ."));
        // No node or edge statements, but the prefix block is complete.
        assert!(!ttl.contains("gfy:nodeId"));
        assert_balanced_statements(&ttl);
    }

    #[test]
    fn literals_with_quotes_newlines_and_backslashes_are_escaped() {
        let mut kg = KnowledgeGraph::new();
        kg.add_node(node(
            "weird",
            "say \"hi\"\nand \\escape\ttab",
            "src/a\"b.rs",
            NodeType::Function,
        ))
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = export_rdf(&kg, dir.path()).unwrap();
        let ttl = fs::read_to_string(&path).unwrap();

        assert!(ttl.contains(r#"rdfs:label "say \"hi\"\nand \\escape\ttab" ;"#));
        assert!(ttl.contains(r#"gfy:sourceFile "src/a\"b.rs" ."#));
        // A raw newline inside a single-quoted literal would break the parse.
        assert!(!ttl.contains("say \"hi\"\nand"));
        assert_balanced_statements(&ttl);
    }

    #[test]
    fn control_characters_fall_back_to_unicode_escapes() {
        assert_eq!(escape_literal("a\u{1}b"), "a\\u0001b");
        assert_eq!(escape_literal("a\u{7f}b"), "a\\u007Fb");
    }

    #[test]
    fn node_iri_percent_encodes_unsafe_characters() {
        assert_eq!(
            node_iri("src/mod.rs::foo bar<x>"),
            format!("{NODE_NS}src/mod.rs::foo%20bar%3Cx%3E")
        );
        // Multi-byte UTF-8 is encoded byte-wise.
        assert_eq!(node_iri("é"), format!("{NODE_NS}%C3%A9"));
    }

    #[test]
    fn relation_predicate_sanitizes_and_falls_back() {
        assert_eq!(relation_predicate("imports_from"), "imports_from");
        assert_eq!(relation_predicate("is-a"), "is_a");
        assert_eq!(relation_predicate("!!!"), "relatedTo");
    }

    #[test]
    fn parallel_edges_are_deduplicated() {
        let mut kg = sample_graph();
        kg.add_edge(edge("my_class", "helper", "calls")).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = export_rdf(&kg, dir.path()).unwrap();
        let ttl = fs::read_to_string(&path).unwrap();

        assert_eq!(ttl.matches("gfy:calls").count(), 1);
    }

    /// Cheap structural check: every non-comment line either continues a
    /// statement (`;` / `,`), terminates one (`.`), or is a bare subject IRI —
    /// and every line has an even number of unescaped double quotes.
    fn assert_balanced_statements(ttl: &str) {
        for line in ttl.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let mut quotes = 0;
            let mut escaped = false;
            for c in trimmed.chars() {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => quotes += 1,
                    _ => {}
                }
            }
            assert_eq!(quotes % 2, 0, "unbalanced quotes in: {trimmed}");
            assert!(
                trimmed.ends_with(['.', ';', ',', '>']),
                "statement does not terminate: {trimmed}"
            );
        }
    }
}
