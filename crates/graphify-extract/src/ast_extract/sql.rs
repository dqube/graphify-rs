//! SQL extractor: tables, views, functions, and procedures (simplified regex).

use std::path::Path;
use std::sync::LazyLock;

use super::{line_of, make_edge, make_file_node, make_node};
use graphify_core::confidence::Confidence;
use graphify_core::model::{ExtractionResult, NodeType};
use regex::Regex;

static RE_TABLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?im)^\s*create\s+(?:or\s+replace\s+)?(?:temp(?:orary)?\s+)?table\s+(?:if\s+not\s+exists\s+)?([\w."`\[\]]+)"#,
    )
    .unwrap()
});
static RE_VIEW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?im)^\s*create\s+(?:or\s+replace\s+)?(?:materialized\s+)?view\s+([\w."`\[\]]+)"#,
    )
    .unwrap()
});
static RE_ROUTINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?im)^\s*create\s+(?:or\s+replace\s+)?(?:function|procedure|proc)\s+([\w."`\[\]]+)"#,
    )
    .unwrap()
});

fn clean_name(raw: &str) -> &str {
    raw.trim_matches(|c| c == '"' || c == '`' || c == '[' || c == ']')
}

pub(crate) fn extract_sql(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let push = |cap: &regex::Captures<'_>, node_type: NodeType, result: &mut ExtractionResult| {
        let name = clean_name(&cap[1]);
        if name.is_empty() {
            return;
        }
        let line = line_of(source, cap);
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
    };

    for cap in RE_TABLE.captures_iter(source) {
        push(&cap, NodeType::Class, &mut result);
    }
    for cap in RE_VIEW.captures_iter(source) {
        push(&cap, NodeType::Interface, &mut result);
    }
    for cap in RE_ROUTINE.captures_iter(source) {
        push(&cap, NodeType::Function, &mut result);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tables_views_and_routines() {
        let src = "CREATE TABLE users (id INT);\n\
                   create or replace view active_users as select * from users;\n\
                   CREATE PROCEDURE refresh_users() LANGUAGE sql AS $$ select 1 $$;\n";
        let r = extract_sql(Path::new("schema.sql"), src);
        assert!(r.nodes.iter().any(|n| n.label == "users"));
        assert!(r.nodes.iter().any(|n| n.label == "active_users"));
        assert!(r.nodes.iter().any(|n| n.label == "refresh_users"));
    }
}
