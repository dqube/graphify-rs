//! Verilog / SystemVerilog extractor: modules, classes, functions, tasks, includes.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use super::{end_line_at, infer_calls, line_of, make_edge, make_file_node, make_node, path_str};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use regex::Regex;

static RE_MODULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:module|interface|package)\s+(\w+)").unwrap());
static RE_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(?:virtual\s+)?class\s+(\w+)").unwrap());
static RE_FUNC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:function|task)\s+(?:automatic\s+|static\s+)*(?:[\w:]+\s+)?(\w+)")
        .unwrap()
});
static RE_INCLUDE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*`include\s+"([^"]+)""#).unwrap());
static RE_IMPORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*import\s+([\w:*]+)").unwrap());

pub(crate) fn extract_verilog(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let lines: Vec<&str> = source.lines().collect();
    let ps = path_str(path);

    for cap in RE_MODULE.captures_iter(source) {
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

    for cap in RE_CLASS.captures_iter(source) {
        let line = line_of(source, &cap);
        let node = make_node(&cap[1], path, NodeType::Class, line);
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
    let matches: Vec<_> = RE_FUNC.captures_iter(source).collect();
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

    let push_import = |module: &str, line: usize, result: &mut ExtractionResult| {
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
    };

    for cap in RE_INCLUDE.captures_iter(source) {
        push_import(&cap[1], line_of(source, &cap), &mut result);
    }
    for cap in RE_IMPORT.captures_iter(source) {
        push_import(&cap[1], line_of(source, &cap), &mut result);
    }

    result.edges.extend(infer_calls(&functions, &lines, path));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_verilog_entities() {
        let src = "`include \"defs.svh\"\nimport uvm_pkg::*;\n\
                   module alu(input a, output b);\n\
                   function automatic logic add(input logic x);\n  return x;\nendfunction\n\
                   endmodule\n";
        let r = extract_verilog(Path::new("alu.sv"), src);
        assert!(r.nodes.iter().any(|n| n.label == "alu"));
        assert!(r.nodes.iter().any(|n| n.label == "add"));
        assert!(r.nodes.iter().any(|n| n.label == "defs.svh"));
        assert!(r.nodes.iter().any(|n| n.label == "uvm_pkg::*"));
    }
}
