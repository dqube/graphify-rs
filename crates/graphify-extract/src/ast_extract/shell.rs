//! Bash / Shell extractor: functions, sourced files, and call inference.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use super::{line_of, make_edge, make_file_node, make_node, path_str};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use regex::Regex;

static RE_FUNC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:function\s+)?([a-zA-Z_][\w.-]*)\s*\(\s*\)\s*(?:\{|$)").unwrap()
});
static RE_FUNC_KW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*function\s+([a-zA-Z_][\w.-]*)\s*(?:\{|$)").unwrap());
static RE_SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*(?:source|\.)\s+["']?([^\s"']+)["']?"#).unwrap());

pub(crate) fn extract_shell(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let lines: Vec<&str> = source.lines().collect();
    let ps = path_str(path);

    let mut functions: Vec<(String, String, usize, usize)> = Vec::new();
    let push_func = |cap: &regex::Captures<'_>,
                         funcs: &mut Vec<(String, String, usize, usize)>,
                         result: &mut ExtractionResult,
                         next_start: Option<usize>| {
        let name = cap[1].to_string();
        if matches!(name.as_str(), "if" | "while" | "for" | "until" | "case") {
            return;
        }
        let start_line = line_of(source, cap);
        let end_line = match next_start {
            Some(off) => source[..off].lines().count(),
            None => source.lines().count(),
        };
        let node = make_node(&name, path, NodeType::Function, start_line);
        let node_id = node.id.clone();
        funcs.push((name, node_id.clone(), start_line, end_line));
        result.nodes.push(node);
        result.edges.push(make_edge(
            &file_id,
            &node_id,
            "defines",
            path,
            Confidence::Extracted,
        ));
    };

    let matches: Vec<_> = RE_FUNC.captures_iter(source).collect();
    for (i, cap) in matches.iter().enumerate() {
        let next = matches.get(i + 1).map(|c| c.get(0).unwrap().start());
        push_func(cap, &mut functions, &mut result, next);
    }
    let kw_matches: Vec<_> = RE_FUNC_KW.captures_iter(source).collect();
    for (i, cap) in kw_matches.iter().enumerate() {
        let next = kw_matches.get(i + 1).map(|c| c.get(0).unwrap().start());
        push_func(cap, &mut functions, &mut result, next);
    }

    for cap in RE_SOURCE.captures_iter(source) {
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

    result.edges.extend(infer_shell_calls(&functions, &lines, path));
    result
}

/// Shell call inference: functions are invoked by bare name (no parentheses),
/// so match whole-word occurrences of known function names in each body.
fn infer_shell_calls(
    functions: &[(String, String, usize, usize)],
    source_lines: &[&str],
    path: &Path,
) -> Vec<graphify_core::model::GraphEdge> {
    let mut edges = Vec::new();
    for (_caller_name, caller_id, start, end) in functions {
        let body = source_lines
            .get(*start..*end)
            .unwrap_or_default()
            .join("\n");
        for (callee_name, callee_id, _, _) in functions {
            if caller_id == callee_id {
                continue;
            }
            let pattern = format!(r"\b{}\b", regex::escape(callee_name));
            if let Ok(re) = regex::Regex::new(&pattern)
                && re.is_match(&body)
            {
                edges.push(make_edge(
                    caller_id,
                    callee_id,
                    "calls",
                    path,
                    Confidence::Inferred,
                ));
            }
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_functions_and_sources() {
        let src = "#!/bin/bash\nsource ./lib.sh\n\ngreet() {\n  echo hi\n}\n\nfunction main {\n  greet\n}\n";
        let r = extract_shell(Path::new("run.sh"), src);
        assert!(r.nodes.iter().any(|n| n.label == "greet"));
        assert!(r.nodes.iter().any(|n| n.label == "main"));
        assert!(r.nodes.iter().any(|n| n.label == "./lib.sh"));
        assert!(r.edges.iter().any(|e| e.relation == "calls"));
    }
}
