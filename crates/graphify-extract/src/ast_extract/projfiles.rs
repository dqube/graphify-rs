//! .NET project-file extractor: solutions, project references, XAML classes.
//!
//! Handles `.sln`, `.csproj`, `.fsproj`, `.vbproj` (metadata extraction) and
//! `.xaml`, `.razor`, `.cshtml` (lightweight markup with code-behind links).

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use super::{line_of, make_edge, make_file_node, path_str};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use regex::Regex;

static RE_SLN_PROJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^Project\("\{[^}]+\}"\)\s*=\s*"([^"]+)",\s*"([^"]+)""#).unwrap()
});
static RE_PACKAGE_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<PackageReference\s+(?:Include|Update)="([^"]+)""#).unwrap());
static RE_PROJECT_REF: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<ProjectReference\s+Include="([^"]+)""#).unwrap());
static RE_XAML_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"x:Class="([^"]+)""#).unwrap());
static RE_INHERITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)@inherits\s+([\w.]+)").unwrap());

pub(crate) fn extract_dotnet_proj(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);
    let ps = path_str(path);

    let push_ref = |label: &str,
                    line: usize,
                    relation: &str,
                    node_type: NodeType,
                    result: &mut ExtractionResult| {
        let ref_id = make_id(&[&ps, relation, label]);
        result.nodes.push(GraphNode {
            id: ref_id.clone(),
            label: label.to_string(),
            source_file: ps.clone(),
            source_location: Some(format!("L{line}")),
            node_type,
            community: None,
            extra: HashMap::new(),
        });
        result.edges.push(make_edge(
            &file_id,
            &ref_id,
            relation,
            path,
            Confidence::Extracted,
        ));
    };

    // Solution files: each Project(...) entry becomes a module node.
    for cap in RE_SLN_PROJECT.captures_iter(source) {
        push_ref(
            &cap[1],
            line_of(source, &cap),
            "defines",
            NodeType::Module,
            &mut result,
        );
    }

    // MSBuild project files: package and project references become imports.
    for cap in RE_PACKAGE_REF.captures_iter(source) {
        push_ref(
            &cap[1],
            line_of(source, &cap),
            "imports",
            NodeType::Package,
            &mut result,
        );
    }
    for cap in RE_PROJECT_REF.captures_iter(source) {
        push_ref(
            &cap[1],
            line_of(source, &cap),
            "imports",
            NodeType::Package,
            &mut result,
        );
    }

    // XAML / Razor markup: link to code-behind class when declared.
    for cap in RE_XAML_CLASS.captures_iter(source) {
        push_ref(
            &cap[1],
            line_of(source, &cap),
            "defines",
            NodeType::Class,
            &mut result,
        );
    }
    for cap in RE_INHERITS.captures_iter(source) {
        push_ref(
            &cap[1],
            line_of(source, &cap),
            "inherits",
            NodeType::Class,
            &mut result,
        );
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_csproj_references() {
        let src = "<Project Sdk=\"Microsoft.NET.Sdk\">\n<ItemGroup>\n\
                   <PackageReference Include=\"Newtonsoft.Json\" Version=\"13.0.1\" />\n\
                   <ProjectReference Include=\"..\\Core\\Core.csproj\" />\n\
                   </ItemGroup>\n</Project>\n";
        let r = extract_dotnet_proj(Path::new("App.csproj"), src);
        assert!(r.nodes.iter().any(|n| n.label == "Newtonsoft.Json"));
        assert!(r.nodes.iter().any(|n| n.label == "..\\Core\\Core.csproj"));
        assert!(r.edges.iter().all(|e| e.relation == "imports"));
    }

    #[test]
    fn extracts_sln_projects() {
        let src =
            "Project(\"{FAE04EC0}\") = \"Core\", \"Core\\Core.csproj\", \"{GUID}\"\nEndProject\n";
        let r = extract_dotnet_proj(Path::new("App.sln"), src);
        assert!(r.nodes.iter().any(|n| n.label == "Core"));
    }

    #[test]
    fn extracts_xaml_class() {
        let src = "<Window x:Class=\"MyApp.MainWindow\" xmlns=\"...\">\n</Window>\n";
        let r = extract_dotnet_proj(Path::new("MainWindow.xaml"), src);
        assert!(r.nodes.iter().any(|n| n.label == "MyApp.MainWindow"));
    }
}
