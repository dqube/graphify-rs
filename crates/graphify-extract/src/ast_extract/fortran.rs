//! Fortran extractor: modules, programs, subroutines, functions, and `use` imports.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use super::{end_line_at, infer_calls, line_of, make_edge, make_file_node, make_node, path_str};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use regex::Regex;

static RE_MODULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\s*module\s+(\w+)\s*$").unwrap());
static RE_PROGRAM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\s*program\s+(\w+)").unwrap());
static RE_SUBROUTINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?im)^\s*(?:recursive\s+|pure\s+|elemental\s+)*subroutine\s+(\w+)").unwrap()
});
static RE_FUNCTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^\s*(?:recursive\s+|pure\s+|elemental\s+)*(?:(?:integer|real|logical|complex|character|double\s+precision|type|class)(?:\s*\([^)]*\))?\s+)?function\s+(\w+)",
    )
    .unwrap()
});
static RE_USE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?im)^\s*use\s*(?:,\s*intrinsic\s*)?(?:::\s*)?(\w+)").unwrap());

pub(crate) fn extract_fortran(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let lines: Vec<&str> = source.lines().collect();
    let ps = path_str(path);

    for cap in RE_MODULE.captures_iter(source) {
        let name = &cap[1];
        // Skip "module procedure ..." interface bodies.
        if name.eq_ignore_ascii_case("procedure") {
            continue;
        }
        let line = line_of(source, &cap);
        let node = make_node(name, path, NodeType::Module, line);
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

    for cap in RE_PROGRAM.captures_iter(source) {
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
    for re in [&*RE_SUBROUTINE, &*RE_FUNCTION] {
        let matches: Vec<_> = re.captures_iter(source).collect();
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
    }

    for cap in RE_USE.captures_iter(source) {
        let module = &cap[1];
        let line = line_of(source, &cap);
        let import_id = make_id(&[&ps, "import", &module.to_lowercase()]);
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
    fn extracts_fortran_entities() {
        let src = "module geometry\nuse iso_fortran_env\ncontains\n\
                   function area(r)\n  real :: r, area\n  area = 3.14*r*r\nend function area\n\
                   end module geometry\n\
                   program main\ncall run()\nend program main\n\
                   subroutine run()\nend subroutine run\n";
        let r = extract_fortran(Path::new("geom.f90"), src);
        assert!(r.nodes.iter().any(|n| n.label == "geometry"));
        assert!(r.nodes.iter().any(|n| n.label == "main"));
        assert!(r.nodes.iter().any(|n| n.label == "area"));
        assert!(r.nodes.iter().any(|n| n.label == "run"));
        assert!(r.nodes.iter().any(|n| n.label == "iso_fortran_env"));
    }
}
