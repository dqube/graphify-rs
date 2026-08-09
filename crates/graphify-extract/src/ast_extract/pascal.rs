//! Pascal / Delphi extractor: units, programs, procedures, functions, `uses` imports.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use super::{end_line_at, infer_calls, line_of, make_edge, make_file_node, make_node, path_str};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use regex::Regex;

static RE_UNIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\s*(?:unit|program|library|package)\s+(\w+)").unwrap());
static RE_ROUTINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:procedure|function|constructor|destructor)\s+([\w.]+)").unwrap()
});
static RE_USES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\s*uses\s+([^;]+);").unwrap());

pub(crate) fn extract_pascal(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let lines: Vec<&str> = source.lines().collect();
    let ps = path_str(path);

    for cap in RE_UNIT.captures_iter(source) {
        let line = line_of(source, &cap);
        let node = make_node(&cap[1], path, NodeType::Module, line);
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
    let matches: Vec<_> = RE_ROUTINE.captures_iter(source).collect();
    for (i, cap) in matches.iter().enumerate() {
        let name = cap[1].to_string();
        let start_line = line_of(source, cap);
        let end_line = end_line_at(source, matches.get(i + 1));
        let node = make_node(&name, path, NodeType::Function, start_line);
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

    for cap in RE_USES.captures_iter(source) {
        let line = line_of(source, &cap);
        for unit in cap[1].split(',') {
            let unit = unit.trim();
            if unit.is_empty() {
                continue;
            }
            let import_id = make_id(&[&ps, "import", &unit.to_lowercase()]);
            result.nodes.push(GraphNode {
                id: import_id.clone(),
                label: unit.to_string(),
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
    }

    result.edges.extend(infer_calls(&functions, &lines, path));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_pascal_entities() {
        let src = "unit MathUtils;\ninterface\nuses SysUtils, Classes;\n\
                   function Add(a, b: Integer): Integer;\nimplementation\n\
                   function Add(a, b: Integer): Integer;\nbegin\n  Result := a + b;\nend;\nend.\n";
        let r = extract_pascal(Path::new("MathUtils.pas"), src);
        assert!(r.nodes.iter().any(|n| n.label == "MathUtils"));
        assert!(r.nodes.iter().any(|n| n.label == "Add"));
        assert!(r.nodes.iter().any(|n| n.label == "SysUtils"));
        assert!(r.nodes.iter().any(|n| n.label == "Classes"));
    }
}
