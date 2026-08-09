//! Salesforce Apex extractor: classes, triggers, and methods.

use std::path::Path;
use std::sync::LazyLock;

use super::{end_line_at, infer_calls, line_of, make_edge, make_file_node, make_node};
use graphify_core::confidence::Confidence;
use graphify_core::model::{ExtractionResult, NodeType};
use regex::Regex;

static RE_CLASS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)(?:public\s+|private\s+|protected\s+|global\s+)?(?:abstract\s+|virtual\s+|with\s+sharing\s+|without\s+sharing\s+)*(class|interface|enum)\s+(\w+)",
    )
    .unwrap()
});
static RE_TRIGGER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*trigger\s+(\w+)\s+on\s+(\w+)\s*\(").unwrap()
});
static RE_METHOD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)^\s+(?:public\s+|private\s+|protected\s+|global\s+)?(?:static\s+)?(?:virtual\s+)?(?:override\s+)?(?:webservice\s+)?(?:\w+(?:<[^>]*>)?(?:\[\])?)\s+(\w+)\s*\([^;]*?\)\s*\{",
    )
    .unwrap()
});

pub(crate) fn extract_apex(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let lines: Vec<&str> = source.lines().collect();

    for cap in RE_CLASS.captures_iter(source) {
        let kind = &cap[1];
        let name = &cap[2];
        let line = line_of(source, &cap);
        let node_type = match kind {
            "interface" => NodeType::Interface,
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

    for cap in RE_TRIGGER.captures_iter(source) {
        let name = &cap[1];
        let object = &cap[2];
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
        // Edge from trigger to the sObject it fires on.
        let obj_node = make_node(object, path, NodeType::Class, line);
        let obj_id = obj_node.id.clone();
        result.nodes.push(obj_node);
        result.edges.push(make_edge(
            &node_id,
            &obj_id,
            "depends_on",
            path,
            Confidence::Inferred,
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

    result.edges.extend(infer_calls(&functions, &lines, path));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_apex_class_and_trigger() {
        let cls = "public class AccountService {\n  public static void recalc(List<Account> accts) {\n    save(accts);\n  }\n  private static void save(List<Account> accts) {\n  }\n}\n";
        let r = extract_apex(Path::new("AccountService.cls"), cls);
        assert!(r.nodes.iter().any(|n| n.label == "AccountService"));
        assert!(r.nodes.iter().any(|n| n.label == "recalc"));
        assert!(r.edges.iter().any(|e| e.relation == "calls"));

        let trg = "trigger AccountTrigger on Account (before insert) {\n}\n";
        let r2 = extract_apex(Path::new("AccountTrigger.trigger"), trg);
        assert!(r2.nodes.iter().any(|n| n.label == "AccountTrigger"));
        assert!(r2.nodes.iter().any(|n| n.label == "Account"));
    }
}
