//! Groovy extractor: classes, methods (typed and `def`), and imports.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use super::{end_line_at, infer_calls, line_of, make_edge, make_file_node, make_node, path_str};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use regex::Regex;

static RE_CLASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)(?:public\s+|private\s+|protected\s+)?(?:abstract\s+|static\s+|final\s+)*(class|interface|enum|trait)\s+(\w+)",
    )
    .unwrap()
});
static RE_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s+(?:public\s+|private\s+|protected\s+)?(?:static\s+)?(?:final\s+)?(?:synchronized\s+)?(?:def|void|[A-Z][\w<>\[\], ]*?)\s+(\w+)\s*\([^;]*?\)\s*(?:throws\s+[\w., ]+\s*)?\{",
    )
    .unwrap()
});
static RE_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^import\s+(?:static\s+)?([\w.]+(?:\.\*)?)").unwrap());

pub(crate) fn extract_groovy(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let lines: Vec<&str> = source.lines().collect();
    let ps = path_str(path);

    for cap in RE_CLASS.captures_iter(source) {
        let kind = &cap[1];
        let name = &cap[2];
        let line = line_of(source, &cap);
        let node_type = match kind {
            "interface" | "trait" => NodeType::Interface,
            "enum" => NodeType::Enum,
            _ => NodeType::Class,
        };
        let node = make_node(name, path, node_type, line);
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

    let mut functions: Vec<(String, String, usize, usize)> = Vec::new();
    let matches: Vec<_> = RE_METHOD.captures_iter(source).collect();
    for (i, cap) in matches.iter().enumerate() {
        let name = cap[1].to_string();
        let start_line = line_of(source, cap);
        let end_line = end_line_at(source, matches.get(i + 1));
        let node = make_node(&name, path, NodeType::Method, start_line);
        let node_id = node.id.clone();
        functions.push((name, node_id.clone(), start_line, end_line));
        result.nodes.push(node);
        result.edges.push(make_edge(
            &file_id,
            &node_id,
            "defines",
            path,
            Confidence::Extracted,
        ));
    }

    for cap in RE_IMPORT.captures_iter(source) {
        let module = &cap[1];
        let line = line_of(source, &cap);
        let import_id = make_id(&[&ps, "import", module]);
        result.nodes.push(GraphNode {
            id: import_id.clone(),
            label: module.to_string(),
            source_file: ps.clone(),
            source_location: Some(format!("L{line}")),
            node_type: NodeType::Package,
            community: None,
            extra: HashMap::new(),
        });
        result.edges.push(make_edge(
            &file_id,
            &import_id,
            "imports",
            path,
            Confidence::Extracted,
        ));
    }

    result.edges.extend(infer_calls(&functions, &lines, path));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_groovy_entities() {
        let src = "import groovy.json.JsonSlurper\n\
                   class BuildTool {\n  def compile() {\n    packageApp()\n  }\n  static void packageApp() {\n  }\n}\n";
        let r = extract_groovy(Path::new("build.gradle"), src);
        assert!(r.nodes.iter().any(|n| n.label == "BuildTool"));
        assert!(r.nodes.iter().any(|n| n.label == "compile"));
        assert!(r.nodes.iter().any(|n| n.label == "packageApp"));
        assert!(r.nodes.iter().any(|n| n.label == "groovy.json.JsonSlurper"));
    }
}
