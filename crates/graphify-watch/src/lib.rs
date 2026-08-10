//! File watching and auto-rebuild for graphify.
//!
//! Uses `notify` + debouncing to watch for file changes and trigger
//! incremental graph rebuilds. Port of Python `watch.py`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use notify::RecursiveMode;
use notify_debouncer_mini::new_debouncer;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Debounce duration before triggering a rebuild.
const DEBOUNCE_DURATION: Duration = Duration::from_secs(3);

/// Default ignore patterns for files that should not trigger rebuilds.
const IGNORE_PATTERNS: &[&str] = &[
    ".git",
    "node_modules",
    "__pycache__",
    ".pyc",
    "target",
    "graphify-rs-out",
    ".DS_Store",
];

/// Errors from the watcher.
#[derive(Debug, Error)]
pub enum WatchError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("notify error: {0}")]
    Notify(#[from] notify::Error),

    #[error("watch setup failed: {0}")]
    Setup(String),

    #[error("rebuild failed: {0}")]
    Rebuild(String),
}

/// Check if a path should be ignored based on common patterns.
fn should_ignore(path: &Path) -> bool {
    let path_str = path.to_string_lossy();
    IGNORE_PATTERNS.iter().any(|p| path_str.contains(p))
}

/// Filter changed paths to only include relevant source files.
fn filter_changes(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|p| !should_ignore(p))
        .cloned()
        .collect()
}

/// How a rebuild should treat the graph already on disk.
#[derive(Debug, Clone, Copy, Default)]
pub struct RebuildOptions {
    /// Overwrite `graph.json` even when the rebuild produced fewer nodes.
    ///
    /// The shrink guard exists because a rebuild that loses nodes is far more
    /// often a broken extraction than a real deletion. After a refactor that
    /// legitimately removes code, this is how you say so.
    pub force: bool,
    /// Skip community detection and leave nodes unassigned.
    pub no_cluster: bool,
}

/// What a rebuild produced, so the caller can report it.
#[derive(Debug, Clone, Copy, Default)]
pub struct RebuildOutcome {
    pub nodes: usize,
    pub edges: usize,
    pub communities: usize,
    pub cache_hits: usize,
    pub extracted: usize,
    pub errors: usize,
    /// Communities that kept a name from the previous graph.
    pub labels_preserved: usize,
}

/// Refuse to overwrite an existing graph with a smaller one.
///
/// Ported from Python's `to_json` guard. The policy is deliberately strict —
/// *any* net node loss stops the write, not some percentage — because the
/// failure it prevents is silent: a parser regression or a half-finished
/// extraction quietly replaces a good graph with a worse one, and nothing
/// downstream can tell the difference afterwards.
///
/// Note the unreadable case fails *closed*. If the existing graph cannot be
/// parsed we cannot prove the new one is not a shrink, and overwriting on a
/// transient read failure is exactly the data loss this guards against.
fn check_not_shrinking(graph_path: &Path, new_nodes: usize) -> Result<(), WatchError> {
    let Ok(raw) = std::fs::read_to_string(graph_path) else {
        // No existing graph (or it cannot be opened at all) — nothing to lose.
        return Ok(());
    };
    if raw.trim().is_empty() {
        return Ok(());
    }

    let existing_nodes = match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => v
            .get("nodes")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len),
        Err(e) => {
            return Err(WatchError::Rebuild(format!(
                "existing {} could not be read to verify the new graph is not smaller ({e}); \
                 refusing to overwrite — pass --force to override",
                graph_path.display()
            )));
        }
    };

    if new_nodes < existing_nodes {
        return Err(WatchError::Rebuild(format!(
            "new graph has {new_nodes} nodes but {} has {existing_nodes} (net -{}); \
             refusing to overwrite. This is usually a failed extraction rather than \
             deleted code — re-run a full build to be safe, or pass --force if you \
             have verified the reduction is real",
            graph_path.display(),
            existing_nodes - new_nodes
        )));
    }
    Ok(())
}

/// Recover each node's previous community from the graph already on disk.
///
/// Returns an empty map when there is no readable previous graph, which makes
/// the first rebuild fall through to plain numbering.
fn previous_node_communities(graph_path: &Path) -> HashMap<String, usize> {
    let mut out = HashMap::new();
    let Ok(raw) = std::fs::read_to_string(graph_path) else {
        return out;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return out;
    };
    let Some(nodes) = value.get("nodes").and_then(serde_json::Value::as_array) else {
        return out;
    };
    for node in nodes {
        if let (Some(id), Some(cid)) = (
            node.get("id").and_then(serde_json::Value::as_str),
            node.get("community").and_then(serde_json::Value::as_u64),
        ) {
            out.insert(id.to_string(), cid as usize);
        }
    }
    out
}

/// Recover community names written by `label` from the graph already on disk.
fn previous_community_labels(graph_path: &Path) -> HashMap<usize, String> {
    let mut out = HashMap::new();
    let Ok(raw) = std::fs::read_to_string(graph_path) else {
        return out;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return out;
    };
    let Some(nodes) = value.get("nodes").and_then(serde_json::Value::as_array) else {
        return out;
    };
    for node in nodes {
        if let (Some(cid), Some(name)) = (
            node.get("community").and_then(serde_json::Value::as_u64),
            node.get(graphify_cluster::label::NODE_LABEL_FIELD)
                .and_then(serde_json::Value::as_str),
        ) {
            out.entry(cid as usize).or_insert_with(|| name.to_string());
        }
    }
    out
}

/// Run the full pipeline: detect -> extract -> build -> cluster -> analyze -> export.
///
/// When `changed_files` is provided, only those files have their cache invalidated
/// before extraction, achieving an incremental rebuild without re-parsing unchanged files.
///
/// Community ids are renumbered to line up with the previous graph, and names
/// written by `label` are carried across, so a rebuild does not silently
/// reshuffle every community and strand its LLM-generated names.
pub fn rebuild_code(
    root: &Path,
    output_dir: &Path,
    changed_files: Option<&[PathBuf]>,
    options: &RebuildOptions,
) -> Result<RebuildOutcome, WatchError> {
    let cache_dir = output_dir.join("cache");

    if let Some(changed) = changed_files {
        for path in changed {
            let _ = graphify_cache::invalidate_cached(path, root, &cache_dir);
        }
        info!(
            "rebuild: invalidated cache for {} changed file(s)",
            changed.len()
        );
    }

    info!("rebuild: detecting files...");
    let detection = graphify_detect::detect(root);
    info!(
        "rebuild: found {} files (~{} words)",
        detection.total_files, detection.total_words
    );

    let code_files: Vec<PathBuf> = detection
        .files
        .get(&graphify_detect::FileType::Code)
        .map(|v| v.iter().map(|f| root.join(f)).collect())
        .unwrap_or_default();

    if code_files.is_empty() {
        info!("rebuild: no code files found, skipping");
        return Ok(RebuildOutcome::default());
    }

    info!(
        "rebuild: extracting AST from {} code files...",
        code_files.len()
    );
    let mut ast_result = graphify_core::model::ExtractionResult::default();
    let mut cache_hits = 0usize;
    let mut errors = 0usize;
    for file_path in &code_files {
        if let Some(cached) = graphify_cache::load_cached_from::<
            graphify_core::model::ExtractionResult,
        >(file_path, root, &cache_dir)
        {
            cache_hits += 1;
            ast_result.nodes.extend(cached.nodes);
            ast_result.edges.extend(cached.edges);
            ast_result.hyperedges.extend(cached.hyperedges);
            continue;
        }
        if let Ok(fresh) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            graphify_extract::extract(std::slice::from_ref(file_path))
        })) {
            let _ = graphify_cache::save_cached_to(file_path, &fresh, root, &cache_dir);
            ast_result.nodes.extend(fresh.nodes);
            ast_result.edges.extend(fresh.edges);
            ast_result.hyperedges.extend(fresh.hyperedges);
        } else {
            errors += 1;
            warn!("rebuild: extraction panicked for {}", file_path.display());
        }
    }
    if cache_hits > 0 {
        info!(
            "rebuild: cache {} hits, {} extracted fresh",
            cache_hits,
            code_files.len() - cache_hits
        );
    }
    if errors > 0 {
        warn!("rebuild: {} file(s) had extraction errors", errors);
    }
    info!(
        "rebuild: Pass 1 (AST): {} nodes, {} edges",
        ast_result.nodes.len(),
        ast_result.edges.len()
    );

    let extractions = vec![ast_result];

    info!("rebuild: building graph...");
    let mut graph = graphify_build::build(&extractions)
        .map_err(|e| WatchError::Rebuild(format!("build failed: {e}")))?;
    info!(
        "rebuild: graph has {} nodes, {} edges",
        graph.node_count(),
        graph.edge_count()
    );

    // Checked before anything is written, so a rejected rebuild leaves every
    // output file exactly as it was.
    let graph_path = output_dir.join("graph.json");
    if !options.force {
        check_not_shrinking(&graph_path, graph.node_count())?;
    }

    let mut labels_preserved = 0usize;
    let (communities, community_labels) = if options.no_cluster {
        info!("rebuild: skipping community detection (--no-cluster)");
        (HashMap::new(), HashMap::new())
    } else {
        info!("rebuild: detecting communities...");
        let fresh = graphify_cluster::cluster(&graph);

        // Line the new numbering up with the old before anything keyed by
        // community id is derived from it.
        let previous = previous_node_communities(&graph_path);
        let communities = if previous.is_empty() {
            fresh
        } else {
            graphify_cluster::remap_communities_to_previous(&fresh, &previous)
        };

        // Stamp the assignment onto the nodes: `graph.json` carries community
        // membership per node, and without this the rebuild would write a graph
        // with every node unassigned.
        for (&cid, nodes) in &communities {
            for nid in nodes {
                if let Some(node) = graph.get_node_mut(nid) {
                    node.community = Some(cid);
                }
            }
        }

        let carried = previous_community_labels(&graph_path);
        let community_labels: HashMap<usize, String> = communities
            .iter()
            .map(|(cid, nodes)| {
                if let Some(name) = carried.get(cid) {
                    labels_preserved += 1;
                    return (*cid, name.clone());
                }
                let label = nodes
                    .first()
                    .and_then(|id| graph.get_node(id))
                    .map_or_else(|| format!("Community {cid}"), |n| n.label.clone());
                (*cid, label)
            })
            .collect();

        // Write the names back onto the nodes. `graph.json` is the only place a
        // community name survives a reload, so without this step the names are
        // used for this run's exports and then silently lost — the next rebuild
        // would find nothing to carry over and quietly renumber back to
        // heuristics. Only genuinely carried-over names are re-stamped;
        // first-node-label fallbacks are not real names and must not be
        // mistaken for `label` output by a later run.
        for (&cid, nodes) in &communities {
            let Some(name) = carried.get(&cid) else {
                continue;
            };
            for nid in nodes {
                if let Some(node) = graph.get_node_mut(nid) {
                    node.extra.insert(
                        graphify_cluster::label::NODE_LABEL_FIELD.to_string(),
                        serde_json::Value::String(name.clone()),
                    );
                }
            }
        }

        (communities, community_labels)
    };
    let cohesion = graphify_cluster::score_all(&graph, &communities);
    info!(
        "rebuild: {} communities ({} names carried over)",
        communities.len(),
        labels_preserved
    );

    info!("rebuild: analyzing...");
    let god_list = graphify_analyze::god_nodes(&graph, 10);
    let surprise_list = graphify_analyze::surprising_connections(&graph, &communities, 5);
    let questions = graphify_analyze::suggest_questions(&graph, &communities, &community_labels, 7);

    std::fs::create_dir_all(output_dir)
        .map_err(|e| WatchError::Rebuild(format!("create output dir: {e}")))?;

    let _ = graphify_export::export_json(&graph, output_dir, Some(&community_labels));
    let _ = graphify_export::export_html(&graph, &communities, &community_labels, output_dir, None);
    let _ = graphify_export::export_graphml(&graph, output_dir);
    let _ = graphify_export::export_cypher(&graph, output_dir);
    let _ = graphify_export::export_svg(&graph, &communities, output_dir);
    let _ = graphify_export::export_wiki(&graph, &communities, &community_labels, output_dir);

    let detection_json = serde_json::json!({
        "total_files": detection.total_files,
        "total_words": detection.total_words,
        "warning": detection.warning,
    });
    let question_json: Vec<serde_json::Value> = questions
        .iter()
        .map(|q| serde_json::to_value(q).unwrap_or_default())
        .collect();
    let token_cost: HashMap<String, usize> =
        HashMap::from([("input".to_string(), 0), ("output".to_string(), 0)]);

    let root_str = root.to_string_lossy();
    if let Ok(report) = graphify_export::generate_report(&graphify_export::ReportInput {
        graph: &graph,
        communities: &communities,
        cohesion_scores: &cohesion,
        community_labels: &community_labels,
        god_nodes: &god_list,
        surprises: &surprise_list,
        detection_result: &detection_json,
        token_cost: &token_cost,
        root: &root_str,
        suggested_questions: Some(&question_json),
    }) {
        let report_path = output_dir.join("GRAPH_REPORT.md");
        let _ = std::fs::write(&report_path, &report);
    }

    info!("rebuild: done");
    Ok(RebuildOutcome {
        nodes: graph.node_count(),
        edges: graph.edge_count(),
        communities: communities.len(),
        cache_hits,
        extracted: code_files.len() - cache_hits,
        errors,
        labels_preserved,
    })
}

/// Watch `root` for file changes and trigger rebuilds into `output_dir`.
///
/// This is an async loop that runs until cancelled. On each batch of
/// debounced file changes, it logs the changed paths and invokes an
/// incremental rebuild (only changed files have their cache invalidated).
///
/// # Arguments
/// * `root` - Directory to watch recursively.
/// * `output_dir` - Where to write rebuild output.
pub async fn watch_directory(root: &Path, output_dir: &Path) -> Result<(), WatchError> {
    let (tx, mut rx) = mpsc::channel::<Vec<PathBuf>>(100);

    let mut debouncer = new_debouncer(
        DEBOUNCE_DURATION,
        move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| match res {
            Ok(events) => {
                let paths: Vec<PathBuf> = events.into_iter().map(|e| e.path).collect();
                if let Err(e) = tx.blocking_send(paths) {
                    warn!("Failed to send watch events: {}", e);
                }
            }
            Err(e) => {
                warn!("Watch error: {}", e);
            }
        },
    )
    .map_err(|e| WatchError::Setup(e.to_string()))?;

    debouncer.watcher().watch(root, RecursiveMode::Recursive)?;

    info!(
        "Watching {} for changes (output: {})",
        root.display(),
        output_dir.display()
    );
    println!("Watching {} for changes...", root.display());

    println!("Running initial build...");
    let root_clone = root.to_path_buf();
    let out_clone = output_dir.to_path_buf();
    // The watcher's own rebuilds force past the shrink guard: the user is
    // editing, so nodes legitimately come and go, and a watcher that refused to
    // write after a deletion would silently stop tracking the file it watches.
    let watch_opts = RebuildOptions {
        force: true,
        no_cluster: false,
    };
    match tokio::task::spawn_blocking(move || {
        rebuild_code(&root_clone, &out_clone, None, &watch_opts)
    })
    .await
    {
        Ok(Ok(_)) => println!("Initial build complete."),
        Ok(Err(e)) => eprintln!("Initial build failed: {e}"),
        Err(e) => eprintln!("Initial build panicked: {e}"),
    }

    while let Some(changed_paths) = rx.recv().await {
        let relevant = filter_changes(&changed_paths);

        if relevant.is_empty() {
            debug!("Ignoring changes in excluded paths");
            continue;
        }

        info!("{} file(s) changed, triggering rebuild...", relevant.len());
        println!(
            "Files changed ({}), triggering incremental rebuild...",
            relevant.len()
        );

        for p in &relevant {
            debug!("  changed: {}", p.display());
        }

        let root_clone = root.to_path_buf();
        let out_clone = output_dir.to_path_buf();
        match tokio::task::spawn_blocking(move || {
            rebuild_code(&root_clone, &out_clone, Some(&relevant), &watch_opts)
        })
        .await
        {
            Ok(Ok(out)) => println!(
                "Rebuild complete: {} nodes, {} edges, {} communities.",
                out.nodes, out.edges, out.communities
            ),
            Ok(Err(e)) => eprintln!("Rebuild failed: {e}"),
            Err(e) => eprintln!("Rebuild panicked: {e}"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_should_ignore_git() {
        assert!(should_ignore(Path::new("/repo/.git/objects/abc")));
        assert!(should_ignore(Path::new("/repo/node_modules/foo.js")));
        assert!(should_ignore(Path::new("/repo/__pycache__/mod.pyc")));
        assert!(should_ignore(Path::new("/repo/target/debug/build")));
        assert!(should_ignore(Path::new("/repo/graphify-rs-out/graph.json")));
    }

    /// A graph.json with `n` nodes, optionally carrying community + name.
    fn graph_file(dir: &Path, nodes: &[(&str, Option<usize>, Option<&str>)]) -> PathBuf {
        let nodes: Vec<serde_json::Value> = nodes
            .iter()
            .map(|(id, cid, name)| {
                let mut o = serde_json::json!({"id": id});
                if let Some(c) = cid {
                    o["community"] = serde_json::json!(c);
                }
                if let Some(n) = name {
                    o[graphify_cluster::label::NODE_LABEL_FIELD] = serde_json::json!(n);
                }
                o
            })
            .collect();
        let path = dir.join("graph.json");
        std::fs::write(&path, serde_json::json!({"nodes": nodes}).to_string()).unwrap();
        path
    }

    #[test]
    fn shrink_guard_allows_growth_and_equality() {
        let tmp = tempfile::tempdir().unwrap();
        let p = graph_file(tmp.path(), &[("a", None, None), ("b", None, None)]);
        assert!(check_not_shrinking(&p, 5).is_ok(), "growth is fine");
        assert!(check_not_shrinking(&p, 2).is_ok(), "no change is fine");
    }

    #[test]
    fn shrink_guard_refuses_any_net_node_loss() {
        let tmp = tempfile::tempdir().unwrap();
        let p = graph_file(tmp.path(), &[("a", None, None), ("b", None, None)]);

        let err = check_not_shrinking(&p, 1).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("net -1"), "got: {msg}");
        assert!(msg.contains("--force"), "must say how to override: {msg}");
    }

    #[test]
    fn shrink_guard_fails_closed_on_an_unreadable_graph() {
        // The dangerous case: if we cannot parse the old graph we cannot prove
        // the new one is not a shrink, so refuse rather than overwrite.
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("graph.json");
        std::fs::write(&p, "{ truncated mid-write").unwrap();

        assert!(check_not_shrinking(&p, 10_000).is_err());
    }

    #[test]
    fn shrink_guard_permits_a_missing_or_empty_graph() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(check_not_shrinking(&tmp.path().join("absent.json"), 0).is_ok());

        let empty = tmp.path().join("graph.json");
        std::fs::write(&empty, "   \n").unwrap();
        assert!(check_not_shrinking(&empty, 1).is_ok());
    }

    #[test]
    fn previous_communities_and_labels_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let p = graph_file(
            tmp.path(),
            &[
                ("a", Some(3), Some("Auth")),
                ("b", Some(3), Some("Auth")),
                ("c", Some(7), Some("Export")),
                ("d", None, None),
            ],
        );

        let comms = previous_node_communities(&p);
        assert_eq!(comms.get("a"), Some(&3));
        assert_eq!(comms.get("c"), Some(&7));
        assert!(!comms.contains_key("d"), "unassigned nodes are skipped");

        let labels = previous_community_labels(&p);
        assert_eq!(labels.get(&3).map(String::as_str), Some("Auth"));
        assert_eq!(labels.get(&7).map(String::as_str), Some("Export"));
    }

    #[test]
    fn previous_state_is_empty_when_there_is_no_readable_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope.json");
        assert!(previous_node_communities(&missing).is_empty());
        assert!(previous_community_labels(&missing).is_empty());

        let corrupt = tmp.path().join("graph.json");
        std::fs::write(&corrupt, "not json").unwrap();
        assert!(previous_node_communities(&corrupt).is_empty());
        assert!(previous_community_labels(&corrupt).is_empty());
    }

    #[test]
    fn test_should_not_ignore_source() {
        assert!(!should_ignore(Path::new("/repo/src/main.rs")));
        assert!(!should_ignore(Path::new("/repo/lib/utils.py")));
        assert!(!should_ignore(Path::new("/repo/README.md")));
    }

    #[test]
    fn test_filter_changes() {
        let paths = vec![
            PathBuf::from("/repo/src/main.rs"),
            PathBuf::from("/repo/.git/HEAD"),
            PathBuf::from("/repo/src/lib.rs"),
            PathBuf::from("/repo/node_modules/foo/index.js"),
        ];
        let filtered = filter_changes(&paths);
        assert_eq!(filtered.len(), 2);
        assert!(filtered.contains(&PathBuf::from("/repo/src/main.rs")));
        assert!(filtered.contains(&PathBuf::from("/repo/src/lib.rs")));
    }

    #[test]
    fn test_filter_changes_all_ignored() {
        let paths = vec![
            PathBuf::from("/repo/.git/HEAD"),
            PathBuf::from("/repo/.DS_Store"),
        ];
        let filtered = filter_changes(&paths);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_filter_changes_empty() {
        let filtered = filter_changes(&[]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn test_rebuild_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let result = rebuild_code(
            dir.path(),
            output.path(),
            None,
            &RebuildOptions::default(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_rebuild_with_code_files() {
        let dir = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("main.rs"),
            "fn main() { hello(); }\nfn hello() { println!(\"hi\"); }\n",
        )
        .unwrap();
        std::fs::write(
            src.join("lib.rs"),
            "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
        )
        .unwrap();

        let outcome = rebuild_code(
            dir.path(),
            output.path(),
            None,
            &RebuildOptions::default(),
        )
        .unwrap();
        assert!(outcome.nodes > 0);

        assert!(output.path().join("graph.json").exists());
        assert!(output.path().join("graph.html").exists());
        assert!(output.path().join("GRAPH_REPORT.md").exists());

        // A rebuild must write community membership onto the nodes. Without the
        // stamping step graph.json parses fine but every node is unassigned,
        // which silently breaks everything that groups by community.
        let raw = std::fs::read_to_string(output.path().join("graph.json")).unwrap();
        let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let assigned = doc["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n.get("community").is_some_and(|c| !c.is_null()))
            .count();
        assert!(assigned > 0, "no node carried a community in {raw}");
    }

    #[test]
    fn a_rebuild_keeps_community_names_written_by_label() {
        let dir = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("main.rs"),
            "fn main() { hello(); }\nfn hello() { println!(\"hi\"); }\n",
        )
        .unwrap();

        rebuild_code(
            dir.path(),
            output.path(),
            None,
            &RebuildOptions::default(),
        )
        .unwrap();

        // Stand in for `label`: name every community in the graph on disk.
        let path = output.path().join("graph.json");
        let mut doc: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for node in doc["nodes"].as_array_mut().unwrap() {
            if node.get("community").is_some_and(|c| !c.is_null()) {
                node[graphify_cluster::label::NODE_LABEL_FIELD] =
                    serde_json::json!("Hand Written Name");
            }
        }
        std::fs::write(&path, doc.to_string()).unwrap();

        let outcome = rebuild_code(
            dir.path(),
            output.path(),
            None,
            &RebuildOptions::default(),
        )
        .unwrap();

        assert!(
            outcome.labels_preserved > 0,
            "rebuild discarded every community name"
        );

        // The names must also be written back to disk, not just used for this
        // run's exports. A rebuild that carries names in memory but drops them
        // from graph.json looks correct once and loses everything on the run
        // after — so assert the third rebuild still finds them.
        let reloaded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let named = reloaded["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|n| n.get(graphify_cluster::label::NODE_LABEL_FIELD).is_some())
            .count();
        assert!(named > 0, "names were not persisted back into graph.json");

        let third = rebuild_code(
            dir.path(),
            output.path(),
            None,
            &RebuildOptions::default(),
        )
        .unwrap();
        assert!(
            third.labels_preserved > 0,
            "names survived one rebuild but not the next"
        );
    }

    #[test]
    fn a_rebuild_that_would_shrink_the_graph_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("main.rs"),
            "fn main() { hello(); }\nfn hello() { println!(\"hi\"); }\n",
        )
        .unwrap();
        std::fs::write(src.join("extra.rs"), "pub fn a() {}\npub fn b() {}\n").unwrap();

        rebuild_code(
            dir.path(),
            output.path(),
            None,
            &RebuildOptions::default(),
        )
        .unwrap();
        let before = std::fs::read_to_string(output.path().join("graph.json")).unwrap();

        // Delete half the corpus, as a broken extraction would effectively do.
        std::fs::remove_file(src.join("extra.rs")).unwrap();

        let err = rebuild_code(
            dir.path(),
            output.path(),
            None,
            &RebuildOptions::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"));
        assert_eq!(
            std::fs::read_to_string(output.path().join("graph.json")).unwrap(),
            before,
            "the rejected rebuild must not have touched graph.json"
        );

        // --force is the escape hatch for a real deletion.
        let forced = rebuild_code(
            dir.path(),
            output.path(),
            None,
            &RebuildOptions {
                force: true,
                no_cluster: false,
            },
        );
        assert!(forced.is_ok(), "--force must permit a genuine shrink");
    }

    #[test]
    fn test_incremental_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let output = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("main.rs"),
            "fn main() { hello(); }\nfn hello() { println!(\"hi\"); }\n",
        )
        .unwrap();

        let result = rebuild_code(
            dir.path(),
            output.path(),
            None,
            &RebuildOptions::default(),
        );
        assert!(result.is_ok());

        let changed = vec![src.join("main.rs")];
        let result = rebuild_code(
            dir.path(),
            output.path(),
            Some(&changed),
            &RebuildOptions::default(),
        );
        assert!(result.is_ok());
    }
}
