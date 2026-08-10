//! Delphi/Lazarus Form (`.dfm` / `.lfm`) extractor.
//!
//! Form files describe a component tree in a textual property syntax:
//!
//! ```text
//! object Form1: TForm1
//!   Caption = 'My Form'
//!   object Panel1: TPanel
//!     Caption = 'Left'
//!     object Button1: TButton
//!       OnClick = Button1Click
//!     end
//!   end
//! end
//! ```
//!
//! Extraction produces:
//!
//! - A `Class` node per `object Name: TType` block (name and class both
//!   preserved via label / `extra`).
//! - `contains` edges from parent objects to nested objects, mirroring the
//!   DFM tree.
//! - `binds` edges from the component owning an event handler (`OnClick`,
//!   `OnChange`, etc.) to a node named after the handler. That handler
//!   normally lives in the paired `.pas` unit; a later cross-file resolution
//!   pass can rewrite these to the actual method node.

use std::path::Path;
use std::sync::LazyLock;

use super::{make_edge, make_file_node, make_node};
use graphify_core::confidence::Confidence;
use graphify_core::model::{ExtractionResult, NodeType};
use regex::Regex;

/// `object Name: TType` — component declaration. The name may include an
/// optional array index suffix (`Panel1[0]`), which we ignore for the label.
static RE_OBJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\s*(?:object|inherited|inline)\s+(\w+)(?:\[\d+\])?\s*:\s*(\w+)").unwrap()
});
/// `end` closes the innermost open `object` block. Match on a whole-line
/// `end` to avoid picking up `end` inside string literals.
static RE_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*end\b").unwrap());
/// `OnClick = HandlerName` — an event property whose value is a method
/// reference. Property names always begin with `On` in Delphi/Lazarus form
/// files.
static RE_EVENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\s*(On\w+)\s*=\s*([A-Za-z_]\w*)\s*$").unwrap());

/// One token emitted by the tree walker, tagged with the byte position it
/// was seen at so we can order objects, ends, and events into one stream.
#[derive(Debug)]
enum Token<'a> {
    Object { name: &'a str, kind: &'a str, line: usize },
    End,
    Event { property: &'a str, handler: &'a str, line: usize },
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset].lines().count() + 1
}

pub(crate) fn extract_pascal_form(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    // Collect all events, objects, and ends in one pass, tagged with byte
    // offset so we can sort them into document order.
    let mut tokens: Vec<(usize, Token<'_>)> = Vec::new();
    for cap in RE_OBJECT.captures_iter(source) {
        let offset = cap.get(0).unwrap().start();
        tokens.push((
            offset,
            Token::Object {
                name: cap.get(1).unwrap().as_str(),
                kind: cap.get(2).unwrap().as_str(),
                line: line_of(source, offset),
            },
        ));
    }
    for m in RE_END.find_iter(source) {
        tokens.push((m.start(), Token::End));
    }
    for cap in RE_EVENT.captures_iter(source) {
        let offset = cap.get(0).unwrap().start();
        tokens.push((
            offset,
            Token::Event {
                property: cap.get(1).unwrap().as_str(),
                handler: cap.get(2).unwrap().as_str(),
                line: line_of(source, offset),
            },
        ));
    }
    tokens.sort_by_key(|(offset, _)| *offset);

    // Walk the token stream, maintaining the current object stack so that
    // nested `object`s get `contains` edges from their parent, and events
    // attach to the innermost open object.
    let mut stack: Vec<String> = Vec::new();
    for (_, token) in tokens {
        match token {
            Token::Object { name, kind, line } => {
                let mut node = make_node(name, path, NodeType::Class, line);
                node.extra.insert(
                    "component_type".to_string(),
                    serde_json::Value::String(kind.to_string()),
                );
                let node_id = node.id.clone();
                result.nodes.push(node);

                let parent = stack.last().cloned().unwrap_or_else(|| file_id.clone());
                result.edges.push(make_edge(
                    &parent,
                    &node_id,
                    "contains",
                    path,
                    Confidence::Extracted,
                ));

                stack.push(node_id);
            }
            Token::End => {
                stack.pop();
            }
            Token::Event { property, handler, line } => {
                let Some(owner) = stack.last().cloned() else {
                    continue;
                };
                let mut handler_node = make_node(handler, path, NodeType::Method, line);
                handler_node.extra.insert(
                    "event_binding".to_string(),
                    serde_json::Value::String(property.to_string()),
                );
                let handler_id = handler_node.id.clone();
                result.nodes.push(handler_node);
                result.edges.push(make_edge(
                    &owner,
                    &handler_id,
                    "binds",
                    path,
                    Confidence::Extracted,
                ));
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_object_tree_and_events() {
        let src = "object Form1: TForm1\n  Caption = 'Main'\n  object Panel1: TPanel\n    Caption = 'Left'\n    object Button1: TButton\n      Caption = 'OK'\n      OnClick = Button1Click\n    end\n  end\n  object Label1: TLabel\n    Caption = 'Hi'\n  end\nend\n";
        let r = extract_pascal_form(Path::new("MainForm.dfm"), src);
        // Every `object` becomes a node.
        for name in ["Form1", "Panel1", "Button1", "Label1"] {
            assert!(
                r.nodes.iter().any(|n| n.label == name),
                "expected node {name}"
            );
        }
        // Button1 → Panel1 → Form1 nesting via contains edges.
        let form_id = r.nodes.iter().find(|n| n.label == "Form1").unwrap().id.clone();
        let panel_id = r.nodes.iter().find(|n| n.label == "Panel1").unwrap().id.clone();
        let button_id = r.nodes.iter().find(|n| n.label == "Button1").unwrap().id.clone();
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "contains" && e.source == form_id && e.target == panel_id),
            "Form1 should contain Panel1"
        );
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "contains" && e.source == panel_id && e.target == button_id),
            "Panel1 should contain Button1"
        );
        // OnClick binds Button1 to Button1Click.
        assert!(
            r.edges
                .iter()
                .any(|e| e.relation == "binds" && e.source == button_id
                    && r.nodes
                        .iter()
                        .any(|n| n.id == e.target && n.label == "Button1Click")),
            "expected Button1 --binds--> Button1Click"
        );
    }

    #[test]
    fn component_type_survives_on_extra() {
        let src = "object Btn: TBitBtn\nend\n";
        let r = extract_pascal_form(Path::new("F.dfm"), src);
        let btn = r.nodes.iter().find(|n| n.label == "Btn").unwrap();
        assert_eq!(
            btn.extra.get("component_type"),
            Some(&serde_json::Value::String("TBitBtn".to_string()))
        );
    }

    #[test]
    fn inherited_and_inline_variants_are_recognised() {
        let src = "inherited ChildForm: TChildForm\n  inline SubForm: TSubForm\n  end\nend\n";
        let r = extract_pascal_form(Path::new("Child.dfm"), src);
        assert!(r.nodes.iter().any(|n| n.label == "ChildForm"));
        assert!(r.nodes.iter().any(|n| n.label == "SubForm"));
    }
}
