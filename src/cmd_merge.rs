//! Merge commands: combine graphs and resolve graph.json in git merges.
//!
//! The resolution rules live in [`graphify_build::merge`]; this module only deals
//! with the I/O around them — reading the files, enforcing size caps before
//! anything is parsed, and writing the result where each caller expects it
//! (an explicit `--output` for `merge-graphs`, the `ours` path for git).

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use graphify_build::merge::{MergeInput, distinct_repo_tags, merge_graphs, three_way_merge};
use graphify_core::graph::KnowledgeGraph;

/// Reject a `graph.json` larger than this before parsing it.
///
/// A corrupt or hostile file must not be able to exhaust memory just by being
/// handed to `serde_json`. Real graphs are well under 5 MB, so 50 MB is far above
/// anything legitimate; anything bigger should fail loudly and get a human's
/// attention rather than take the process down.
const MAX_GRAPH_BYTES: u64 = 50 * 1024 * 1024;

/// Reject a merged graph larger than this. Same reasoning, applied to the result:
/// a merge that produces 100k+ nodes is a sign the inputs were not what the user
/// thought they were.
const MAX_MERGED_NODES: usize = 100_000;

/// Merge two or more graph.json files into one.
///
/// Each input is namespaced under a tag derived from its repo directory (the
/// directory above `graphify-out/`), so a `src/main` node from two different
/// repos stays two nodes instead of silently fusing into one and inventing edges
/// between unrelated codebases. The original id survives as `local_id` and the
/// tag as `repo` on every node.
pub fn cmd_merge_graphs(inputs: &[String], output: &str) -> Result<()> {
    if inputs.len() < 2 {
        bail!(
            "merge-graphs needs at least two input graphs (got {})",
            inputs.len()
        );
    }

    let paths: Vec<PathBuf> = inputs.iter().map(PathBuf::from).collect();
    for path in &paths {
        check_size_cap(path)?;
    }

    let tags = distinct_repo_tags(&paths);
    let naive: Vec<&str> = tags.iter().map(String::as_str).collect();
    let graphs: Vec<KnowledgeGraph> = paths
        .iter()
        .map(|p| load_graph(p))
        .collect::<Result<Vec<_>>>()?;

    let merge_inputs: Vec<MergeInput<'_>> = graphs
        .iter()
        .zip(tags.iter())
        .map(|(g, tag)| MergeInput::tagged(tag.clone(), g))
        .collect();

    let (merged, stats) = merge_graphs(&merge_inputs).context("merging graphs")?;
    if stats.nodes > MAX_MERGED_NODES {
        bail!(
            "merged graph has {} nodes, exceeding the {MAX_MERGED_NODES}-node cap",
            stats.nodes
        );
    }

    write_graph(&merged, Path::new(output))?;

    println!(
        "Merged {} graphs -> {} nodes, {} edges",
        stats.inputs, stats.nodes, stats.edges
    );
    println!("Namespaces: {}", naive.join(", "));
    if stats.dangling_dropped > 0 {
        println!(
            "  dropped {} edge(s)/hyperedge(s) with missing endpoints",
            stats.dangling_dropped
        );
    }
    println!("Written to: {output}");
    Ok(())
}

/// Git merge driver: reconcile `ours` and `theirs` against `base`, writing the
/// result over `ours` as git expects.
///
/// `graph.json` is generated, never hand-edited, so a textual conflict in it is
/// noise — the union of both sides is always a more useful answer than conflict
/// markers, and a regeneration (`graphify-rs update .`) is the real fix. This
/// exits non-zero only when it genuinely cannot produce a result: an unreadable
/// or oversized `ours`/`theirs`, or a merged graph past the node cap. A missing
/// or unparseable `base` is not fatal — git hands over an empty file for an
/// add/add conflict, which correctly means "everything on both sides is new".
///
/// # Setup
///
/// Register the driver once per machine (it names a command, so git will not read
/// it from a committed file):
///
/// ```text
/// git config merge.graphify.name "graphify graph.json union merge"
/// git config merge.graphify.driver "graphify-rs merge-driver %O %A %B"
/// ```
///
/// Then point graph.json at it from a committed `.gitattributes`:
///
/// ```text
/// graphify-out/graph.json merge=graphify
/// ```
pub fn cmd_merge_driver(base: &str, ours: &str, theirs: &str) -> Result<()> {
    let ours_path = Path::new(ours);
    check_size_cap(ours_path)?;
    check_size_cap(Path::new(theirs))?;

    let ours_graph = load_graph(ours_path)?;
    let theirs_graph = load_graph(Path::new(theirs))?;
    let base_graph = load_base(Path::new(base));

    let (merged, stats) =
        three_way_merge(&base_graph, &ours_graph, &theirs_graph).context("merging graph.json")?;

    if stats.nodes > MAX_MERGED_NODES {
        bail!(
            "merged graph has {} nodes, exceeding the {MAX_MERGED_NODES}-node cap; \
             aborting so git reports a conflict",
            stats.nodes
        );
    }

    // Git reads the result back from the `ours` path — writing anywhere else
    // silently discards the merge.
    write_graph(&merged, ours_path)?;

    // stdout belongs to git during a merge, so status goes to stderr.
    eprintln!(
        "[graphify merge-driver] merged graph.json: {} nodes, {} edges \
         ({} conflict(s) resolved to ours, {} deletion(s) honored)",
        stats.nodes, stats.edges, stats.node_conflicts, stats.deletions_honored
    );
    Ok(())
}

/// Fail before parsing if a graph file is implausibly large.
fn check_size_cap(path: &Path) -> Result<()> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("cannot stat graph file {}", path.display()))?;
    if meta.len() > MAX_GRAPH_BYTES {
        bail!(
            "{} is {} bytes, exceeding the {MAX_GRAPH_BYTES}-byte cap",
            path.display(),
            meta.len()
        );
    }
    Ok(())
}

fn load_graph(path: &Path) -> Result<KnowledgeGraph> {
    graphify_serve::load_graph(path)
        .with_context(|| format!("failed to load graph from {}", path.display()))
}

/// Load the merge base, tolerating its absence.
///
/// Git always passes a `%O` path, but for an add/add conflict the file is empty
/// and there is no common ancestor. An empty graph is the right model of that:
/// nothing existed before, so nothing can have been deleted and the merge reduces
/// to a union.
fn load_base(path: &Path) -> KnowledgeGraph {
    match graphify_serve::load_graph(path) {
        Ok(graph) => graph,
        Err(e) => {
            eprintln!(
                "[graphify merge-driver] no usable merge base at {} ({e}); \
                 treating both sides as additions",
                path.display()
            );
            KnowledgeGraph::new()
        }
    }
}

/// Write a graph as node-link JSON, creating the parent directory if needed.
fn write_graph(graph: &KnowledgeGraph, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let file = File::create(path).with_context(|| format!("cannot write to {}", path.display()))?;
    graph
        .write_node_link_json(BufWriter::new(file))
        .with_context(|| format!("failed to serialize graph to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Minimal node-link graph with the given node ids and edges.
    fn graph_json(nodes: &[&str], edges: &[(&str, &str)]) -> String {
        let nodes: Vec<String> = nodes
            .iter()
            .map(|id| {
                format!(
                    r#"{{"id":"{id}","label":"{id}","source_file":"t.rs","node_type":"class"}}"#
                )
            })
            .collect();
        let links: Vec<String> = edges
            .iter()
            .map(|(s, t)| {
                format!(
                    r#"{{"source":"{s}","target":"{t}","relation":"calls",
                        "confidence":"EXTRACTED","source_file":"t.rs"}}"#
                )
            })
            .collect();
        format!(
            r#"{{"directed":false,"multigraph":false,"graph":{{}},
                "nodes":[{}],"links":[{}]}}"#,
            nodes.join(","),
            links.join(",")
        )
    }

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    fn node_ids(path: &Path) -> Vec<String> {
        let graph = graphify_serve::load_graph(path).unwrap();
        let mut ids = graph.node_ids();
        ids.sort();
        ids
    }

    #[test]
    fn merge_graphs_namespaces_and_writes_output() {
        let tmp = tempfile::tempdir().unwrap();
        let a_dir = tmp.path().join("api/graphify-out");
        let b_dir = tmp.path().join("web/graphify-out");
        std::fs::create_dir_all(&a_dir).unwrap();
        std::fs::create_dir_all(&b_dir).unwrap();

        let a = write(
            &a_dir,
            "graph.json",
            &graph_json(&["main", "x"], &[("main", "x")]),
        );
        let b = write(
            &b_dir,
            "graph.json",
            &graph_json(&["main", "y"], &[("main", "y")]),
        );
        let out = tmp.path().join("nested/merged.json");

        cmd_merge_graphs(
            &[
                a.to_string_lossy().into_owned(),
                b.to_string_lossy().into_owned(),
            ],
            &out.to_string_lossy(),
        )
        .unwrap();

        assert_eq!(
            node_ids(&out),
            vec!["api::main", "api::x", "web::main", "web::y"]
        );
    }

    #[test]
    fn merge_graphs_rejects_a_single_input() {
        let err = cmd_merge_graphs(&["only.json".into()], "out.json").unwrap_err();
        assert!(err.to_string().contains("at least two"));
    }

    #[test]
    fn merge_graphs_reports_a_missing_input() {
        let tmp = tempfile::tempdir().unwrap();
        let present = write(tmp.path(), "a.json", &graph_json(&["a"], &[]));
        let err = cmd_merge_graphs(
            &[
                present.to_string_lossy().into_owned(),
                tmp.path().join("nope.json").to_string_lossy().into_owned(),
            ],
            &tmp.path().join("m.json").to_string_lossy(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("cannot stat"));
    }

    #[test]
    fn merge_driver_unions_both_sides_into_ours() {
        let tmp = tempfile::tempdir().unwrap();
        let base = write(tmp.path(), "base.json", &graph_json(&["shared"], &[]));
        let ours = write(
            tmp.path(),
            "ours.json",
            &graph_json(&["shared", "mine"], &[("shared", "mine")]),
        );
        let theirs = write(
            tmp.path(),
            "theirs.json",
            &graph_json(&["shared", "yours"], &[("shared", "yours")]),
        );

        cmd_merge_driver(
            &base.to_string_lossy(),
            &ours.to_string_lossy(),
            &theirs.to_string_lossy(),
        )
        .unwrap();

        // Git reads the result back from the `ours` path.
        assert_eq!(node_ids(&ours), vec!["mine", "shared", "yours"]);
        let merged = graphify_serve::load_graph(&ours).unwrap();
        assert_eq!(merged.edge_count(), 2);
        // `theirs` is left untouched.
        assert_eq!(node_ids(&theirs), vec!["shared", "yours"]);
    }

    #[test]
    fn merge_driver_survives_an_empty_base() {
        let tmp = tempfile::tempdir().unwrap();
        let base = write(tmp.path(), "base.json", "");
        let ours = write(tmp.path(), "ours.json", &graph_json(&["a"], &[]));
        let theirs = write(tmp.path(), "theirs.json", &graph_json(&["b"], &[]));

        cmd_merge_driver(
            &base.to_string_lossy(),
            &ours.to_string_lossy(),
            &theirs.to_string_lossy(),
        )
        .unwrap();

        assert_eq!(node_ids(&ours), vec!["a", "b"]);
    }

    #[test]
    fn merge_driver_fails_on_a_corrupt_side() {
        let tmp = tempfile::tempdir().unwrap();
        let base = write(tmp.path(), "base.json", &graph_json(&[], &[]));
        let ours = write(tmp.path(), "ours.json", "{not json");
        let theirs = write(tmp.path(), "theirs.json", &graph_json(&["b"], &[]));

        // Non-zero exit is how the driver tells git to report a conflict.
        let err = cmd_merge_driver(
            &base.to_string_lossy(),
            &ours.to_string_lossy(),
            &theirs.to_string_lossy(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to load graph"));
    }
}
