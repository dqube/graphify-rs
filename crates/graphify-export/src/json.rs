//! Python-compat `node_link_data` JSON export.
//!
//! Writes `graph.json` matching the exact schema produced by the reference
//! Python `graphify.export.to_json` — relation names translated
//! (`defines → contains`, `imports → imports_from`), every node carries
//! `_origin`, `file_type`, `norm_label`, plus a top-level `hyperedges`
//! array and (when available) `built_at_commit`.

use std::collections::HashMap;
use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use graphify_core::graph::KnowledgeGraph;
use graphify_core::py_compat::write_python_compat_json;
use tracing::info;

/// Export graph to `graph.json` in Python-compat NetworkX `node_link_data`
/// format. `community_labels` feeds the per-node `community_name` field;
/// pass `None` when labels aren't available (e.g. `--no-llm` builds).
pub fn export_json(
    graph: &KnowledgeGraph,
    output_dir: &Path,
    community_labels: Option<&HashMap<usize, String>>,
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(output_dir)?;
    let path = output_dir.join("graph.json");
    let file = fs::File::create(&path)?;
    let writer = BufWriter::new(file);

    let nodes = graph.nodes();
    let edges = graph.edges();
    write_python_compat_json(writer, &nodes, &edges, &graph.hyperedges, community_labels)?;

    info!(path = %path.display(), "exported graph JSON");
    Ok(path)
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
            id: "a".into(),
            label: "A".into(),
            source_file: "test.rs".into(),
            source_location: None,
            node_type: NodeType::Class,
            community: None,
            extra: HashMap::new(),
        })
        .unwrap();
        kg.add_node(GraphNode {
            id: "b".into(),
            label: "B".into(),
            source_file: "test.rs".into(),
            source_location: None,
            node_type: NodeType::Function,
            community: None,
            extra: HashMap::new(),
        })
        .unwrap();
        kg.add_edge(GraphEdge {
            source: "a".into(),
            target: "b".into(),
            relation: "defines".into(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: "test.rs".into(),
            source_location: None,
            weight: 1.0,
            provenance: None,
            extra: HashMap::new(),
        })
        .unwrap();
        kg
    }

    #[test]
    fn export_json_writes_python_compat_shape() {
        let dir = tempfile::tempdir().unwrap();
        let kg = sample_graph();
        let path = export_json(&kg, dir.path(), None).unwrap();
        assert!(path.exists());

        let content: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for k in ["directed", "multigraph", "graph", "nodes", "links", "hyperedges"] {
            assert!(content.get(k).is_some(), "missing top-level key {k}");
        }
        // Relation was translated to python vocabulary.
        assert_eq!(
            content["links"][0]["relation"],
            serde_json::Value::String("contains".into())
        );
        // Nodes carry _origin / file_type / norm_label.
        let n = &content["nodes"][0];
        assert_eq!(n["_origin"], serde_json::Value::String("ast".into()));
        assert!(n.get("file_type").is_some());
        assert!(n.get("norm_label").is_some());
        // node_type is dropped.
        assert!(n.get("node_type").is_none());
    }
}
