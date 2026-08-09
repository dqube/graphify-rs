//! CUDA extractor: kernels (`__global__`/`__device__`/`__host__`), structs, includes.
//!
//! Extends the C/C++ regex approach with CUDA execution-space qualifiers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::LazyLock;

use super::{
    RE_C_FUNC, RE_C_INCLUDE, RE_CPP_CLASS, RE_C_STRUCT, end_line_at, infer_calls, line_of,
    make_edge, make_file_node, make_node, path_str,
};
use graphify_core::confidence::Confidence;
use graphify_core::id::make_id;
use graphify_core::model::{ExtractionResult, GraphNode, NodeType};
use regex::Regex;

static RE_KERNEL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)__(?:global|device|host)__\s+[\w:<>\s*&]+?(\w+)\s*\([^;]*\)\s*\{").unwrap()
});

pub(crate) fn extract_cuda(path: &Path, source: &str) -> ExtractionResult {
    let mut result = ExtractionResult::default();
    let file_node = make_file_node(path);
    let file_id = file_node.id.clone();
    result.nodes.push(file_node);

    let lines: Vec<&str> = source.lines().collect();
    let ps = path_str(path);

    for re in [&*RE_CPP_CLASS, &*RE_C_STRUCT] {
        for cap in re.captures_iter(source) {
            let name = &cap[1];
            let line = line_of(source, &cap);
            let node = make_node(name, path, NodeType::Struct, line);
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
    }

    let mut functions: Vec<(String, String, usize, usize)> = Vec::new();
    for re in [&*RE_KERNEL, &*RE_C_FUNC] {
        let matches: Vec<_> = re.captures_iter(source).collect();
        for (i, cap) in matches.iter().enumerate() {
            let name = cap[1].to_string();
            if name == "if" || name == "while" || name == "for" || name == "switch" {
                continue;
            }
            let start_line = line_of(source, cap);
            let end_line = end_line_at(source, matches.get(i + 1));
            let node = make_node(&name, path, NodeType::Function, start_line);
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
    }

    for cap in RE_C_INCLUDE.captures_iter(source) {
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

    result.edges.extend(infer_calls(&functions, &lines, path));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_cuda_kernels() {
        let src = "#include <cuda_runtime.h>\n\
                   struct Vec3 { float x, y, z; };\n\
                   __global__ void saxpy(float a, float* x, float* y) {\n  scale(a, x);\n}\n\
                   __device__ float scale(float a, float* x) {\n  return a * x[0];\n}\n";
        let r = extract_cuda(Path::new("kernel.cu"), src);
        assert!(r.nodes.iter().any(|n| n.label == "saxpy"));
        assert!(r.nodes.iter().any(|n| n.label == "scale"));
        assert!(r.nodes.iter().any(|n| n.label == "Vec3"));
        assert!(r.nodes.iter().any(|n| n.label == "cuda_runtime.h"));
    }
}
