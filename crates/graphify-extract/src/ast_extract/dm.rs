//! DM (Dream Maker / BYOND) extractor: type paths, procs, and verbs.

use std::path::Path;
use std::sync::LazyLock;

use super::{end_line_at, infer_calls, line_of, make_edge, make_file_node, make_node};
use graphify_core::confidence::Confidence;
use graphify_core::model::{ExtractionResult, NodeType};
use regex::Regex;

static RE_TYPE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^/((?:\w+/)*\w+)\s*$").unwrap());
static RE_PROC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^(/(?:\w+/)*)(?:proc|verb)/(\w+)\s*\(").unwrap());

pub(crate) fn extract_dm(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let lines: Vec<&str> = source.lines().collect();

    for cap in RE_TYPE.captures_iter(source) {
        let full_path = &cap[1];
        let name = full_path.rsplit('/').next().unwrap_or(full_path);
        let line = line_of(source, &cap);
        let node = make_node(name, path, NodeType::Class, line);
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
    let matches: Vec<_> = RE_PROC.captures_iter(source).collect();
    for (i, cap) in matches.iter().enumerate() {
        let name = cap[2].to_string();
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

    result.edges.extend(infer_calls(&functions, &lines, path));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_dm_types_and_procs() {
        let src = "/mob/player\n\tvar/health = 100\n\
                   /mob/player/proc/attack(target)\n\tdefend()\n\
                   /mob/player/proc/defend()\n\treturn\n";
        let r = extract_dm(Path::new("player.dm"), src);
        assert!(r.nodes.iter().any(|n| n.label == "player"));
        assert!(r.nodes.iter().any(|n| n.label == "attack"));
        assert!(r.nodes.iter().any(|n| n.label == "defend"));
    }
}
