//! Laravel Blade template extractor.
//!
//! Blade templates (`*.blade.php`) form a graph through the following
//! directives:
//!
//! - `@extends('layouts.app')` — template inheritance
//! - `@include('partials.header')`, `@includeIf`, `@includeWhen`,
//!   `@includeUnless`, `@includeFirst([...])` — partial inclusion
//! - `@component('alerts.info')`, `@each('view', $items, 'item')` — reuse
//! - `@section('name') … @endsection` and `@yield('name')` — named blocks
//!
//! Referenced templates use their dot-notation name (`layouts.app`) as a
//! stable identity so different files pointing at the same template converge
//! on one node. Section nodes are scoped to the file that defines them.

use std::path::Path;
use std::sync::LazyLock;

use super::{make_edge, make_file_node, make_node};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use regex::Regex;

/// `@extends('name')` — template inheritance.
static RE_EXTENDS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@extends\s*\(\s*['"]([^'"]+)['"]"#).unwrap());
/// `@include('name')` and its conditional variants. Each match's first
/// capture is the template name.
static RE_INCLUDE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"@include(?:If|When|Unless)?\s*\(\s*(?:[^,'"]+?,\s*)?['"]([^'"]+)['"]"#).unwrap()
});
/// `@includeFirst(['a','b',...])` — the fallback list. Only the first named
/// template is captured; that is the primary reference for graph purposes.
static RE_INCLUDE_FIRST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@includeFirst\s*\(\s*\[\s*['"]([^'"]+)['"]"#).unwrap());
/// `@component('name')` — Blade component reuse.
static RE_COMPONENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@component\s*\(\s*['"]([^'"]+)['"]"#).unwrap());
/// `@each('view.name', $items, 'item')` — repeated include.
static RE_EACH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@each\s*\(\s*['"]([^'"]+)['"]"#).unwrap());
/// `@section('name')` — named block definition.
static RE_SECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@section\s*\(\s*['"]([^'"]+)['"]"#).unwrap());
/// `@yield('name')` — placeholder for a section defined by a parent.
static RE_YIELD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@yield\s*\(\s*['"]([^'"]+)['"]"#).unwrap());

/// Stable, cross-file node id for a referenced template.
///
/// Blade names use dot notation (`layouts.app`) that maps to a file path
/// (`resources/views/layouts/app.blade.php`), but the extractor works one
/// file at a time and does not know the project root. Emitting one shared
/// node per template name lets every reference converge here; later
/// cross-file resolution can rewrite these to the actual file nodes.
fn template_node(name: &str, path: &Path, line: usize) -> GraphNode {
    GraphNode {
        id: make_id(&["blade_template", name]),
        label: name.to_string(),
        source_file: path.to_string_lossy().into_owned(),
        source_location: Some(format!("L{line}")),
        node_type: NodeType::Module,
        community: None,
        extra: std::collections::HashMap::new(),
    }
}

fn line_of(source: &str, mat_start: usize) -> usize {
    source[..mat_start].lines().count() + 1
}

pub(crate) fn extract_blade(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let emit_ref = |cap: regex::Captures<'_>, relation: &str, result: &mut ExtractionResult| {
        let name = &cap[1];
        let line = line_of(source, cap.get(0).unwrap().start());
        let node = template_node(name, path, line);
        let node_id = node.id.clone();
        result.nodes.push(node);
        result.edges.push(make_edge(
            &file_id,
            &node_id,
            relation,
            path,
            Confidence::Extracted,
        ));
    };

    for cap in RE_EXTENDS.captures_iter(source) {
        emit_ref(cap, "extends", &mut result);
    }
    for cap in RE_INCLUDE.captures_iter(source) {
        emit_ref(cap, "includes", &mut result);
    }
    for cap in RE_INCLUDE_FIRST.captures_iter(source) {
        emit_ref(cap, "includes", &mut result);
    }
    for cap in RE_EACH.captures_iter(source) {
        emit_ref(cap, "includes", &mut result);
    }
    for cap in RE_COMPONENT.captures_iter(source) {
        emit_ref(cap, "uses", &mut result);
    }

    // Sections defined in this file.
    for cap in RE_SECTION.captures_iter(source) {
        let name = &cap[1];
        let line = line_of(source, cap.get(0).unwrap().start());
        let node = make_node(name, path, NodeType::Concept, line);
        let node_id = node.id.clone();
        result.nodes.push(node);
        result.edges.push(make_edge(
            &file_id,
            &node_id,
            "defines_section",
            path,
            Confidence::Extracted,
        ));
    }

    // `@yield('name')` records the contract this template exposes to
    // children; useful as a `yields` edge to the same section-name concept
    // node so parent/child templates converge on the same slot.
    for cap in RE_YIELD.captures_iter(source) {
        let name = &cap[1];
        let line = line_of(source, cap.get(0).unwrap().start());
        let node = make_node(name, path, NodeType::Concept, line);
        let node_id = node.id.clone();
        result.nodes.push(node);
        result.edges.push(make_edge(
            &file_id,
            &node_id,
            "yields",
            path,
            Confidence::Extracted,
        ));
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_extends_include_and_component() {
        let src = "@extends('layouts.app')\n@section('body')\n  @include('partials.nav')\n  @component('alerts.info')\n    hi\n  @endcomponent\n@endsection\n";
        let r = extract_blade(Path::new("home.blade.php"), src);
        assert!(r.edges.iter().any(|e| e.relation == "extends"));
        assert!(r.edges.iter().any(|e| e.relation == "includes"));
        assert!(r.edges.iter().any(|e| e.relation == "uses"));
        assert!(r.edges.iter().any(|e| e.relation == "defines_section"));
    }

    #[test]
    fn referenced_templates_share_id_across_files() {
        let a = extract_blade(Path::new("a.blade.php"), "@extends('layouts.app')\n");
        let b = extract_blade(Path::new("b.blade.php"), "@extends('layouts.app')\n");
        let a_target = &a
            .edges
            .iter()
            .find(|e| e.relation == "extends")
            .expect("a should extend")
            .target;
        let b_target = &b
            .edges
            .iter()
            .find(|e| e.relation == "extends")
            .expect("b should extend")
            .target;
        assert_eq!(a_target, b_target, "shared template id across files");
    }

    #[test]
    fn include_variants_and_each() {
        let src = r#"
@includeIf('partials.hero')
@includeWhen($cond, 'partials.debug')
@includeUnless($hide, 'partials.notice')
@includeFirst(['themes.custom', 'themes.default'])
@each('items.card', $items, 'item')
"#;
        let r = extract_blade(Path::new("t.blade.php"), src);
        let include_labels: Vec<_> = r
            .nodes
            .iter()
            .filter(|n| n.node_type == NodeType::Module)
            .map(|n| n.label.as_str())
            .collect();
        for expected in [
            "partials.hero",
            "partials.debug",
            "partials.notice",
            "themes.custom",
            "items.card",
        ] {
            assert!(
                include_labels.contains(&expected),
                "expected include for {expected}, got {include_labels:?}"
            );
        }
    }

    #[test]
    fn yield_matches_section_name() {
        let parent = extract_blade(Path::new("layout.blade.php"), "@yield('body')\n");
        let child = extract_blade(
            Path::new("home.blade.php"),
            "@extends('layout')\n@section('body')\nhi\n@endsection\n",
        );
        let yield_target = &parent
            .edges
            .iter()
            .find(|e| e.relation == "yields")
            .expect("yield edge")
            .target;
        let section_target = &child
            .edges
            .iter()
            .find(|e| e.relation == "defines_section")
            .expect("section edge")
            .target;
        // Yield and section target IDs are file-scoped, so they legitimately
        // differ — cross-template linking is a later resolution pass. The
        // labels, however, are the same "body" slot name.
        let yield_label = &parent
            .nodes
            .iter()
            .find(|n| n.id == *yield_target)
            .unwrap()
            .label;
        let section_label = &child
            .nodes
            .iter()
            .find(|n| n.id == *section_target)
            .unwrap()
            .label;
        assert_eq!(yield_label, "body");
        assert_eq!(section_label, "body");
    }
}
