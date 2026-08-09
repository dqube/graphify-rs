//! JSON extractor: top-level keys as module nodes (simple key-value extraction).

use std::path::Path;

use super::{make_edge, make_file_node, make_node};
use graphify_core::confidence::Confidence;
use graphify_core::model::{ExtractionResult, NodeType};

/// Maximum number of top-level keys extracted from a single JSON document.
const MAX_KEYS: usize = 100;

pub(crate) fn extract_json(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let Ok(value) = serde_json::from_str::<serde_json::Value>(source) else {
        // Malformed JSON: keep just the file node.
        return result;
    };

    match &value {
        serde_json::Value::Object(map) => {
            for (i, key) in map.keys().enumerate() {
                if i >= MAX_KEYS {
                    break;
                }
                let node = make_node(key, path, NodeType::Module, 1);
                let node_id = node.id.clone();
                result.nodes.push(node);
                result.edges.push(make_edge(
                    &file_id,
                    &node_id,
                    "defines",
                    path,
                    Confidence::Extracted,
                ));
            }
        }
        serde_json::Value::Array(_) => {
            let node = make_node("[array]", path, NodeType::Module, 1);
            let node_id = node.id.clone();
            result.nodes.push(node);
            result.edges.push(make_edge(
                &file_id,
                &node_id,
                "defines",
                path,
                Confidence::Extracted,
            ));
        }
        _ => {}
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_top_level_keys() {
        let src = r#"{"name": "demo", "version": "1.0.0", "dependencies": {"a": "1.0"}}"#;
        let r = extract_json(Path::new("package.json"), src);
        assert!(r.nodes.iter().any(|n| n.label == "name"));
        assert!(r.nodes.iter().any(|n| n.label == "version"));
        assert!(r.nodes.iter().any(|n| n.label == "dependencies"));
    }

    #[test]
    fn malformed_json_keeps_file_node() {
        let r = extract_json(Path::new("broken.json"), "{not json");
        assert_eq!(r.nodes.len(), 1);
        assert!(r.edges.is_empty());
    }
}
