//! Terraform / HCL extractor: resources, modules, variables, providers, data sources.

use std::path::Path;
use std::sync::LazyLock;

use super::{line_of, make_edge, make_file_node, make_node};
use graphify_core::confidence::Confidence;
use graphify_core::model::{ExtractionResult, NodeType};
use regex::Regex;

static RE_RESOURCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*resource\s+"([^"]+)"\s+"([^"]+)""#).unwrap()
});
static RE_DATA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?m)^\s*data\s+"([^"]+)"\s+"([^"]+)""#).unwrap()
});
static RE_MODULE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*module\s+"([^"]+)""#).unwrap());
static RE_VARIABLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*variable\s+"([^"]+)""#).unwrap());
static RE_OUTPUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*output\s+"([^"]+)""#).unwrap());
static RE_PROVIDER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?m)^\s*provider\s+"([^"]+)""#).unwrap());

pub(crate) fn extract_hcl(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let push = |name: &str, line: usize, node_type: NodeType, result: &mut ExtractionResult| {
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

    for cap in RE_RESOURCE.captures_iter(source) {
        let name = format!("{}.{}", &cap[1], &cap[2]);
        push(&name, line_of(source, &cap), NodeType::Class, &mut result);
    }
    for cap in RE_DATA.captures_iter(source) {
        let name = format!("data.{}.{}", &cap[1], &cap[2]);
        push(&name, line_of(source, &cap), NodeType::Struct, &mut result);
    }
    for cap in RE_MODULE.captures_iter(source) {
        push(&cap[1], line_of(source, &cap), NodeType::Module, &mut result);
    }
    for cap in RE_VARIABLE.captures_iter(source) {
        push(&cap[1], line_of(source, &cap), NodeType::Variable, &mut result);
    }
    for cap in RE_OUTPUT.captures_iter(source) {
        push(&cap[1], line_of(source, &cap), NodeType::Constant, &mut result);
    }
    for cap in RE_PROVIDER.captures_iter(source) {
        push(&cap[1], line_of(source, &cap), NodeType::Package, &mut result);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_terraform_blocks() {
        let src = r#"
provider "aws" { region = "us-east-1" }
resource "aws_s3_bucket" "logs" { bucket = "logs" }
module "network" { source = "./network" }
variable "env" { type = string }
output "bucket_id" { value = aws_s3_bucket.logs.id }
"#;
        let r = extract_hcl(Path::new("main.tf"), src);
        assert!(r.nodes.iter().any(|n| n.label == "aws_s3_bucket.logs"));
        assert!(r.nodes.iter().any(|n| n.label == "network"));
        assert!(r.nodes.iter().any(|n| n.label == "env"));
        assert!(r.nodes.iter().any(|n| n.label == "bucket_id"));
        assert!(r.nodes.iter().any(|n| n.label == "aws"));
    }
}
