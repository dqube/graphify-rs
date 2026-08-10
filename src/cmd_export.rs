//! `export` and `tree`: re-render an existing graph without rebuilding it.
//!
//! `build --format` produces these same files, but only as part of a full
//! pipeline run. Re-rendering is cheap and re-extraction is not, so anything
//! that only changes presentation — a different diagram scale, a language, a
//! node cap — belongs here rather than in a rebuild.
//!
//! Everything reads `graph.json` and writes beside it. Community membership and
//! the names written by `label` are recovered from the loaded graph, so an
//! export reflects the last `label` run without needing the sidecar.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use colored::Colorize;

use graphify_core::graph::KnowledgeGraph;

/// Formats `export` accepts. `json` and `report` are absent deliberately —
/// they are build outputs, not renderings, and `update` is the way to refresh
/// them.
pub const FORMATS: &[&str] = &[
    "html",
    "split-html",
    "callflow-html",
    "tree",
    "obsidian",
    "wiki",
    "svg",
    "graphml",
    "cypher",
    "rdf",
    "falkordb",
    "neo4j",
];

/// Reject a `graph.json` larger than this before parsing it, matching
/// [`crate::cmd_merge`].
const MAX_GRAPH_BYTES: u64 = 50 * 1024 * 1024;

#[derive(clap::Args, Debug)]
pub struct ExportArgs {
    /// Format to render
    #[arg(value_parser = clap::builder::PossibleValuesParser::new(FORMATS))]
    pub format: String,

    /// Path to graph.json (default: graphify-rs-out/graph.json)
    #[arg(long)]
    pub graph: Option<String>,

    /// Output directory, or an .html file for callflow-html and tree
    #[arg(short, long)]
    pub output: Option<String>,

    /// Maximum nodes in the interactive visualization (html only)
    #[arg(long)]
    pub max_viz_nodes: Option<usize>,

    /// Maximum documented sections (callflow-html only)
    #[arg(long)]
    pub max_sections: Option<usize>,

    /// Mermaid font/spacing multiplier (callflow-html only)
    #[arg(long)]
    pub diagram_scale: Option<f64>,

    /// Maximum nodes drawn per section diagram (callflow-html only)
    #[arg(long)]
    pub max_diagram_nodes: Option<usize>,

    /// Maximum edges drawn per section diagram (callflow-html only)
    #[arg(long)]
    pub max_diagram_edges: Option<usize>,

    /// Generated-copy language: auto, en, or a zh locale (callflow-html only)
    #[arg(long)]
    pub lang: Option<String>,

    /// Project name shown in the page header (callflow-html only)
    #[arg(long)]
    pub label: Option<String>,

    /// Neo4j endpoint to push to, overriding [neo4j].uri (neo4j only)
    #[arg(long)]
    pub push: Option<String>,
}

#[derive(clap::Args, Debug)]
pub struct TreeArgs {
    /// Path to graph.json (default: graphify-rs-out/graph.json)
    #[arg(long)]
    pub graph: Option<String>,

    /// Output .html path (default: <graph dir>/tree.html)
    #[arg(short, long)]
    pub output: Option<String>,
}

/// Render `tree`, which is `export tree` under its own name for parity with
/// the Python CLI.
pub fn cmd_tree(args: &TreeArgs) -> Result<()> {
    let export = ExportArgs {
        format: "tree".into(),
        graph: args.graph.clone(),
        output: args.output.clone(),
        max_viz_nodes: None,
        max_sections: None,
        diagram_scale: None,
        max_diagram_nodes: None,
        max_diagram_edges: None,
        lang: None,
        label: None,
        push: None,
    };
    // `tree` renders locally and never pushes, so the async path is unreachable.
    futures_lite_block_on(cmd_export(&export))
}

/// Drive a future to completion on the current thread.
///
/// `tree` is synchronous work reached from a synchronous call site; borrowing
/// the calling runtime would panic, so it gets its own single-purpose one.
fn futures_lite_block_on(fut: impl std::future::Future<Output = Result<()>>) -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("starting a runtime for tree export")?
        .block_on(fut)
}

pub async fn cmd_export(args: &ExportArgs) -> Result<()> {
    let graph_path = args
        .graph
        .clone()
        .map_or_else(default_graph_path, PathBuf::from);
    warn_about_inapplicable_flags(args);

    let (graph, built_at_commit) = load_graph(&graph_path)?;
    let communities = communities_of(&graph);
    let labels = graphify_cluster::label::labels_from_graph(&graph)
        .into_iter()
        .collect::<HashMap<usize, String>>();

    // Everything lands beside the graph unless told otherwise.
    let graph_dir = graph_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    println!("\n{} {}", "graphify-rs".cyan().bold(), "export".dimmed());
    println!("  {} {}", "graph".dimmed(), graph_path.display());
    println!(
        "  {} {} nodes, {} edges, {} communities",
        "loaded".dimmed(),
        graph.node_count(),
        graph.edge_count(),
        communities.len()
    );

    let format = args.format.as_str();

    // neo4j is a sink rather than a file, so it returns before the path report.
    if format == "neo4j" {
        return push_neo4j(&graph, args.push.as_deref()).await;
    }

    let written = match format {
        "html" => {
            let dir = out_dir(args, &graph_dir);
            graphify_export::export_html(&graph, &communities, &labels, &dir, args.max_viz_nodes)?
        }
        "split-html" => {
            let dir = out_dir(args, &graph_dir);
            graphify_export::export_html_split(&graph, &communities, &labels, &dir)?
        }
        "callflow-html" => {
            let mut options = callflow_options(args, &graph_path);
            // Prefer the commit stamped into graph.json — that is the commit
            // the graph describes; HEAD may have moved since the build.
            options.built_at_commit = built_at_commit
                .or_else(|| {
                    graph_dir
                        .parent()
                        .and_then(graphify_export::git_short_commit)
                })
                .unwrap_or_default();
            write_single_file(args, &graph_dir, "callflow.html", |dir| {
                graphify_export::export_callflow_html(&graph, &labels, dir, &options)
            })?
        }
        "tree" => write_single_file(args, &graph_dir, "tree.html", |dir| {
            graphify_export::export_tree_html(&graph, &communities, &labels, dir)
        })?,
        "obsidian" => {
            let dir = out_dir(args, &graph_dir);
            graphify_export::export_obsidian(&graph, &communities, &labels, &dir)?
        }
        "wiki" => {
            let dir = out_dir(args, &graph_dir);
            graphify_export::export_wiki(&graph, &communities, &labels, &dir)?
        }
        "svg" => {
            let dir = out_dir(args, &graph_dir);
            graphify_export::export_svg(&graph, &communities, &dir)?
        }
        "graphml" => graphify_export::export_graphml(&graph, &out_dir(args, &graph_dir))?,
        "cypher" => graphify_export::export_cypher(&graph, &out_dir(args, &graph_dir))?,
        "rdf" => graphify_export::export_rdf(&graph, &out_dir(args, &graph_dir))?,
        "falkordb" => graphify_export::export_falkordb(&graph, &out_dir(args, &graph_dir))?,
        other => bail!("unknown export format: {other}"),
    };

    println!("\n  {} {}", "✓".green(), written.display());
    println!();
    Ok(())
}

/// Where multi-file and directory formats write.
fn out_dir(args: &ExportArgs, graph_dir: &Path) -> PathBuf {
    args.output
        .as_ref()
        .map_or_else(|| graph_dir.to_path_buf(), PathBuf::from)
}

/// Run an exporter that writes one fixed filename, honouring an `--output`
/// that names a different file.
///
/// The exporters choose their own filename, so redirecting means rendering into
/// a scratch directory and moving the result. Rendering straight into the
/// destination directory would silently overwrite an existing file of the
/// exporter's default name — a `--output notes.html` must not clobber the
/// `callflow.html` sitting next to it.
fn write_single_file(
    args: &ExportArgs,
    graph_dir: &Path,
    default_name: &str,
    render: impl FnOnce(&Path) -> anyhow::Result<PathBuf>,
) -> Result<PathBuf> {
    let Some(requested) = args.output.as_ref() else {
        return render(graph_dir);
    };
    let dest = PathBuf::from(requested);

    // A directory (existing, or trailing-slash) keeps the default filename.
    let is_dir_target = dest.is_dir()
        || requested.ends_with('/')
        || dest.extension().is_none_or(|e| e.eq_ignore_ascii_case(""));
    if is_dir_target {
        std::fs::create_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
        return render(&dest);
    }

    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    std::fs::create_dir_all(&parent).with_context(|| format!("creating {}", parent.display()))?;

    if dest.file_name().is_some_and(|n| n == default_name) {
        return render(&parent);
    }

    let scratch = parent.join(format!(".graphify_export_{}", std::process::id()));
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
    let result = render(&scratch).and_then(|produced| {
        std::fs::rename(&produced, &dest)
            .with_context(|| format!("moving {} to {}", produced.display(), dest.display()))?;
        Ok(dest.clone())
    });
    // Clean up whether or not the render succeeded, so a failure does not
    // strand a scratch directory beside the user's output.
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

fn callflow_options(args: &ExportArgs, graph_path: &Path) -> graphify_export::CallflowOptions {
    let mut options = graphify_export::CallflowOptions::default();
    if let Some(v) = args.max_sections {
        options.max_sections = v;
    }
    if let Some(v) = args.diagram_scale {
        options.diagram_scale = v;
    }
    if let Some(v) = args.max_diagram_nodes {
        options.max_diagram_nodes = v;
    }
    if let Some(v) = args.max_diagram_edges {
        options.max_diagram_edges = v;
    }
    if let Some(v) = &args.lang {
        options.lang = v.clone();
    }
    options.project_name = args.label.clone().unwrap_or_else(|| {
        // The directory above graphify-rs-out is the project, matching how a
        // full build names the page.
        graph_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    options
}

async fn push_neo4j(graph: &KnowledgeGraph, push_uri: Option<&str>) -> Result<()> {
    let mut conn = crate::config::load_config(Path::new("."))
        .neo4j
        .unwrap_or_default()
        .resolve()
        .context(
            "neo4j push requires credentials: set [neo4j] in graphify-rs.toml \
             or NEO4J_USER / NEO4J_PASSWORD",
        )?;
    if let Some(uri) = push_uri {
        conn.uri = uri.to_string();
    }

    println!("  {} {}", "pushing to".dimmed(), conn.uri);
    let stats = graphify_export::push_to_neo4j(graph, &conn)
        .await
        .context("pushing to Neo4j")?;
    println!(
        "\n  {} {} nodes, {} edges pushed",
        "✓".green(),
        stats.nodes,
        stats.edges
    );
    println!();
    Ok(())
}

/// Point out flags that the chosen format will ignore.
///
/// Silently dropping them is the worse failure: a mistyped format leaves the
/// user staring at an unchanged diagram wondering why `--diagram-scale` did
/// nothing. A warning rather than an error keeps existing scripts working.
fn warn_about_inapplicable_flags(args: &ExportArgs) {
    let format = args.format.as_str();
    let mut ignored: Vec<&str> = Vec::new();

    if args.max_viz_nodes.is_some() && format != "html" {
        ignored.push("--max-viz-nodes");
    }
    let callflow_only = [
        (args.max_sections.is_some(), "--max-sections"),
        (args.diagram_scale.is_some(), "--diagram-scale"),
        (args.max_diagram_nodes.is_some(), "--max-diagram-nodes"),
        (args.max_diagram_edges.is_some(), "--max-diagram-edges"),
        (args.lang.is_some(), "--lang"),
        (args.label.is_some(), "--label"),
    ];
    if format != "callflow-html" {
        ignored.extend(
            callflow_only
                .iter()
                .filter(|(set, _)| *set)
                .map(|(_, n)| *n),
        );
    }
    if args.push.is_some() && format != "neo4j" {
        ignored.push("--push");
    }

    if !ignored.is_empty() {
        eprintln!(
            "warning: {} {} no effect on `{format}`",
            ignored.join(", "),
            if ignored.len() == 1 { "has" } else { "have" }
        );
    }
}

fn default_graph_path() -> PathBuf {
    crate::paths::resolve_default_output(Path::new(".")).join("graph.json")
}

/// Load a graph, refusing an oversized file before parsing it.
///
/// Also returns the `built_at_commit` stamped into the file, when present —
/// the JSON is already in hand, so capturing it here costs nothing.
fn load_graph(path: &Path) -> Result<(KnowledgeGraph, Option<String>)> {
    if !path.exists() {
        bail!(
            "graph not found at {} — run `graphify-rs build` first",
            path.display()
        );
    }
    let size = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?
        .len();
    if size > MAX_GRAPH_BYTES {
        bail!(
            "{} is {size} bytes, exceeding the {MAX_GRAPH_BYTES}-byte cap",
            path.display()
        );
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let commit = value
        .get("built_at_commit")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let graph = KnowledgeGraph::from_node_link_json(&value)
        .with_context(|| format!("loading graph from {}", path.display()))?;
    Ok((graph, commit))
}

/// Rebuild the `{cid: [node ids]}` index the exporters take.
fn communities_of(graph: &KnowledgeGraph) -> HashMap<usize, Vec<String>> {
    graph
        .communities
        .iter()
        .map(|c| (c.id, c.nodes.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(format: &str, graph: &Path, output: Option<&Path>) -> ExportArgs {
        ExportArgs {
            format: format.into(),
            graph: Some(graph.to_string_lossy().into_owned()),
            output: output.map(|p| p.to_string_lossy().into_owned()),
            max_viz_nodes: None,
            max_sections: None,
            diagram_scale: None,
            max_diagram_nodes: None,
            max_diagram_edges: None,
            lang: None,
            label: None,
            push: None,
        }
    }

    /// A graph.json with two named communities, as `build` + `label` leave it.
    fn graph_json(dir: &Path) -> PathBuf {
        let doc = serde_json::json!({
            "directed": false,
            "multigraph": false,
            "graph": {},
            "nodes": [
                {"id":"a","label":"Alpha","source_file":"src/a.rs","node_type":"function",
                 "community":0,"community_name":"Core"},
                {"id":"b","label":"Beta","source_file":"src/a.rs","node_type":"function",
                 "community":0,"community_name":"Core"},
                {"id":"c","label":"Gamma","source_file":"src/c.rs","node_type":"function",
                 "community":1,"community_name":"Edge"}
            ],
            "links": [
                {"source":"a","target":"b","relation":"calls","confidence":"EXTRACTED",
                 "confidence_score":1.0,"source_file":"src/a.rs"},
                {"source":"b","target":"c","relation":"calls","confidence":"EXTRACTED",
                 "confidence_score":1.0,"source_file":"src/a.rs"}
            ]
        });
        let path = dir.join("graph.json");
        std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        path
    }

    fn run(a: &ExportArgs) -> Result<()> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cmd_export(a))
    }

    #[test]
    fn every_advertised_format_renders() {
        // A format listed in --help but unreachable in the match is a promise
        // the CLI does not keep; neo4j is the one that needs a live server.
        for format in FORMATS.iter().filter(|f| **f != "neo4j") {
            let tmp = tempfile::tempdir().unwrap();
            let graph = graph_json(tmp.path());
            run(&args(format, &graph, None))
                .unwrap_or_else(|e| panic!("format {format} failed: {e:#}"));
        }
    }

    #[test]
    fn community_names_survive_into_the_render() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = graph_json(tmp.path());

        run(&args("wiki", &graph, None)).unwrap();

        // The names came from graph.json, not from a fresh heuristic pass.
        let mut found = false;
        for entry in walk(&tmp.path().join("wiki")) {
            if std::fs::read_to_string(&entry)
                .unwrap_or_default()
                .contains("Core")
            {
                found = true;
                break;
            }
        }
        assert!(found, "community name 'Core' did not reach the wiki output");
    }

    fn walk(dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(p);
            }
        }
        out
    }

    #[test]
    fn output_defaults_to_the_graph_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = graph_json(tmp.path());
        run(&args("tree", &graph, None)).unwrap();
        assert!(tmp.path().join("tree.html").is_file());
    }

    #[test]
    fn a_named_output_file_is_honoured() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = graph_json(tmp.path());
        let dest = tmp.path().join("nested/custom.html");

        run(&args("tree", &graph, Some(&dest))).unwrap();

        assert!(dest.is_file(), "custom filename was not produced");
        assert!(
            !tmp.path().join("nested/tree.html").exists(),
            "the exporter's default name leaked into the output directory"
        );
    }

    #[test]
    fn redirecting_output_does_not_clobber_the_default_name() {
        // The failure this guards: rendering into the destination directory
        // first would overwrite an unrelated tree.html sitting beside it.
        let tmp = tempfile::tempdir().unwrap();
        let graph = graph_json(tmp.path());
        let bystander = tmp.path().join("tree.html");
        std::fs::write(&bystander, "PRE-EXISTING").unwrap();

        run(&args("tree", &graph, Some(&tmp.path().join("other.html")))).unwrap();

        assert_eq!(
            std::fs::read_to_string(&bystander).unwrap(),
            "PRE-EXISTING",
            "an unrelated tree.html was overwritten"
        );
        assert!(tmp.path().join("other.html").is_file());
    }

    #[test]
    fn no_scratch_directory_is_left_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = graph_json(tmp.path());
        run(&args("tree", &graph, Some(&tmp.path().join("x.html")))).unwrap();

        let leftovers: Vec<PathBuf> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with(".graphify_export_"))
            })
            .collect();
        assert!(leftovers.is_empty(), "left behind {leftovers:?}");
    }

    #[test]
    fn an_output_directory_keeps_the_default_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = graph_json(tmp.path());
        let dir = tmp.path().join("rendered");
        std::fs::create_dir_all(&dir).unwrap();

        run(&args("tree", &graph, Some(&dir))).unwrap();

        assert!(dir.join("tree.html").is_file());
    }

    #[test]
    fn callflow_flags_reach_the_renderer() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = graph_json(tmp.path());
        let mut a = args("callflow-html", &graph, None);
        a.max_sections = Some(3);
        a.diagram_scale = Some(1.5);
        a.lang = Some("en".into());
        a.label = Some("MyProject".into());

        let options = callflow_options(&a, &graph);
        assert_eq!(options.max_sections, 3);
        assert!((options.diagram_scale - 1.5).abs() < f64::EPSILON);
        assert_eq!(options.lang, "en");
        assert_eq!(options.project_name, "MyProject");

        run(&a).unwrap();
        let html = std::fs::read_to_string(tmp.path().join("callflow.html")).unwrap();
        assert!(html.contains("MyProject"), "project label did not render");
    }

    #[test]
    fn callflow_defaults_are_untouched_when_no_flags_are_given() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = graph_json(tmp.path());
        let options = callflow_options(&args("callflow-html", &graph, None), &graph);
        let default = graphify_export::CallflowOptions::default();
        assert_eq!(options.max_sections, default.max_sections);
        assert_eq!(options.max_diagram_nodes, default.max_diagram_nodes);
        assert_eq!(options.lang, default.lang);
    }

    #[test]
    fn a_missing_graph_is_reported_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(&args("html", &tmp.path().join("absent.json"), None)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("graph not found"), "got: {msg}");
        assert!(msg.contains("build"), "should say how to fix it: {msg}");
    }

    #[test]
    fn a_corrupt_graph_is_reported_rather_than_rendered_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("graph.json");
        std::fs::write(&path, "{ truncated").unwrap();
        assert!(run(&args("html", &path, None)).is_err());
    }

    #[test]
    fn tree_command_matches_the_tree_format() {
        let tmp = tempfile::tempdir().unwrap();
        let graph = graph_json(tmp.path());
        cmd_tree(&TreeArgs {
            graph: Some(graph.to_string_lossy().into_owned()),
            output: None,
        })
        .unwrap();
        assert!(tmp.path().join("tree.html").is_file());
    }
}
