//! Salesforce Apex extractor: classes, triggers, methods, SOQL, and DML.
//!
//! SOQL queries and DML statements produce edges from the enclosing method
//! (or file, when the statement lives outside any method) to the sObject:
//!
//! - `[SELECT … FROM Account]` and `Database.query('SELECT … FROM Account')`
//!   emit `queries` edges.
//! - `insert new Account(…)` / `Database.insert(new Account(…))` and the
//!   other DML keywords (`update`, `upsert`, `delete`, `undelete`, `merge`)
//!   emit `writes` edges. Bare DML with a variable target (`insert acc`) is
//!   skipped because the sObject type cannot be recovered without a type
//!   system.

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

/// `[SELECT … FROM ObjectName …]` — inline SOQL literal.
static RE_SOQL_INLINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)\[\s*SELECT\b[^\]]*?\bFROM\s+(\w+)").unwrap()
});
/// `Database.query('SELECT … FROM ObjectName …')` (also `getQueryLocator`,
/// `countQuery`). Only the object in the outermost `FROM` clause is captured.
static RE_SOQL_DYNAMIC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)\bDatabase\.(?:query|getQueryLocator|countQuery)\s*\(\s*['"][^'"]*?\bFROM\s+(\w+)"#,
    )
    .unwrap()
});
/// `insert new Foo(…)` and friends — inline DML on a fresh sObject literal.
static RE_DML_INLINE_NEW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^\s*(insert|update|upsert|delete|undelete|merge)\s+new\s+(\w+)\s*\(",
    )
    .unwrap()
});
/// `Database.insert(new Foo(…))` and friends.
static RE_DML_DATABASE_NEW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)\bDatabase\.(insert|update|upsert|delete|undelete|merge)\s*\(\s*(?:[^)]*?\s+)?new\s+(\w+)\s*\(",
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

    // SOQL and DML edges. Each captured match's byte offset is turned into a
    // line number, then mapped to the enclosing method (if any) via
    // `functions`; unattributable statements attach to the file so the graph
    // still records the dependency.
    let attribution = |line: usize| -> &str {
        functions
            .iter()
            .find(|(_, _, start, end)| line >= *start && line <= *end)
            .map(|(_, id, _, _)| id.as_str())
            .unwrap_or(&file_id)
    };
    let mut soql_sink = |object: &str, line: usize| {
        let obj_node = make_node(object, path, NodeType::Class, line);
        let obj_id = obj_node.id.clone();
        result.nodes.push(obj_node);
        result.edges.push(make_edge(
            attribution(line),
            &obj_id,
            "queries",
            path,
            Confidence::Extracted,
        ));
    };
    for cap in RE_SOQL_INLINE.captures_iter(source) {
        soql_sink(&cap[1], line_of(source, &cap));
    }
    for cap in RE_SOQL_DYNAMIC.captures_iter(source) {
        soql_sink(&cap[1], line_of(source, &cap));
    }

    let mut dml_sink = |object: &str, line: usize| {
        let obj_node = make_node(object, path, NodeType::Class, line);
        let obj_id = obj_node.id.clone();
        result.nodes.push(obj_node);
        result.edges.push(make_edge(
            attribution(line),
            &obj_id,
            "writes",
            path,
            Confidence::Extracted,
        ));
    };
    for cap in RE_DML_INLINE_NEW.captures_iter(source) {
        dml_sink(&cap[2], line_of(source, &cap));
    }
    for cap in RE_DML_DATABASE_NEW.captures_iter(source) {
        dml_sink(&cap[2], line_of(source, &cap));
    }

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

    #[test]
    fn extracts_soql_inline_and_dynamic() {
        let src = "public class Q {\n  public void run() {\n    List<Account> a = [SELECT Id, Name FROM Account WHERE Name != null];\n    List<Contact> c = Database.query('SELECT Id FROM Contact LIMIT 10');\n  }\n}\n";
        let r = extract_apex(Path::new("Q.cls"), src);
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "queries" && e.target.ends_with("_account")),
            "expected queries edge to Account"
        );
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "queries" && e.target.ends_with("_contact")),
            "expected queries edge to Contact from Database.query"
        );
    }

    #[test]
    fn extracts_dml_writes_for_new_sobjects() {
        let src = "public class W {\n  public void run() {\n    insert new Account(Name = 'x');\n    Database.upsert(new Contact(Email='a@b.c'), Contact.Email);\n  }\n}\n";
        let r = extract_apex(Path::new("W.cls"), src);
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "writes" && e.target.ends_with("_account")),
            "expected writes edge to Account"
        );
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "writes" && e.target.ends_with("_contact")),
            "expected writes edge to Contact via Database.upsert"
        );
    }

    #[test]
    fn soql_and_dml_attribute_to_enclosing_method() {
        let src = "public class Q {\n  public void loader() {\n    List<Account> a = [SELECT Id FROM Account];\n    insert new Case();\n  }\n}\n";
        let r = extract_apex(Path::new("Q.cls"), src);
        let loader_id = r
            .nodes
            .iter()
            .find(|n| n.label == "loader")
            .map(|n| n.id.clone())
            .expect("loader method should be extracted");
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "queries" && e.source == loader_id),
            "queries edge should originate from the enclosing method"
        );
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "writes" && e.source == loader_id),
            "writes edge should originate from the enclosing method"
        );
    }
}
