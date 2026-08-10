//! Post-extraction pass that emits `references` edges from functions and
//! type definitions to the type names they mention.
//!
//! Ports the essential subset of `graphify.extractors.rust`'s type-ref
//! extraction from the Python reference. For every function extracted from
//! a Rust source file, walks the tree-sitter `parameters` and `return_type`
//! children and collects `type_identifier` / `generic_type` / etc.,
//! emitting one `references` edge per type name.
//!
//! Edge `extra.context` mirrors Python's vocabulary:
//! `parameter_type`, `return_type`, `generic_arg`, and `field`. Callers rely
//! on `context` for filtering/analysis in the same way the Python graph is
//! consumed.
//!
//! Target resolution: a `name → node_id` symbol table is built from every
//! struct/enum/trait/class node in the current extraction result, then
//! looked up per type. Unresolved names become lower-case stub node ids so
//! external types (`Vec`, `HashMap`, third-party crates) still get an
//! anchor node the graph can dedupe across files.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphEdge, GraphNode, NodeType};
use tree_sitter::{Node, Parser};

/// Public entry point: walk each Rust file whose extraction landed in
/// `result` and append type-ref `references` edges (and any stub type
/// nodes) in place.
pub fn augment_with_rust_type_refs(
    result: &mut ExtractionResult,
    paths_and_source: &[(&Path, Vec<u8>)],
) {
    // Symbol table: type name → node_id, drawn from every struct/enum/trait/
    // interface/class/method node already in `result`.
    let symbols: HashMap<String, String> = result
        .nodes
        .iter()
        .filter(|n| {
            matches!(
                n.node_type,
                NodeType::Struct
                    | NodeType::Enum
                    | NodeType::Trait
                    | NodeType::Class
                    | NodeType::Interface
                    | NodeType::Method
                    | NodeType::Function
            )
        })
        .map(|n| (n.label.trim_end_matches("()").to_string(), n.id.clone()))
        .collect();

    // For each Rust file, we already have function nodes with their id and
    // start line. We need to relocate them in the AST to walk their
    // parameters and return type. Build a lookup keyed by (file, line).
    let mut fn_index: HashMap<(String, usize), String> = HashMap::new();
    for n in &result.nodes {
        if !matches!(n.node_type, NodeType::Function | NodeType::Method) {
            continue;
        }
        if let Some(loc) = &n.source_location
            && let Some(line_str) = loc.strip_prefix('L')
            && let Ok(line) = line_str.parse::<usize>()
        {
            fn_index.insert((n.source_file.clone(), line), n.id.clone());
        }
    }

    let mut collector = RefCollector {
        symbols: &symbols,
        fn_index: &fn_index,
        edges: Vec::new(),
        nodes: Vec::new(),
        // Seed with the ids the extraction already produced so a stub node is
        // only minted for a type nothing else in the graph defines.
        seen: result.nodes.iter().map(|n| n.id.clone()).collect(),
    };

    let mut parser = Parser::new();
    if parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .is_err()
    {
        return;
    }

    for (path, source) in paths_and_source {
        let path_str = path.to_string_lossy().to_string();
        if !path_str.ends_with(".rs") {
            continue;
        }
        let Some(tree) = parser.parse(source, None) else {
            continue;
        };
        collector.walk(tree.root_node(), source, &path_str);
    }

    result.nodes.extend(collector.nodes);
    result.edges.extend(collector.edges);
}

/// Accumulates the nodes and edges one extraction run produces.
///
/// The walk threads five pieces of state through every level; bundling them
/// here keeps each step's signature to the arguments that actually vary
/// (the AST node, its source, and where it came from).
struct RefCollector<'a> {
    /// Type name → id of the node that defines it, for resolving a reference
    /// to a real definition rather than a stub.
    symbols: &'a HashMap<String, String>,
    /// (file, line) → id of the function node the AST pass already emitted.
    fn_index: &'a HashMap<(String, usize), String>,
    edges: Vec<GraphEdge>,
    nodes: Vec<GraphNode>,
    /// Every node id known so far, so stub types are created at most once.
    seen: HashSet<String>,
}

impl RefCollector<'_> {
    /// Recursively find `function_item` definitions and `struct_item` /
    /// `enum_item` / `union_item` type definitions, emitting type-ref edges
    /// for each.
    fn walk(&mut self, node: Node, source: &[u8], path_str: &str) {
        let kind = node.kind();
        if kind == "function_item" {
            let line = node.start_position().row + 1;
            if let Some(func_nid) = self.fn_index.get(&(path_str.to_string(), line)) {
                let func_nid = func_nid.clone();
                self.function_refs(node, source, path_str, &func_nid, line);
            }
        } else if matches!(kind, "struct_item" | "enum_item" | "union_item") {
            // Struct / enum field type refs → `field` context edges.
            let line = node.start_position().row + 1;
            if let Some(name_node) = node.child_by_field_name("name") {
                let type_name = name_node.utf8_text(source).unwrap_or("").to_string();
                let owner_id = self
                    .symbols
                    .get(&type_name)
                    .cloned()
                    .unwrap_or_else(|| make_id(&[path_str, &type_name]));
                self.field_refs(node, source, path_str, &owner_id, line);
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.walk(child, source, path_str);
        }
    }

    /// Walk one function's parameters and return type, emitting `references`
    /// edges with the appropriate `context` string.
    fn function_refs(
        &mut self,
        func_node: Node,
        source: &[u8],
        path_str: &str,
        func_nid: &str,
        line: usize,
    ) {
        if let Some(params) = func_node.child_by_field_name("parameters") {
            let mut cursor = params.walk();
            for param in params.children(&mut cursor) {
                if param.kind() != "parameter" {
                    continue;
                }
                self.type_refs_for(param, source, path_str, func_nid, line, "parameter_type");
            }
        }
        if let Some(return_type) = func_node.child_by_field_name("return_type") {
            self.type_refs_for(return_type, source, path_str, func_nid, line, "return_type");
        }
    }

    /// Walk a struct/enum/union body and emit `references` edges with
    /// `context: field` for every type a field uses.
    fn field_refs(
        &mut self,
        def_node: Node,
        source: &[u8],
        path_str: &str,
        owner_id: &str,
        line: usize,
    ) {
        // tree-sitter-rust exposes the body differently per shape: a named
        // struct has a `field_declaration_list`, a tuple struct an
        // `ordered_field_declaration_list`, an enum an `enum_variant_list`.
        // Iterating direct children covers all three without guessing at a
        // field name.
        let mut cursor = def_node.walk();
        for child in def_node.children(&mut cursor) {
            match child.kind() {
                "field_declaration_list" | "ordered_field_declaration_list" => {
                    let mut inner = child.walk();
                    for field in child.children(&mut inner) {
                        if field.kind() == "field_declaration" {
                            if let Some(ty) = field.child_by_field_name("type") {
                                self.type_refs_for(ty, source, path_str, owner_id, line, "field");
                            }
                        } else if field.kind() == "ordered_field_declaration_list" {
                            // Tuple-struct variant: each child is a bare type.
                            let mut tuple_cursor = field.walk();
                            for ty in field.children(&mut tuple_cursor) {
                                self.type_refs_for(ty, source, path_str, owner_id, line, "field");
                            }
                        }
                    }
                }
                "enum_variant_list" => {
                    let mut variant_cursor = child.walk();
                    for variant in child.children(&mut variant_cursor) {
                        if variant.kind() != "enum_variant" {
                            continue;
                        }
                        if let Some(payload) = variant.child_by_field_name("body") {
                            self.type_refs_for(payload, source, path_str, owner_id, line, "field");
                        } else {
                            let mut vcursor = variant.walk();
                            for c in variant.children(&mut vcursor) {
                                if matches!(
                                    c.kind(),
                                    "field_declaration_list" | "ordered_field_declaration_list"
                                ) {
                                    self.type_refs_for(
                                        c, source, path_str, owner_id, line, "field",
                                    );
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Collect and emit type references for a single type expression node.
    ///
    /// Anything nested inside `<...>` is reported as `generic_arg` regardless
    /// of `default_ctx`, matching Python's context vocabulary.
    fn type_refs_for(
        &mut self,
        node: Node,
        source: &[u8],
        path_str: &str,
        owner_id: &str,
        line: usize,
        default_ctx: &str,
    ) {
        let mut refs = Vec::new();
        collect_type_refs(node, source, false, &mut refs);
        for (name, role) in refs {
            let ctx = if role == "generic_arg" {
                "generic_arg"
            } else {
                default_ctx
            };
            self.reference(owner_id, &name, path_str, line, ctx);
        }
    }

    /// Emit a `references` edge from `owner_id` to whichever node id `name`
    /// resolves to. Creates a stub node for unknown names so external types
    /// (Vec, HashMap, crate names) show up as first-class nodes rather than
    /// dangling edges.
    fn reference(
        &mut self,
        owner_id: &str,
        name: &str,
        path_str: &str,
        line: usize,
        context: &str,
    ) {
        // Primitive types don't get nodes.
        if is_primitive(name) {
            return;
        }
        let target_id = self
            .symbols
            .get(name)
            .cloned()
            .unwrap_or_else(|| make_id(&[name]));
        if self.seen.insert(target_id.clone()) {
            // Stub node for an external type nothing in this graph defines.
            self.nodes.push(GraphNode {
                id: target_id.clone(),
                label: name.to_string(),
                source_file: String::new(),
                source_location: None,
                node_type: NodeType::Class,
                community: None,
                extra: Default::default(),
            });
        }
        let mut extra = HashMap::new();
        extra.insert(
            "context".into(),
            serde_json::Value::String(context.to_string()),
        );
        self.edges.push(GraphEdge {
            source: owner_id.to_string(),
            target: target_id,
            relation: "references".to_string(),
            confidence: Confidence::Extracted,
            confidence_score: 1.0,
            source_file: path_str.to_string(),
            source_location: Some(format!("L{line}")),
            weight: 1.0,
            provenance: Some(format!("type-ref:{context}")),
            extra,
        });
    }
}

/// Walk a Rust type node and collect `(name, role)` pairs.
///
/// `role` is `"type"` for the outer type and `"generic_arg"` for anything
/// nested inside `<...>`. Matches Python's `_rust_collect_type_refs`.
///
/// Free function rather than a method: it reads the AST and reports what it
/// found, touching none of the collector's state.
fn collect_type_refs(node: Node, source: &[u8], generic: bool, out: &mut Vec<(String, String)>) {
    let role = || if generic { "generic_arg" } else { "type" }.to_string();
    let t = node.kind();
    if t == "type_identifier" {
        // Only accept actual type nodes; plain identifiers would drag in
        // variable names (parameter names, expression identifiers), which
        // Python's walker rejects for the same reason.
        let text = node.utf8_text(source).unwrap_or("").to_string();
        if !text.is_empty() {
            out.push((text, role()));
        }
        return;
    }
    if t == "scoped_type_identifier" || t == "scoped_identifier" {
        // `foo::Bar` — take the tail (Bar).
        let text = node.utf8_text(source).unwrap_or("");
        let tail = text.rsplit("::").next().unwrap_or(text).trim().to_string();
        if !tail.is_empty() {
            out.push((tail, role()));
        }
        return;
    }
    if t == "generic_type" {
        // Container name, then recurse into the type arguments.
        if let Some(container_node) = node.child_by_field_name("type") {
            let container_text = container_node.utf8_text(source).unwrap_or("");
            let container = container_text
                .rsplit("::")
                .next()
                .unwrap_or(container_text)
                .trim()
                .to_string();
            if !container.is_empty() {
                out.push((container, role()));
            }
        }
        if let Some(args) = node.child_by_field_name("type_arguments") {
            let mut cursor = args.walk();
            for c in args.children(&mut cursor) {
                if matches!(
                    c.kind(),
                    "type_identifier"
                        | "scoped_type_identifier"
                        | "generic_type"
                        | "reference_type"
                        | "tuple_type"
                        | "array_type"
                        | "slice_type"
                        | "pointer_type"
                        | "primitive_type"
                ) {
                    collect_type_refs(c, source, true, out);
                }
            }
        }
        return;
    }
    // Reference / tuple / array / slice / pointer wrappers: recurse into
    // children, keeping the current generic depth.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_type_refs(child, source, generic, out);
    }
}

fn is_primitive(name: &str) -> bool {
    matches!(
        name,
        "bool"
            | "char"
            | "str"
            | "String"
            | "i8"
            | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
            | "f32"
            | "f64"
            | "()"
    )
}
